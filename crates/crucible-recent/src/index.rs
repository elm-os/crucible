//! The index and its one writer.

use std::collections::BTreeSet;
use std::fmt;
use std::future::poll_fn;
use std::hash::{BuildHasher, Hash};
use std::sync::{Arc, PoisonError};
use std::task::{Poll, Waker};
use std::time::Duration;

use crucible_env::{Clock, ClockExt, StoreFlavour};
use crucible_records::{
    Lsn, PartitionId, ProjectId, ProjectScope, SpanId, SpanRecord, TableId, TimeWindow, Timestamp,
    TraceId,
};
use rustc_hash::FxBuildHasher;

use crate::control::{Control, QuarantinedPair, ReadState};
use crate::entry::{RecentSpan, SpanEntry};
use crate::shard::{DeletionSelector, Shard};
use crate::sync::{AtomicU64, Mutex, Ordering, RwLock};
use crate::wal::{WalReadError, WalView};

/// The number of shards.
pub const SHARDS: usize = 16;

/// The settings of the index (contract 8.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecentConfig {
    /// `recent.max_bytes`: the bound on the bytes charged for entries and their strings.
    pub max_bytes: u64,
    /// How long after its checkpoint an entry stays, so that the store serves it before the index forgets it.
    pub visibility_grace: Duration,
    /// `recent.read_barrier`: the longest a trace-by-id read waits for stage B. It must stay shorter than the
    /// visibility grace: a reader may start its store read up to one barrier before it snapshots the index.
    pub read_barrier: Duration,
}

impl RecentConfig {
    /// The default of `recent.read_barrier`.
    pub const READ_BARRIER: Duration = Duration::from_millis(250);

    /// The settings for a store of this flavour, with the default read barrier.
    pub const fn new(max_bytes: u64, flavour: StoreFlavour) -> Self {
        Self {
            max_bytes,
            visibility_grace: Self::visibility_grace(flavour),
            read_barrier: Self::READ_BARRIER,
        }
    }

    /// The visibility grace: 2 s on a single node, 10 s clustered or on Cloud.
    pub const fn visibility_grace(flavour: StoreFlavour) -> Duration {
        match flavour {
            StoreFlavour::SingleNode => Duration::from_secs(2),
            StoreFlavour::Clustered | StoreFlavour::Cloud => Duration::from_secs(10),
        }
    }

    /// The default of `recent.max_bytes` on a host with this much memory: min(1 GiB, 10% of RAM).
    pub const fn default_max_bytes(ram_bytes: u64) -> u64 {
        let tenth = ram_bytes / 10;
        if tenth < 1 << 30 { tenth } else { 1 << 30 }
    }
}

/// Whether a response reflects every span this node acknowledged before the request arrived (contract 8.6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Consistency {
    /// It does.
    Strong,
    /// It may trail: stage B had not applied through the durable LSN.
    Lagging,
}

/// One WAL frame as stage B applies it to the index. Frames are applied in LSN order, each exactly once; the
/// replay after a restart yields the same type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Frame {
    /// The accepted spans of an ingest frame.
    Spans {
        /// The frame's LSN.
        lsn: Lsn,
        /// The frame header's `received_at`.
        received_at: Timestamp,
        /// The spans, in wire order.
        spans: Vec<SpanEntry>,
    },
    /// A checkpoint: every frame at or below `last_lsn` is resolved.
    Checkpoint {
        /// The frame's LSN.
        lsn: Lsn,
        /// The low watermark.
        last_lsn: Lsn,
    },
    /// A quarantine frame.
    Quarantine {
        /// The frame's LSN.
        lsn: Lsn,
        /// The pairs it lists.
        pairs: Vec<QuarantinedPair>,
    },
    /// A tombstone: matching entries that arrived before `requested_at` are removed.
    Tombstone {
        /// The frame's LSN.
        lsn: Lsn,
        /// The project.
        project_id: ProjectId,
        /// The deletion's point in time.
        requested_at: Timestamp,
        /// What is deleted.
        selector: DeletionSelector,
    },
    /// Any other frame. It only moves the applied LSN.
    Other {
        /// The frame's LSN.
        lsn: Lsn,
    },
}

/// The size of the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecentStats {
    /// The number of entries.
    pub entries: u64,
    /// The bytes charged against `recent.max_bytes`.
    pub bytes: u64,
}

fn lock<G>(result: Result<G, PoisonError<G>>) -> G {
    // A panic while a lock is held is a bug elsewhere; the maps stay usable.
    result.unwrap_or_else(PoisonError::into_inner)
}

/// The recent-span index (contract 8.6): what makes a span readable from its ack until the store serves it.
///
/// Readers take a `ProjectScope` and never see another project's entries. The one writer is the
/// [`RecentWriter`] that [`RecentIndex::new`] returns.
pub struct RecentIndex {
    config: RecentConfig,
    clock: Arc<dyn Clock>,
    wal: Arc<dyn WalView>,
    shards: [RwLock<Shard>; SHARDS],
    control: RwLock<Control>,
    /// The LSN of the last frame stage B applied.
    applied: AtomicU64,
    bytes: AtomicU64,
    /// Read barriers waiting for `applied` to move.
    waiters: Mutex<Vec<Waker>>,
}

impl fmt::Debug for RecentIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecentIndex")
            .field("config", &self.config)
            .field("applied", &self.applied())
            .finish_non_exhaustive()
    }
}

fn shard_of(project: ProjectId, trace: TraceId) -> usize {
    let mut hasher = FxBuildHasher.build_hasher();
    (project, trace).hash(&mut hasher);
    (std::hash::Hasher::finish(&hasher) >> 32) as usize % SHARDS
}

impl RecentIndex {
    /// An empty index and its writer.
    pub fn new(
        config: RecentConfig,
        clock: Arc<dyn Clock>,
        wal: Arc<dyn WalView>,
    ) -> (Arc<Self>, RecentWriter) {
        let index = Arc::new(Self {
            config,
            clock,
            wal,
            shards: std::array::from_fn(|_| RwLock::new(Shard::new())),
            control: RwLock::new(Control::default()),
            applied: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            waiters: Mutex::new(Vec::new()),
        });
        let writer = RecentWriter {
            index: Arc::clone(&index),
            bytes: 0,
            evicted: Lsn::ZERO,
        };
        (index, writer)
    }

    /// Rebuilds the index after a restart (contract 4.6 step 7). `watermark` is the recovered checkpoint and
    /// `replay` yields, in LSN order, every frame above it plus the frames at or below it that are younger
    /// than the visibility grace, and every `Quarantine` frame that is still open. The grace of the recovered
    /// checkpoint restarts now, so the replayed entries below it stay for one more grace.
    pub fn rebuild(
        config: RecentConfig,
        clock: Arc<dyn Clock>,
        wal: Arc<dyn WalView>,
        watermark: Lsn,
        replay: impl IntoIterator<Item = Frame>,
    ) -> (Arc<Self>, RecentWriter) {
        let (index, mut writer) = Self::new(config, clock, wal);
        lock(index.control.write()).checkpoint(watermark, index.clock.now());
        index.publish(watermark);
        for frame in replay {
            writer.apply(frame);
        }
        (index, writer)
    }

    /// The settings.
    pub const fn config(&self) -> &RecentConfig {
        &self.config
    }

    /// The LSN of the last frame applied.
    pub fn applied(&self) -> Lsn {
        Lsn::new(self.applied.load(Ordering::SeqCst))
    }

    /// Whether stage B has applied every durable frame. A read that starts while this holds reflects every
    /// span acknowledged before it.
    pub fn caught_up(&self) -> bool {
        self.applied() >= self.wal.durable_lsn()
    }

    /// The size of the index.
    pub fn stats(&self) -> RecentStats {
        let entries = self
            .shards
            .iter()
            .map(|s| lock(s.read()).len() as u64)
            .sum();
        RecentStats {
            entries,
            bytes: self.bytes.load(Ordering::SeqCst),
        }
    }

    fn publish(&self, lsn: Lsn) {
        self.applied.fetch_max(lsn.get(), Ordering::SeqCst);
        // The waiters lock is taken after the store, and a barrier re-checks under it before it waits, so no
        // barrier sleeps through this wake.
        let waiters = std::mem::take(&mut *lock(self.waiters.lock()));
        for waiter in waiters {
            waiter.wake();
        }
    }

    /// The read barrier (contract 8.6): snapshots the durable LSN and waits, at most `recent.read_barrier`,
    /// until stage B has applied through it. Read the index after this returns, before any store read.
    pub async fn read_barrier(&self) -> Consistency {
        let target = self.wal.durable_lsn();
        if self.applied() >= target {
            return Consistency::Strong;
        }
        let applied = poll_fn(|cx| {
            if self.applied() >= target {
                return Poll::Ready(());
            }
            let mut waiters = lock(self.waiters.lock());
            if self.applied() >= target {
                return Poll::Ready(());
            }
            waiters.push(cx.waker().clone());
            Poll::Pending
        });
        match self.clock.timeout(self.config.read_barrier, applied).await {
            Ok(()) => Consistency::Strong,
            Err(_) => Consistency::Lagging,
        }
    }

    fn state(&self, scope: ProjectScope) -> ReadState {
        lock(self.control.read()).state(scope.project_id())
    }

    /// The gap, quarantine and watermark state of a project. A reader that also reads entries must take the
    /// state from the same call as the entries; this is for reads that found nothing to look up.
    pub fn read_state(&self, scope: ProjectScope) -> ReadState {
        self.state(scope)
    }

    /// The entries of one trace, in `span_id` order.
    pub fn trace(&self, scope: ProjectScope, trace_id: TraceId) -> (Vec<RecentSpan>, ReadState) {
        let project = scope.project_id();
        let spans = lock(self.shards[shard_of(project, trace_id)].read()).trace(project, trace_id);
        (spans, self.state(scope))
    }

    /// The entries of several traces, concatenated.
    pub fn traces(
        &self,
        scope: ProjectScope,
        trace_ids: &[TraceId],
    ) -> (Vec<RecentSpan>, ReadState) {
        let project = scope.project_id();
        let mut spans = Vec::new();
        for &trace_id in trace_ids {
            spans.extend(
                lock(self.shards[shard_of(project, trace_id)].read()).trace(project, trace_id),
            );
        }
        (spans, self.state(scope))
    }

    /// The trace heads of the project whose `start_time` is inside the window, in no defined order.
    pub fn heads(&self, scope: ProjectScope, window: TimeWindow) -> (Vec<RecentSpan>, ReadState) {
        let mut heads = Vec::new();
        for shard in &self.shards {
            lock(shard.read()).heads(scope.project_id(), window, &mut heads);
        }
        (heads, self.state(scope))
    }

    /// Live tail: the entries of the project with an LSN above `after`, in LSN order.
    pub fn since(&self, scope: ProjectScope, after: Lsn) -> (Vec<RecentSpan>, ReadState) {
        let mut spans = Vec::new();
        for shard in &self.shards {
            lock(shard.read()).since(scope.project_id(), after, &mut spans);
        }
        spans.sort_by_key(|span| (span.lsn, span.summary.trace_id, span.summary.span_id));
        (spans, self.state(scope))
    }

    /// The earliest and latest `start_time` of the project's entries.
    pub fn time_bounds(&self, scope: ProjectScope) -> (Option<(Timestamp, Timestamp)>, ReadState) {
        let mut bounds: Option<(Timestamp, Timestamp)> = None;
        for shard in &self.shards {
            if let Some((first, last)) = lock(shard.read()).time_bounds(scope.project_id()) {
                bounds = Some(bounds.map_or((first, last), |(a, b)| (a.min(first), b.max(last))));
            }
        }
        (bounds, self.state(scope))
    }

    /// The full record of a recent span, re-read from the WAL at its locator. `None` when the index does not
    /// hold the span.
    pub async fn span_record(
        &self,
        scope: ProjectScope,
        trace_id: TraceId,
        span_id: SpanId,
    ) -> Result<Option<SpanRecord>, WalReadError> {
        let project = scope.project_id();
        let found =
            lock(self.shards[shard_of(project, trace_id)].read()).span(project, trace_id, span_id);
        let Some(found) = found else {
            return Ok(None);
        };
        let record = self.wal.read_span(project, found.locator).await?;
        let core = &record.core;
        if (core.project_id, core.trace_id, core.span_id) != (project, trace_id, span_id) {
            return Err(WalReadError::Gone);
        }
        Ok(Some(record))
    }
}

/// The one writer of the index: stage B. It is not `Clone`, and every method takes `&mut self`.
///
/// The writer takes project ids from WAL frames, not from a `ProjectScope`: it files each entry under the
/// project its frame was accepted for, and only scoped readers get entries back.
#[derive(Debug)]
pub struct RecentWriter {
    index: Arc<RecentIndex>,
    bytes: u64,
    /// Entries at or below this LSN have been evicted.
    evicted: Lsn,
}

impl RecentWriter {
    /// The index this writer feeds.
    pub const fn index(&self) -> &Arc<RecentIndex> {
        &self.index
    }

    /// Applies one frame. On return the frame is visible to readers and the applied LSN is the frame's.
    pub fn apply(&mut self, frame: Frame) {
        let index = Arc::clone(&self.index);
        let lsn = match frame {
            Frame::Spans {
                lsn,
                received_at,
                spans,
            } => {
                for span in &spans {
                    let shard = shard_of(span.project_id, span.summary.trace_id);
                    self.mutate(shard, |shard| shard.insert(lsn, received_at, span));
                }
                lsn
            }
            Frame::Checkpoint { lsn, last_lsn } => {
                lock(index.control.write()).checkpoint(last_lsn, index.clock.now());
                lsn
            }
            Frame::Quarantine { lsn, pairs } => {
                let mut control = lock(index.control.write());
                pairs.into_iter().for_each(|pair| control.quarantine(pair));
                lsn
            }
            Frame::Tombstone {
                lsn,
                project_id,
                requested_at,
                selector,
            } => {
                for shard in 0..SHARDS {
                    self.mutate(shard, |shard| {
                        shard.remove_selected(project_id, requested_at, &selector);
                    });
                }
                lsn
            }
            Frame::Other { lsn } => lsn,
        };
        self.maintain();
        index.publish(lsn);
    }

    /// A quarantined pair was requeued and is present in the store: reads of its projects are whole again.
    pub fn requeued(&mut self, batch_seq: Lsn, table: TableId, partition: PartitionId) {
        lock(self.index.control.write()).requeue(batch_seq, table, partition);
    }

    /// Evicts the entries whose checkpoint is now older than the visibility grace, then enforces the byte bound.
    /// `apply` does this for every frame; stage B also calls it on a timer, so that an idle node lets go.
    pub fn maintain(&mut self) {
        let index = Arc::clone(&self.index);
        let now = index.clock.now();
        let evictable = lock(index.control.write()).advance(now, index.config.visibility_grace);
        if evictable > self.evicted {
            self.remove_through(evictable);
            self.evicted = evictable;
        }
        // The gap rule: the bound is hit before entries are evictable. The oldest frame's entries go, and the
        // gap is recorded first, so that no reader finds the entries gone and the gap not yet open.
        while self.bytes > index.config.max_bytes {
            let oldest = (0..SHARDS)
                .filter_map(|shard| lock(index.shards[shard].read()).front_lsn())
                .min();
            let Some(oldest) = oldest else {
                break;
            };
            let mut projects = BTreeSet::new();
            for shard in &index.shards {
                lock(shard.read()).projects_through(oldest, &mut projects);
            }
            lock(index.control.write()).open_gap(&projects, oldest);
            self.remove_through(oldest);
        }
    }

    fn remove_through(&mut self, lsn: Lsn) {
        for shard in 0..SHARDS {
            self.mutate(shard, |shard| shard.remove_through(lsn));
        }
    }

    fn mutate(&mut self, shard: usize, change: impl FnOnce(&mut Shard)) {
        let mut shard = lock(self.index.shards[shard].write());
        let before = shard.bytes();
        change(&mut shard);
        self.bytes = self.bytes - before + shard.bytes();
        self.index.bytes.store(self.bytes, Ordering::SeqCst);
    }
}
