//! Replay: durable frames in LSN order, for the shipper and the recent-span index rebuild.

use std::sync::Arc;

use crucible_env::{DiskError, OpenMode};
use crucible_records::Lsn;
use crucible_wal_codec::SEGMENT_HEADER_LEN;

use crate::error::ReplayError;
use crate::layout::segment_path;
use crate::scan::{SegmentReader, Step, WalFrame};
use crate::wal::Shared;

/// A cursor over the durable log. It never yields a frame above the durable LSN, nor a frame that a later
/// segment superseded. At the end of the durable log [`Replay::next`] returns `None`; calling it again
/// later continues with frames that became durable in between.
#[derive(Debug)]
pub struct Replay {
    shared: Arc<Shared>,
    next_lsn: u64,
    reader: Option<SegmentReader>,
    segment_no: u64,
}

impl Replay {
    pub(crate) fn new(shared: Arc<Shared>, from: Lsn) -> Self {
        Self {
            shared,
            next_lsn: from.get().max(1),
            reader: None,
            segment_no: 0,
        }
    }

    /// The LSN of the frame the next call yields.
    pub fn position(&self) -> Lsn {
        Lsn::new(self.next_lsn)
    }

    /// Resolves once the frame at [`Replay::position`] is durable, so that [`Replay::next`] returns it, or the error
    /// reading it gives, rather than `None`. The committer wakes the wait after it stores a new durable LSN, and
    /// the wait registers for that wake before it checks the LSN, so a frame that becomes durable between a `next`
    /// that returned `None` and this wait ends it at once. Cancel-safe.
    pub async fn wait_durable(&self) {
        self.shared.durable_reaches(self.next_lsn).await;
    }

    async fn open_segment(&mut self) -> Result<(), ReplayError> {
        let segments = self.shared.segments();
        let lsn = Lsn::new(self.next_lsn);
        let reclaimed = || ReplayError::Reclaimed {
            lsn,
            oldest: segments.first().map_or(Lsn::FIRST, |s| s.first_lsn),
        };
        // Of several segments with one first LSN, the last holds the frames; the others were abandoned.
        let meta = segments
            .iter()
            .rfind(|s| s.first_lsn <= lsn)
            .ok_or_else(reclaimed)?;
        let path = segment_path(meta.segment_no)?;
        let file = match self.shared.disk.open(&path, OpenMode::Read).await {
            Ok(file) => file,
            Err(DiskError::NotFound) => return Err(reclaimed()),
            Err(e) => return Err(e.into()),
        };
        self.segment_no = meta.segment_no;
        self.reader = Some(SegmentReader::new(
            file,
            meta.segment_no,
            SEGMENT_HEADER_LEN as u64,
        ));
        Ok(())
    }

    /// The next durable frame, or `None` at the end of the durable log.
    pub async fn next(&mut self) -> Result<Option<WalFrame>, ReplayError> {
        // Read before the segment list: a segment is registered before the durable LSN advances into it,
        // so a frame at or below this LSN belongs to its segment unless a registered successor claims it.
        let durable = self.shared.durable_lsn().get();
        if self.next_lsn > durable {
            return Ok(None);
        }
        let mut reread = false;
        loop {
            if self.reader.is_none() {
                self.open_segment().await?;
            }
            // The cursor leaves its segment when the next frame belongs to a successor, and also when
            // garbage collection removed the segment: the file may still be readable, but nothing says
            // any more which of its frames a successor superseded.
            let leave = match self.shared.successor_first_lsn(self.segment_no) {
                Some(successor) => successor.is_some_and(|first| first.get() <= self.next_lsn),
                None => true,
            };
            let Some(reader) = self.reader.as_mut().filter(|_| !leave) else {
                self.reader = None;
                continue;
            };
            match reader.step().await? {
                Step::Frame(frame) if frame.lsn().get() < self.next_lsn => {}
                Step::Frame(frame) if frame.lsn().get() == self.next_lsn => {
                    self.next_lsn += 1;
                    return Ok(Some(frame));
                }
                // The frame is durable, so it is in this segment. The window may predate it: read once
                // more before reporting the frame unreadable.
                Step::End(_) if !reread => {
                    reader.invalidate();
                    reread = true;
                }
                Step::Frame(_) | Step::End(_) => {
                    let offset = reader.offset();
                    self.reader = None;
                    return Err(ReplayError::Unreadable {
                        segment_no: self.segment_no,
                        offset,
                        lsn: Lsn::new(self.next_lsn),
                    });
                }
            }
        }
    }
}
