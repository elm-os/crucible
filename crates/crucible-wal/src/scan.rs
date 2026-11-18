//! Reads the frames of one segment file through a sliding window, so a scan never holds a whole segment.

use std::borrow::Cow;

use bytes::Bytes;
use crucible_env::{DiskError, File};
use crucible_records::{Lsn, Timestamp};
use crucible_wal_codec::{
    CleanEnd, CompressionError, FRAME_HEADER_LEN, FrameHeader, MAX_PAYLOAD, RecordType, Scan,
    SegmentEnd, TornReason, load_payload, scan_frame,
};

/// Bytes read per window. A frame larger than the window is read with one window of its own size.
const WINDOW: usize = 1 << 20;

/// One valid frame read from the log: both checksums and every header field are valid.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WalFrame {
    header: FrameHeader,
    segment_no: u64,
    offset: u64,
    /// The header and the stored payload, as they are on disk.
    raw: Bytes,
}

impl WalFrame {
    /// The decoded header.
    pub fn header(&self) -> &FrameHeader {
        &self.header
    }

    /// The frame's LSN.
    pub fn lsn(&self) -> Lsn {
        self.header.lsn
    }

    /// The frame's record type.
    pub fn record_type(&self) -> RecordType {
        self.header.record_type
    }

    /// The wall time the committer read when it assigned the LSN.
    pub fn received_at(&self) -> Timestamp {
        self.header.received_at
    }

    /// The segment that holds the frame. With [`WalFrame::offset`] it is the frame's locator.
    pub fn segment_no(&self) -> u64 {
        self.segment_no
    }

    /// Offset of the frame header in its segment.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Offset one past the frame in its segment.
    pub fn end_offset(&self) -> u64 {
        self.offset + self.raw.len() as u64
    }

    /// The payload bytes as stored (compressed when the header says so).
    pub fn stored_payload(&self) -> &[u8] {
        self.raw.get(FRAME_HEADER_LEN..).unwrap_or_default()
    }

    /// The payload: borrowed when stored as it is, decompressed otherwise.
    pub fn payload(&self) -> Result<Cow<'_, [u8]>, CompressionError> {
        load_payload(self.header.compressed, self.stored_payload())
    }

    /// The header and stored payload exactly as read.
    pub(crate) fn raw(&self) -> &Bytes {
        &self.raw
    }
}

/// What a reader found at its position.
#[derive(Debug)]
pub(crate) enum Step {
    Frame(WalFrame),
    /// No valid frame starts at the reader's offset.
    End(SegmentEnd),
}

/// A forward reader over one segment file.
#[derive(Debug)]
pub(crate) struct SegmentReader {
    file: File,
    segment_no: u64,
    /// File offset of the next frame.
    offset: u64,
    window: Bytes,
    window_start: u64,
    /// The window ends at the end of the file.
    window_at_eof: bool,
}

impl SegmentReader {
    pub(crate) fn new(file: File, segment_no: u64, offset: u64) -> Self {
        Self {
            file,
            segment_no,
            offset,
            window: Bytes::new(),
            window_start: offset,
            window_at_eof: false,
        }
    }

    /// File offset of the next frame: the end of the valid data read so far.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    pub(crate) fn file(&self) -> &File {
        &self.file
    }

    pub(crate) fn into_file(self) -> File {
        self.file
    }

    /// Forgets the window, so the next step reads the file again.
    pub(crate) fn invalidate(&mut self) {
        self.window = Bytes::new();
        self.window_start = self.offset;
        self.window_at_eof = false;
    }

    async fn refill(&mut self, len: usize) -> Result<(), DiskError> {
        let window = self.file.read_at(self.offset, len).await?;
        self.window_at_eof = window.len() < len;
        self.window = window;
        self.window_start = self.offset;
        Ok(())
    }

    /// Reads the frame at the current offset, or says why there is none. Offsets in the returned
    /// [`SegmentEnd`] are relative to the window; use [`SegmentReader::offset`] for the file offset.
    pub(crate) async fn step(&mut self) -> Result<Step, DiskError> {
        loop {
            // The window never starts after the offset, and both fit in memory.
            let rel = usize::try_from(self.offset - self.window_start).unwrap_or(usize::MAX);
            let rel = rel.min(self.window.len());
            let available = self.window.len() - rel;
            let end = match scan_frame(&self.window, rel) {
                Scan::Frame(frame) => {
                    let found = WalFrame {
                        header: *frame.header(),
                        segment_no: self.segment_no,
                        offset: self.offset,
                        raw: self.window.slice(frame.offset()..frame.end_offset()),
                    };
                    self.offset = found.end_offset();
                    return Ok(Step::Frame(found));
                }
                Scan::End(end) => end,
            };
            if self.window_at_eof {
                return Ok(Step::End(end));
            }
            // The window stops short of the file's end: decide whether it showed the whole frame.
            let need = match end {
                SegmentEnd::Clean {
                    reason: CleanEnd::ZeroMagic,
                    ..
                } if available >= 4 => return Ok(Step::End(end)),
                SegmentEnd::Torn {
                    reason: TornReason::ShortPayload,
                    ..
                } => {
                    let declared = self
                        .window
                        .get(rel + 8..rel + 12)
                        .and_then(|b| <[u8; 4]>::try_from(b).ok())
                        .map_or(0, u32::from_le_bytes)
                        .min(MAX_PAYLOAD);
                    FRAME_HEADER_LEN + declared as usize
                }
                SegmentEnd::Clean { .. }
                | SegmentEnd::Torn {
                    reason: TornReason::ShortHeader,
                    ..
                } => WINDOW,
                SegmentEnd::Torn { .. } | SegmentEnd::Corrupt { .. } => {
                    return Ok(Step::End(end));
                }
            };
            if available >= need {
                return Ok(Step::End(end));
            }
            self.refill(need.max(WINDOW)).await?;
        }
    }

    /// Whether any byte from the current offset to the end of the file is not zero.
    pub(crate) async fn tail_has_data(&self) -> Result<bool, DiskError> {
        let mut at = self.offset;
        loop {
            let chunk = self.file.read_at(at, WINDOW).await?;
            if chunk.iter().any(|&b| b != 0) {
                return Ok(true);
            }
            if chunk.len() < WINDOW {
                return Ok(false);
            }
            at += chunk.len() as u64;
        }
    }
}
