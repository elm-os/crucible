//! The log handle: open, append, admission signals and garbage collection.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crucible_env::metrics::registry;
use crucible_env::{Clock, Counter, Disk, DiskError, Env, Gauge, Histogram, OpenMode, Root};
use crucible_records::{Lsn, Timestamp, WalEpoch};
use crucible_wal_codec::{
    Checkpoint, PreparedFrame, Record, RecordType, SEGMENT_HEADER_LEN, load_payload,
};
use tokio::sync::{mpsc, oneshot};

use crate::committer::{Committer, Item};
use crate::config::WalConfig;
use crate::error::{AppendError, GcError, RecoveryError};
use crate::layout::{SegmentMeta, segment_path, wal_dir, write_hint};
use crate::recovery::{RecoveryReport, Watermark, recover};
use crate::replay::Replay;
use crate::scan::{SegmentReader, Step, WalFrame};

/// What ingest admission reads before it accepts a request (contract sections 4.7 and 4.8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Admission {
    /// The WAL is in the `failed` state: a group failed and so did the rotation after it. Until restart.
    pub failed: bool,
    /// Total segment bytes are at or above 90% of `wal.max_bytes`.
    pub over_high_water: bool,
    /// Free disk space is below `wal.min_free_bytes`.
    pub below_disk_floor: bool,
    /// Total bytes of the retained segments.
    pub segment_bytes: u64,
    /// Bytes of the segments that hold a frame above the checkpoint watermark.
    pub unshipped_bytes: u64,
    /// Free bytes on the WAL root when the log last looked (at open, rotation and garbage collection).
    pub free_bytes: u64,
    /// The durability mode can lose acknowledged frames; health output must say `durability: unsafe`.
    pub durability_unsafe: bool,
    /// The rate at which shipping resolves frames, in raw WAL bytes per second, as the shipper last reported
    /// it through [`Wal::set_drain_rate`]; 0 when nothing reports or nothing drains. It scales the `Retry-After`
    /// of a 503 for WAL space (contract 4.8).
    pub drain_bytes_per_s: u64,
}

impl Admission {
    /// Whether ingest may accept a request. When false it answers 503 with `Retry-After`.
    pub const fn accepting(&self) -> bool {
        !(self.failed || self.over_high_water || self.below_disk_floor)
    }
}

/// What one garbage collection did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GcReport {
    /// The watermark the collection ran against; zero when there is no checkpoint.
    pub watermark: Lsn,
    /// Segments removed, oldest first.
    pub removed: Vec<SegmentMeta>,
    /// Segments wholly at or below the watermark that replay retention kept.
    pub retained_for_replay: u64,
}

#[derive(Debug)]
pub(crate) struct WalMetrics {
    pub(crate) appended_frames: Counter,
    pub(crate) appended_bytes: Counter,
    pub(crate) group_failures: Counter,
    pub(crate) sync_duration: Histogram,
    pub(crate) durable_lsn: Gauge,
    pub(crate) checkpoint_lsn: Gauge,
    pub(crate) segment_bytes: Gauge,
    pub(crate) unshipped_bytes: Gauge,
    pub(crate) free_bytes: Gauge,
    pub(crate) failed: Gauge,
    pub(crate) gc_removed_segments: Counter,
}

impl WalMetrics {
    fn new(env: &Env) -> Self {
        let m = env.metrics.as_ref();
        Self {
            appended_frames: m.counter(&registry::WAL_APPENDED_FRAMES, &[]),
            appended_bytes: m.counter(&registry::WAL_APPENDED_BYTES, &[]),
            group_failures: m.counter(&registry::WAL_GROUP_FAILURES, &[]),
            sync_duration: m.histogram(&registry::WAL_SYNC_DURATION, &[]),
            durable_lsn: m.gauge(&registry::WAL_DURABLE_LSN, &[]),
            checkpoint_lsn: m.gauge(&registry::WAL_CHECKPOINT_LSN, &[]),
            segment_bytes: m.gauge(&registry::WAL_SEGMENT_BYTES, &[]),
            unshipped_bytes: m.gauge(&registry::WAL_UNSHIPPED_BYTES, &[]),
            free_bytes: m.gauge(&registry::WAL_FREE_BYTES, &[]),
            failed: m.gauge(&registry::WAL_FAILED, &[]),
            gc_removed_segments: m.counter(&registry::WAL_GC_REMOVED_SEGMENTS, &[]),
        }
    }
}

fn gauge_value(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// State shared by the handle, the committer and replay readers. Nothing here is held across an await.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) epoch: WalEpoch,
    pub(crate) config: WalConfig,
    pub(crate) disk: Arc<dyn Disk>,
    pub(crate) clock: Arc<dyn Clock>,
    /// The retained segments, oldest first. A new segment is registered before the durable LSN can
    /// advance into it.
    segments: Mutex<Vec<SegmentMeta>>,
    durable_lsn: AtomicU64,
    /// The highest checkpoint whose frame is durable. Only the committer and recovery set it, from log
    /// records; no caller can supply a watermark.
    watermark: Mutex<Option<Watermark>>,
    failed: AtomicBool,
    free_bytes: AtomicU64,
    /// What the shipper last reported through [`Wal::set_drain_rate`].
    drain_bytes_per_s: AtomicU64,
    pub(crate) metrics: WalMetrics,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    pub(crate) fn durable_lsn(&self) -> Lsn {
        Lsn::new(self.durable_lsn.load(Ordering::SeqCst))
    }

    pub(crate) fn set_durable_lsn(&self, lsn: u64) {
        self.durable_lsn.store(lsn, Ordering::SeqCst);
        self.metrics.durable_lsn.set(gauge_value(lsn));
    }

    pub(crate) fn watermark(&self) -> Option<Watermark> {
        *lock(&self.watermark)
    }

    pub(crate) fn watermark_lsn(&self) -> Lsn {
        self.watermark().map_or(Lsn::ZERO, |w| w.last_lsn())
    }

    /// Raises the watermark to a checkpoint whose frame is durable. Checkpoints are monotonic; a lower one
    /// changes nothing.
    pub(crate) fn raise_watermark(&self, checkpoint: Checkpoint, segment_no: u64) {
        let mut watermark = lock(&self.watermark);
        if watermark.is_none_or(|w| checkpoint.last_lsn >= w.last_lsn()) {
            *watermark = Some(Watermark {
                checkpoint,
                segment_no,
            });
            self.metrics
                .checkpoint_lsn
                .set(gauge_value(checkpoint.last_lsn.get()));
        }
        drop(watermark);
        self.publish_sizes();
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    pub(crate) fn set_failed(&self) {
        self.failed.store(true, Ordering::SeqCst);
        self.metrics.failed.set(1);
    }

    pub(crate) fn segments(&self) -> Vec<SegmentMeta> {
        lock(&self.segments).clone()
    }

    pub(crate) fn push_segment(&self, meta: SegmentMeta) {
        lock(&self.segments).push(meta);
        self.publish_sizes();
    }

    /// The first LSN of the segment after `segment_no`, when it has a successor. `None` when the segment
    /// itself is no longer retained: its successor, which says where its valid frames end, may be gone too.
    pub(crate) fn successor_first_lsn(&self, segment_no: u64) -> Option<Option<Lsn>> {
        let segments = lock(&self.segments);
        let index = segments.iter().position(|s| s.segment_no == segment_no)?;
        Some(segments.get(index + 1).map(|s| s.first_lsn))
    }

    pub(crate) async fn refresh_free_bytes(&self) {
        // A failed query keeps the last reading.
        if let Ok(free) = self.disk.free_space(Root::Wal).await {
            self.free_bytes.store(free, Ordering::SeqCst);
            self.metrics.free_bytes.set(gauge_value(free));
        }
    }

    /// Total segment bytes and the bytes of segments that hold a frame above the watermark.
    fn sizes(&self) -> (u64, u64) {
        let watermark = self.watermark_lsn();
        let segments = lock(&self.segments);
        let resolved = resolved_prefix(&segments, watermark) as u64;
        let total = segments.len() as u64;
        let bytes = self.config.segment_bytes;
        (
            total.saturating_mul(bytes),
            (total - resolved).saturating_mul(bytes),
        )
    }

    fn publish_sizes(&self) {
        let (total, unshipped) = self.sizes();
        self.metrics.segment_bytes.set(gauge_value(total));
        self.metrics.unshipped_bytes.set(gauge_value(unshipped));
    }
}

async fn remove_segment(disk: &dyn Disk, segment_no: u64) -> Result<(), DiskError> {
    match disk.remove(&segment_path(segment_no)?).await {
        // Already gone: an earlier collection removed it and failed before its `sync_dir`.
        Ok(()) | Err(DiskError::NotFound) => disk.sync_dir(&wal_dir()).await,
        Err(e) => Err(e),
    }
}

/// How many of the oldest segments lie wholly at or below `floor`. The last segment never counts: it is the
/// current one. A segment's last LSN is the next segment's first LSN minus 1.
fn resolved_prefix(segments: &[SegmentMeta], floor: Lsn) -> usize {
    segments
        .iter()
        .skip(1)
        .take_while(|next| next.first_lsn.get().saturating_sub(1) <= floor.get())
        .count()
}

/// The write-ahead log of one node. Cloning gives another handle to the same log; the committer stops
/// when the last handle is dropped.
#[derive(Clone, Debug)]
pub struct Wal {
    shared: Arc<Shared>,
    queue: mpsc::UnboundedSender<Item>,
    /// Serialises garbage collections; holds the watermark of the `CHECKPOINT` hint on disk.
    gc: Arc<tokio::sync::Mutex<Option<Lsn>>>,
}

impl Wal {
    /// Opens the WAL directory under the `wal` root: initialises an empty directory, otherwise recovers
    /// it (contract section 4.6), then starts the committer. The log accepts appends as soon as this
    /// returns.
    pub async fn open(
        env: &Env,
        config: WalConfig,
    ) -> Result<(Self, RecoveryReport), RecoveryError> {
        let capacity = config
            .segment_bytes
            .saturating_sub(SEGMENT_HEADER_LEN as u64);
        if capacity <= crucible_wal_codec::FRAME_HEADER_LEN as u64 {
            return Err(RecoveryError::InvalidConfig("segment_bytes holds no frame"));
        }
        let recovered = recover(env, &config).await?;
        let report = recovered.report;
        let last = report.segments.last().copied();
        let shared = Arc::new(Shared {
            epoch: report.epoch,
            config,
            disk: Arc::clone(&env.disk),
            clock: Arc::clone(&env.clock),
            segments: Mutex::new(report.segments.clone()),
            durable_lsn: AtomicU64::new(report.last_lsn.get()),
            watermark: Mutex::new(report.watermark),
            failed: AtomicBool::new(false),
            free_bytes: AtomicU64::new(u64::MAX),
            drain_bytes_per_s: AtomicU64::new(0),
            metrics: WalMetrics::new(env),
        });
        shared.set_durable_lsn(report.last_lsn.get());
        shared
            .metrics
            .checkpoint_lsn
            .set(gauge_value(report.watermark_lsn().get()));
        shared.metrics.failed.set(0);
        shared.publish_sizes();
        shared.refresh_free_bytes().await;

        let (queue, rx) = mpsc::unbounded_channel();
        let committer = Committer::new(
            Arc::clone(&shared),
            rx,
            recovered.file,
            last.map_or(1, |s| s.segment_no),
            recovered.write_offset,
        );
        // Dropping the handle detaches the task: it ends when the queue closes, or with its host's scope.
        drop(env.spawn("wal-committer", committer.run()));
        let wal = Self {
            shared,
            queue,
            gc: Arc::new(tokio::sync::Mutex::new(recovered.hint_lsn)),
        };
        Ok((wal, report))
    }

    /// Submits a frame. The future resolves with the frame's LSN only after the frame is durable
    /// (`wal.durability = group`); on `Err` the frame was not acknowledged and the caller may retry.
    ///
    /// The frame is queued by this call, not by the first poll, so frames submitted by one task keep their
    /// order. Dropping the future does not withdraw the frame.
    pub fn append(
        &self,
        frame: PreparedFrame,
    ) -> impl Future<Output = Result<Lsn, AppendError>> + Send + 'static + use<> {
        let submitted = self.submit(frame);
        async move {
            match submitted {
                Ok(done) => done.await.unwrap_or(Err(AppendError::Closed)),
                Err(e) => Err(e),
            }
        }
    }

    fn submit(
        &self,
        frame: PreparedFrame,
    ) -> Result<oneshot::Receiver<Result<Lsn, AppendError>>, AppendError> {
        if self.shared.is_failed() {
            return Err(AppendError::Failed);
        }
        let len = frame.encoded_len() as u64;
        let max = self.shared.config.segment_bytes - SEGMENT_HEADER_LEN as u64;
        if len > max {
            return Err(AppendError::TooLarge { len, max });
        }
        // The header fields the committer fills are not known yet; only the record type is read here.
        let record_type = frame.header(Lsn::ZERO, Timestamp::EPOCH).record_type;
        let checkpoint = match record_type {
            RecordType::BatchSeal | RecordType::Checkpoint => {
                self.checked_control(record_type, &frame)?
            }
            _ => None,
        };
        let (done, receiver) = oneshot::channel();
        let item = Item {
            frame,
            checkpoint,
            done,
        };
        self.queue.send(item).map_err(|_| AppendError::Closed)?;
        Ok(receiver)
    }

    /// Decodes a `BatchSeal` or `Checkpoint` payload off the committer and returns the checkpoint. A
    /// seal or a watermark may only cover durable frames: a watermark above the durable LSN would let
    /// garbage collection remove frames that were never resolved, and recovery refuses a log whose
    /// control records reach their own LSN.
    fn checked_control(
        &self,
        record_type: RecordType,
        frame: &PreparedFrame,
    ) -> Result<Option<Checkpoint>, AppendError> {
        let payload = load_payload(frame.is_compressed(), frame.stored_payload())
            .map_err(|_| AppendError::InvalidControl)?;
        let durable = self.shared.durable_lsn();
        match Record::decode(record_type, &payload) {
            Ok(Record::Checkpoint(checkpoint)) if checkpoint.last_lsn <= durable => {
                Ok(Some(checkpoint))
            }
            Ok(Record::BatchSeal(seal)) if seal.last_lsn <= durable => Ok(None),
            _ => Err(AppendError::InvalidControl),
        }
    }

    /// The directory's epoch.
    pub fn epoch(&self) -> WalEpoch {
        self.shared.epoch
    }

    /// The last LSN whose frame is durable. In the unsafe durability modes this is the last appended LSN.
    pub fn durable_lsn(&self) -> Lsn {
        self.shared.durable_lsn()
    }

    /// The highest durable checkpoint.
    pub fn watermark(&self) -> Option<Watermark> {
        self.shared.watermark()
    }

    /// The retained segments, oldest first.
    pub fn segments(&self) -> Vec<SegmentMeta> {
        self.shared.segments()
    }

    /// The signals ingest admission reads.
    pub fn admission(&self) -> Admission {
        let config = &self.shared.config;
        let (segment_bytes, unshipped_bytes) = self.shared.sizes();
        let free_bytes = self.shared.free_bytes.load(Ordering::SeqCst);
        Admission {
            failed: self.shared.is_failed(),
            over_high_water: segment_bytes >= config.high_water_bytes(),
            below_disk_floor: free_bytes < config.min_free_bytes,
            segment_bytes,
            unshipped_bytes,
            free_bytes,
            durability_unsafe: config.durability.is_unsafe(),
            drain_bytes_per_s: self.shared.drain_bytes_per_s.load(Ordering::SeqCst),
        }
    }

    /// Records the rate at which shipping resolves frames, in raw WAL bytes per second. The shipper reports it
    /// at every checkpoint and on its idle timer; admission reads it through [`Wal::admission`].
    pub fn set_drain_rate(&self, bytes_per_s: u64) {
        self.shared
            .drain_bytes_per_s
            .store(bytes_per_s, Ordering::SeqCst);
    }

    /// Reads durable frames in LSN order, starting at `from`.
    pub fn replay(&self, from: Lsn) -> Replay {
        Replay::new(Arc::clone(&self.shared), from)
    }

    /// Reads the durable frame whose header starts at `offset` of segment `segment_no`: the point read behind
    /// a recent-span locator (contract 8.6). `None` when the segment is no longer retained or no durable frame
    /// starts at that offset.
    pub async fn read_at(
        &self,
        segment_no: u64,
        offset: u64,
    ) -> Result<Option<WalFrame>, DiskError> {
        let durable = self.shared.durable_lsn();
        let retained = self.shared.segments();
        if !retained.iter().any(|s| s.segment_no == segment_no) {
            return Ok(None);
        }
        let file = match self
            .shared
            .disk
            .open(&segment_path(segment_no)?, OpenMode::Read)
            .await
        {
            Ok(file) => file,
            Err(DiskError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        match SegmentReader::new(file, segment_no, offset).step().await? {
            Step::Frame(frame) if frame.lsn() <= durable => Ok(Some(frame)),
            Step::Frame(_) | Step::End(_) => Ok(None),
        }
    }

    /// Rewrites the `CHECKPOINT` hint when the watermark moved, then removes every segment that may go
    /// (contract section 4.7): wholly at or below the watermark, below `recent_floor` (the lowest LSN the
    /// recent-span index still references), not the current segment, and outside replay retention by age,
    /// by the byte budget, or because total bytes are above the high-water mark. Removal is oldest first,
    /// so the log stays contiguous; each removal is `remove` then `sync_dir`.
    pub async fn gc(&self, recent_floor: Option<Lsn>) -> Result<GcReport, GcError> {
        let mut hint_lsn = self.gc.lock().await;
        let shared = &self.shared;
        let watermark = shared.watermark();
        let watermark_lsn = watermark.map_or(Lsn::ZERO, |w| w.last_lsn());
        if let Some(watermark) = watermark
            && *hint_lsn != Some(watermark_lsn)
        {
            write_hint(shared.disk.as_ref(), watermark.hint()).await?;
            *hint_lsn = Some(watermark_lsn);
        }

        let mut floor = watermark_lsn;
        if shared.config.gc_above_checkpoint() {
            floor = shared.durable_lsn();
        }
        if let Some(recent) = recent_floor {
            floor = floor.min(Lsn::new(recent.get().saturating_sub(1)));
        }
        let segments = shared.segments();
        let resolved = resolved_prefix(&segments, floor);
        let config = &shared.config;
        let now = shared.clock.wall_now().micros();
        let retention = config.replay_retention_micros();
        let under_pressure = (segments.len() as u64).saturating_mul(config.segment_bytes)
            >= config.high_water_bytes();
        // Retained segments are evicted oldest first. One may stay only while it is younger than the
        // retention, the segments kept from it on fit the byte budget, and the log is below high-water.
        let mut removable = 0;
        for (index, next) in segments.iter().skip(1).take(resolved).enumerate() {
            let kept_bytes = ((resolved - index) as u64).saturating_mul(config.segment_bytes);
            // A segment was closed when its successor was created.
            let age = now.saturating_sub(next.created_at_micros);
            if !under_pressure && age < retention && kept_bytes <= config.replay_budget_bytes() {
                break;
            }
            removable = index + 1;
        }

        // One `sync_dir` per removal: a crash must never keep a later removal and lose an earlier one,
        // which would leave a hole in the segment numbers.
        let mut removed = Vec::with_capacity(removable);
        let mut result = Ok(());
        for meta in segments.iter().take(removable) {
            result = remove_segment(shared.disk.as_ref(), meta.segment_no).await;
            if result.is_err() {
                break;
            }
            lock(&shared.segments).retain(|s| s.segment_no != meta.segment_no);
            removed.push(*meta);
        }
        if !removed.is_empty() {
            shared.publish_sizes();
            shared.metrics.gc_removed_segments.add(removed.len() as u64);
        }
        shared.refresh_free_bytes().await;
        result?;
        Ok(GcReport {
            watermark: watermark_lsn,
            retained_for_replay: (resolved - removed.len()) as u64,
            removed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(segment_no: u64, first_lsn: u64) -> SegmentMeta {
        SegmentMeta {
            segment_no,
            first_lsn: Lsn::new(first_lsn),
            created_at_micros: 0,
        }
    }

    #[test]
    fn only_segments_wholly_at_or_below_the_floor_are_resolved() {
        let segments = [meta(1, 1), meta(2, 10), meta(3, 10), meta(4, 25)];
        let resolved = |floor| resolved_prefix(&segments, Lsn::new(floor));
        assert_eq!(resolved(0), 0);
        assert_eq!(resolved(8), 0);
        // Segment 1 ends at lsn 9; segment 2 was abandoned empty and ends there too.
        assert_eq!(resolved(9), 2);
        assert_eq!(resolved(23), 2);
        assert_eq!(resolved(24), 3);
        // The current segment is never resolved.
        assert_eq!(resolved(u64::MAX), 3);
        assert_eq!(resolved_prefix(&[], Lsn::new(5)), 0);
        assert_eq!(resolved_prefix(&segments[..1], Lsn::new(5)), 0);
    }
}
