//! The committer task (contract section 4.5): one per WAL. It assigns LSNs, stamps `received_at`, writes
//! each group with one `append` and one `sync_data`, and only then completes the group's waiters.

use std::sync::Arc;

use bytes::Bytes;
use crucible_env::{ClockExt, DiskError, File, Instant};
use crucible_records::Lsn;
use crucible_wal_codec::{Checkpoint, PreparedFrame, SEGMENT_HEADER_LEN};
use tokio::sync::{mpsc, oneshot};

use crate::config::{Durability, GROUP_BYTES};
use crate::error::AppendError;
use crate::recovery::create_segment;
use crate::wal::Shared;

/// One submitted frame and its waiter.
#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) frame: PreparedFrame,
    /// The decoded payload when the frame is a `Checkpoint`.
    pub(crate) checkpoint: Option<Checkpoint>,
    pub(crate) done: oneshot::Sender<Result<Lsn, AppendError>>,
}

impl Item {
    fn len(&self) -> u64 {
        self.frame.encoded_len() as u64
    }

    fn finish(self, result: Result<Lsn, AppendError>) {
        // The submitter may have stopped waiting; the frame's fate does not depend on it.
        let _unobserved = self.done.send(result);
    }
}

#[derive(Debug)]
pub(crate) struct Committer {
    shared: Arc<Shared>,
    queue: mpsc::UnboundedReceiver<Item>,
    /// The current segment.
    file: File,
    segment_no: u64,
    /// Where the next frame goes in the current segment.
    offset: u64,
    /// The LSN of the next frame: the last durable LSN plus 1.
    next_lsn: u64,
    /// A frame taken from the queue that did not fit its group or its segment.
    carry: Option<Item>,
    /// In the interval mode: when the appended, unsynced frames are due for their sync.
    sync_due: Option<Instant>,
}

impl Committer {
    pub(crate) fn new(
        shared: Arc<Shared>,
        queue: mpsc::UnboundedReceiver<Item>,
        file: File,
        segment_no: u64,
        offset: u64,
    ) -> Self {
        let next_lsn = shared.durable_lsn().get().saturating_add(1);
        Self {
            shared,
            queue,
            file,
            segment_no,
            offset,
            next_lsn,
            carry: None,
            sync_due: None,
        }
    }

    /// Runs until every [`crate::Wal`] handle is dropped.
    pub(crate) async fn run(mut self) {
        while let Some(first) = self.next_item().await {
            if self.shared.is_failed() {
                first.finish(Err(AppendError::Failed));
            } else {
                self.commit_group(first).await;
            }
        }
        if self.sync_due.is_some() && !self.shared.is_failed() {
            // A clean stop in the interval mode: one last sync, best effort.
            let _best_effort = self.file.sync_data().await;
        }
    }

    async fn next_item(&mut self) -> Option<Item> {
        loop {
            if let Some(item) = self.carry.take() {
                return Some(item);
            }
            let Some(due) = self.sync_due else {
                return self.queue.recv().await;
            };
            match self.shared.clock.timeout_at(due, self.queue.recv()).await {
                Ok(item) => return item,
                Err(_elapsed) => self.timed_sync().await,
            }
        }
    }

    /// The interval mode's timer. Frames acknowledged since the last sync have no durability claim, so a
    /// failure only abandons the segment.
    async fn timed_sync(&mut self) {
        self.sync_due = None;
        if self.sync().await.is_err() {
            self.shared.metrics.group_failures.add(1);
            self.abandon_segment().await;
        }
    }

    async fn sync(&self) -> Result<(), DiskError> {
        let clock = &self.shared.clock;
        let started = clock.now();
        let result = self.file.sync_data().await;
        let micros = clock.now().saturating_duration_since(started).as_micros();
        self.shared
            .metrics
            .sync_duration
            .record(u64::try_from(micros).unwrap_or(u64::MAX));
        result
    }

    fn fits_segment(&self, pending: u64, item: &Item) -> bool {
        self.offset + pending + item.len() <= self.shared.config.segment_bytes
    }

    /// Forms a group that starts with `first`, writes it, syncs it and completes its waiters.
    async fn commit_group(&mut self, first: Item) {
        if !self.fits_segment(0, &first) && self.rotate(true).await.is_err() {
            self.enter_failed(vec![first]);
            return;
        }
        let mut bytes = first.len();
        let mut items = vec![first];
        while let Ok(item) = self.queue.try_recv() {
            if bytes + item.len() > GROUP_BYTES as u64 || !self.fits_segment(bytes, &item) {
                self.carry = Some(item);
                break;
            }
            bytes += item.len();
            items.push(item);
        }

        let clock = &self.shared.clock;
        let mut buf = Vec::with_capacity(usize::try_from(bytes).unwrap_or(0));
        let mut lsn = self.next_lsn;
        for item in &items {
            item.frame
                .write_to(Lsn::new(lsn), clock.wall_now(), &mut buf);
            lsn += 1;
        }
        let first_lsn = self.next_lsn;

        let config = &self.shared.config;
        let started = clock.now();
        let mut written = self.file.append(self.offset, vec![Bytes::from(buf)]).await;
        let micros = clock.now().saturating_duration_since(started).as_micros();
        self.shared
            .metrics
            .append_duration
            .record(u64::try_from(micros).unwrap_or(u64::MAX));
        if written.is_ok() && config.ack_before_sync() {
            for (acked, item) in (first_lsn..).zip(std::mem::take(&mut items)) {
                item.finish(Ok(Lsn::new(acked)));
            }
        }
        if written.is_ok() {
            match config.durability {
                Durability::Group => written = self.sync().await,
                Durability::Interval(period) => {
                    self.sync_due
                        .get_or_insert_with(|| clock.now().saturating_add(period));
                }
                Durability::None => {}
            }
        }
        if written.is_err() {
            // Nothing of this group is acknowledged. The LSNs it used are assigned again: `next_lsn` still
            // is the last durable LSN plus 1, and the next segment starts there, which supersedes whatever
            // the group left in this one.
            // The waiters learn of it once the log has settled: either a new segment is ready, or the WAL
            // is in the failed state.
            self.shared.metrics.group_failures.add(1);
            self.abandon_segment().await;
            for item in items {
                item.finish(Err(AppendError::Unavailable));
            }
            return;
        }

        self.offset += bytes;
        self.next_lsn = lsn;
        let frames = lsn - first_lsn;
        let metrics = &self.shared.metrics;
        metrics.appended_frames.add(frames);
        metrics.appended_bytes.add(bytes);
        // The watermark moves only once the checkpoint's own frame is durable, and before any waiter
        // learns of it.
        for checkpoint in items.iter().filter_map(|item| item.checkpoint) {
            self.shared.raise_watermark(checkpoint, self.segment_no);
        }
        self.shared.set_durable_lsn(lsn - 1);
        for (acked, item) in (first_lsn..).zip(items) {
            item.finish(Ok(Lsn::new(acked)));
        }
    }

    /// After a failed append or sync: leave the segment at its last durable frame and start the next one.
    /// The failed handle is never synced again. If the rotation fails too, the WAL enters `failed`.
    async fn abandon_segment(&mut self) {
        if self.rotate(false).await.is_err() {
            self.enter_failed(Vec::new());
        }
    }

    /// Starts the next segment: sync the old one, create and preallocate the new one, write its header,
    /// `sync_data`, `sync_dir`. Its `first_lsn` is the last durable LSN plus 1.
    async fn rotate(&mut self, sync_old: bool) -> Result<(), DiskError> {
        if sync_old {
            self.sync_due = None;
            self.sync().await?;
        }
        let shared = &self.shared;
        let (file, meta) = create_segment(
            shared.disk.as_ref(),
            shared.clock.as_ref(),
            &shared.config,
            shared.epoch,
            self.segment_no + 1,
            Lsn::new(self.next_lsn),
        )
        .await?;
        shared.push_segment(meta);
        self.file = file;
        self.segment_no = meta.segment_no;
        self.offset = SEGMENT_HEADER_LEN as u64;
        self.sync_due = None;
        shared.refresh_free_bytes().await;
        Ok(())
    }

    fn enter_failed(&mut self, items: Vec<Item>) {
        tracing::error!("wal entered the failed state; ingest is refused until restart");
        self.shared.set_failed();
        for item in items.into_iter().chain(self.carry.take()) {
            item.finish(Err(AppendError::Failed));
        }
    }
}
