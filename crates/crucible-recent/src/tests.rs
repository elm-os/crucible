//! Tests of the index. Under `cfg(test)` its locks and atomics are Shuttle's, so every test runs inside a
//! Shuttle execution: the concurrency tests explore many schedules, the others exactly one.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crucible_auth::test_support::scope;
use crucible_env::StoreFlavour;
use crucible_records::{
    CivilDate, HeadKind, Level, Lsn, NanoUsd, ObservationType, PartitionId, ProjectId,
    ProjectScope, SpanId, StatusCode, TableId, TimeWindow, Timestamp, TraceId,
};
use shuttle::scheduler::{PctScheduler, RandomScheduler};
use shuttle::sync::Mutex;
use shuttle::sync::atomic::{AtomicU64, Ordering};
use shuttle::{Runner, thread};

use crate::test_support::{FakeWal, ManualClock, span_record};
use crate::{
    Consistency, DeletionSelector, ENTRY_BYTES, Frame, QuarantineReach, QuarantinedPair,
    RecentConfig, RecentIndex, RecentSpan, RecentWriter, SpanEntry, SpanSummary, WalLocator,
    WalReadError,
};

/// Schedules explored by each concurrency test: random, then PCT with depth 3.
const RANDOM_SCHEDULES: usize = 3_000;
const PCT_SCHEDULES: usize = 1_000;

const GRACE: Duration = Duration::from_secs(2);
const WALL: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

/// Shuttle's default thread stack is 32 KiB, which a `SpanRecord` on the stack overflows.
fn config() -> shuttle::Config {
    let mut config = shuttle::Config::new();
    config.stack_size = 1 << 20;
    config
}

fn explore(test: impl Fn() + Send + Sync + 'static) {
    let test = Arc::new(test);
    let random = Arc::clone(&test);
    Runner::new(RandomScheduler::new(RANDOM_SCHEDULES), config()).run(move || random());
    Runner::new(PctScheduler::new(3, PCT_SCHEDULES), config()).run(move || test());
}

fn once(test: impl Fn() + Send + Sync + 'static) {
    Runner::new(RandomScheduler::new(1), config()).run(test);
}

fn project(n: u8) -> ProjectId {
    ProjectId::from_bytes([n; 16])
}

fn scoped(n: u8) -> ProjectScope {
    scope(project(n)).unwrap()
}

/// The trace and the project of the span that frame `lsn` carries: one span per frame, two projects.
fn trace_of(lsn: u64) -> TraceId {
    TraceId::from_bytes([lsn as u8; 16])
}

fn project_of(lsn: u64) -> u8 {
    (lsn % 2) as u8 + 1
}

fn entry(lsn: u64) -> SpanEntry {
    SpanEntry {
        project_id: project(project_of(lsn)),
        summary: SpanSummary {
            trace_id: trace_of(lsn),
            span_id: SpanId::from_bytes([lsn as u8; 8]),
            parent_span_id: SpanId::ZERO,
            is_root: true,
            start_time: Timestamp::from_micros(1_000 + lsn as i64),
            end_time: Timestamp::from_micros(2_000),
            observation_type: ObservationType::Span,
            level: Level::Default,
            status_code: StatusCode::Ok,
            name: Arc::from(""),
            trace_name: Arc::from(""),
            user_id: Arc::from(""),
            session_id: Arc::from(""),
            model: Arc::from(""),
            environment: Arc::from(""),
            total_cost_nusd: NanoUsd::ZERO,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            usage_aggregate: false,
        },
        head: Some(HeadKind::Root),
        locator: WalLocator {
            segment: 1,
            offset: lsn * 64,
            ordinal: 0,
        },
    }
}

fn spans(lsn: u64) -> Frame {
    Frame::Spans {
        lsn: Lsn::new(lsn),
        received_at: WALL,
        spans: vec![entry(lsn)],
    }
}

struct Rig {
    clock: ManualClock,
    wal: FakeWal,
    index: Arc<RecentIndex>,
    writer: RecentWriter,
}

/// An index that holds at most `entries` of the test's entries (their strings are empty, so each costs
/// exactly [`ENTRY_BYTES`]).
fn rig(entries: u64) -> Rig {
    let clock = ManualClock::new(WALL);
    let wal = FakeWal::new();
    let config = RecentConfig::new(entries * ENTRY_BYTES, StoreFlavour::SingleNode);
    let (index, writer) = RecentIndex::new(config, Arc::new(clock.clone()), Arc::new(wal.clone()));
    Rig {
        clock,
        wal,
        index,
        writer,
    }
}

fn holds(index: &RecentIndex, lsn: u64) -> bool {
    let (found, _) = index.trace(scoped(project_of(lsn)), trace_of(lsn));
    found.iter().any(|span| span.lsn == Lsn::new(lsn))
}

fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

// ---- One writer against many readers (invariant I3, Shuttle) ----

/// With no checkpoint, nothing is evictable: a span that was applied is either in the index or behind an open
/// gap of its project, whatever the interleaving, and the byte bound holds after every frame.
#[test]
fn a_reader_finds_every_applied_span_or_its_gap() {
    explore(|| {
        const FRAMES: u64 = 6;
        let Rig {
            index, mut writer, ..
        } = rig(3);
        let acked = Arc::new(AtomicU64::new(0));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let (index, acked) = (Arc::clone(&index), Arc::clone(&acked));
                thread::spawn(move || {
                    for _ in 0..3 {
                        let seen = acked.load(Ordering::SeqCst);
                        for lsn in 1..=seen {
                            let (found, state) =
                                index.trace(scoped(project_of(lsn)), trace_of(lsn));
                            let present = found.iter().any(|span| span.lsn == Lsn::new(lsn));
                            assert!(present || state.gap, "silent miss of lsn {lsn}");
                            let (other, _) =
                                index.trace(scoped(project_of(lsn + 1)), trace_of(lsn));
                            assert!(other.is_empty(), "a scope read another project's entry");
                        }
                    }
                })
            })
            .collect();
        for lsn in 1..=FRAMES {
            writer.apply(spans(lsn));
            let stats = index.stats();
            assert!(stats.bytes <= index.config().max_bytes && stats.entries <= 3);
            acked.store(lsn, Ordering::SeqCst);
        }
        readers
            .into_iter()
            .for_each(|reader| reader.join().unwrap());
        // The three newest frames remain; both projects lost entries and say so.
        assert!((4..=FRAMES).all(|lsn| holds(&index, lsn)));
        assert!(index.read_state(scoped(1)).gap && index.read_state(scoped(2)).gap);
    });
}

/// The gap bookkeeping against a store that lags: frames are shipped, checkpointed and aged past the grace
/// while readers run. A reader that looks in the index first and in the store second finds every applied span,
/// or is told about the gap. Once everything is checkpointed and the grace has passed, no gap is left open and
/// the index is empty.
#[test]
fn gaps_cover_every_dropped_span_until_the_store_serves_it() {
    explore(|| {
        const FRAMES: u64 = 6;
        let Rig {
            clock,
            index,
            mut writer,
            ..
        } = rig(2);
        let acked = Arc::new(AtomicU64::new(0));
        let store = Arc::new(Mutex::new(BTreeSet::<u64>::new()));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let (index, acked, store) =
                    (Arc::clone(&index), Arc::clone(&acked), Arc::clone(&store));
                thread::spawn(move || {
                    for _ in 0..3 {
                        let seen = acked.load(Ordering::SeqCst);
                        for lsn in 1..=seen {
                            let (found, state) =
                                index.trace(scoped(project_of(lsn)), trace_of(lsn));
                            let recent = found.iter().any(|span| span.lsn == Lsn::new(lsn));
                            let stored = store.lock().unwrap().contains(&lsn);
                            assert!(recent || stored || state.gap, "silent miss of lsn {lsn}");
                        }
                    }
                })
            })
            .collect();
        // Frame LSNs: spans at 1..=6, then the checkpoints. The shipper lands a batch in the store, then its
        // checkpoint is applied, then the grace passes.
        let mut next = FRAMES;
        for lsn in 1..=FRAMES {
            writer.apply(spans(lsn));
            assert!(index.stats().bytes <= index.config().max_bytes);
            acked.store(lsn, Ordering::SeqCst);
            if lsn % 3 == 0 {
                store.lock().unwrap().extend(lsn - 2..=lsn);
                next += 1;
                writer.apply(Frame::Checkpoint {
                    lsn: Lsn::new(next),
                    last_lsn: Lsn::new(lsn),
                });
                clock.advance(GRACE);
                writer.maintain();
            }
        }
        readers
            .into_iter()
            .for_each(|reader| reader.join().unwrap());
        assert_eq!(index.stats().entries, 0);
        assert!(!index.read_state(scoped(1)).gap && !index.read_state(scoped(2)).gap);
        assert_eq!(index.applied(), Lsn::new(next));
    });
}

/// The barrier's check-then-wait against the writer's store-then-wake: a barrier that starts behind the durable
/// LSN is always woken by the frame that reaches it. The clock never moves, so a lost wake would hang, which
/// Shuttle reports as a deadlock.
#[test]
fn the_barrier_never_sleeps_through_the_frame_it_waits_for() {
    explore(|| {
        let Rig {
            wal,
            index,
            mut writer,
            ..
        } = rig(100);
        wal.set_durable(Lsn::new(3));
        let barriers: Vec<_> = (0..2)
            .map(|_| {
                let index = Arc::clone(&index);
                thread::spawn(move || {
                    let consistency = shuttle::future::block_on(index.read_barrier());
                    assert_eq!(consistency, Consistency::Strong);
                    // Strong means the acknowledged frames are readable now.
                    assert!((1..=3).all(|lsn| holds(&index, lsn)));
                })
            })
            .collect();
        (1..=3).for_each(|lsn| writer.apply(spans(lsn)));
        barriers
            .into_iter()
            .for_each(|barrier| barrier.join().unwrap());
    });
}

// ---- Single-threaded behaviour ----

#[test]
fn the_barrier_waits_at_most_250_ms_and_then_answers_lagging() {
    once(|| {
        let Rig {
            clock,
            wal,
            index,
            mut writer,
        } = rig(100);
        // Caught up: no wait at all.
        writer.apply(spans(1));
        wal.set_durable(Lsn::new(1));
        assert!(index.caught_up());
        assert_eq!(
            poll_once(pin!(index.read_barrier())),
            Poll::Ready(Consistency::Strong)
        );

        // Behind, and stage B does not get there: lagging at exactly the barrier, not before.
        wal.set_durable(Lsn::new(3));
        assert!(!index.caught_up());
        let mut barrier = pin!(index.read_barrier());
        assert_eq!(poll_once(barrier.as_mut()), Poll::Pending);
        writer.apply(spans(2));
        assert_eq!(poll_once(barrier.as_mut()), Poll::Pending);
        clock.advance(Duration::from_millis(249));
        assert_eq!(poll_once(barrier.as_mut()), Poll::Pending);
        clock.advance(Duration::from_millis(1));
        assert_eq!(
            poll_once(barrier.as_mut()),
            Poll::Ready(Consistency::Lagging)
        );

        // Behind, and stage B arrives inside the barrier: strong, as soon as it does.
        let mut barrier = pin!(index.read_barrier());
        assert_eq!(poll_once(barrier.as_mut()), Poll::Pending);
        clock.advance(Duration::from_millis(100));
        writer.apply(Frame::Other { lsn: Lsn::new(3) });
        assert_eq!(
            poll_once(barrier.as_mut()),
            Poll::Ready(Consistency::Strong)
        );
        // The durable LSN is snapshotted on arrival: later acks do not extend the wait.
        wal.set_durable(Lsn::new(4));
        let mut barrier = pin!(index.read_barrier());
        assert_eq!(poll_once(barrier.as_mut()), Poll::Pending);
        wal.set_durable(Lsn::new(9));
        writer.apply(Frame::Other { lsn: Lsn::new(4) });
        assert_eq!(
            poll_once(barrier.as_mut()),
            Poll::Ready(Consistency::Strong)
        );
    });
}

#[test]
fn an_entry_leaves_only_after_its_checkpoint_and_the_grace() {
    once(|| {
        let Rig {
            clock,
            index,
            mut writer,
            ..
        } = rig(100);
        assert_eq!(index.floor(), None);
        (1..=4).for_each(|lsn| writer.apply(spans(lsn)));
        clock.advance(GRACE * 10);
        writer.maintain();
        assert_eq!(index.stats().entries, 4, "age alone evicts nothing");
        assert_eq!(index.floor(), Some(Lsn::new(1)));

        writer.apply(Frame::Checkpoint {
            lsn: Lsn::new(5),
            last_lsn: Lsn::new(2),
        });
        clock.advance(GRACE - Duration::from_nanos(1));
        writer.maintain();
        assert_eq!(index.stats().entries, 4, "the grace has not elapsed");
        assert_eq!(index.read_state(scoped(1)).watermark, Lsn::new(2));
        clock.advance(Duration::from_nanos(1));
        writer.maintain();
        assert!(!holds(&index, 1) && !holds(&index, 2) && holds(&index, 3) && holds(&index, 4));
        assert_eq!(index.floor(), Some(Lsn::new(3)));
        assert!(!index.read_state(scoped(1)).gap && !index.read_state(scoped(2)).gap);
        assert_eq!(index.stats().bytes, 2 * ENTRY_BYTES);
    });
}

#[test]
fn the_bound_drops_the_oldest_frame_and_opens_a_gap_for_its_projects_only() {
    once(|| {
        let Rig {
            clock,
            index,
            mut writer,
            ..
        } = rig(2);
        (1..=3).for_each(|lsn| writer.apply(spans(lsn)));
        // LSN 1 (project 2) was dropped; project 1 lost nothing.
        assert!(!holds(&index, 1) && holds(&index, 2) && holds(&index, 3));
        assert!(index.read_state(scoped(2)).gap);
        assert!(!index.read_state(scoped(1)).gap);

        // The watermark passing the gap does not close it; the grace does.
        writer.apply(Frame::Checkpoint {
            lsn: Lsn::new(4),
            last_lsn: Lsn::new(1),
        });
        assert!(index.read_state(scoped(2)).gap);
        clock.advance(GRACE);
        writer.maintain();
        assert!(!index.read_state(scoped(2)).gap);

        // An entry that is dropped after the store serves it leaves no gap.
        writer.apply(Frame::Checkpoint {
            lsn: Lsn::new(5),
            last_lsn: Lsn::new(3),
        });
        clock.advance(GRACE);
        writer.apply(spans(6));
        assert_eq!(index.stats().entries, 1);
        assert!(!index.read_state(scoped(1)).gap && !index.read_state(scoped(2)).gap);
    });
}

#[test]
fn lists_tail_and_bounds_read_across_shards_inside_one_project() {
    once(|| {
        let Rig {
            index, mut writer, ..
        } = rig(100);
        (1..=40).for_each(|lsn| writer.apply(spans(lsn)));
        let window = |from: i64, to: i64| {
            TimeWindow::new(Timestamp::from_micros(from), Timestamp::from_micros(to)).unwrap()
        };
        let lsns =
            |spans: &[RecentSpan]| -> BTreeSet<u64> { spans.iter().map(|s| s.lsn.get()).collect() };

        let (heads, _) = index.heads(scoped(1), window(0, 10_000));
        assert_eq!(lsns(&heads), (2..=40).step_by(2).collect());
        let (heads, _) = index.heads(scoped(1), window(1_010, 1_021));
        assert_eq!(lsns(&heads), BTreeSet::from([10, 12, 14, 16, 18, 20]));

        let (tail, _) = index.since(scoped(2), Lsn::new(33));
        let order: Vec<u64> = tail.iter().map(|s| s.lsn.get()).collect();
        assert_eq!(order, [35, 37, 39]);
        assert!(index.since(scoped(2), Lsn::new(40)).0.is_empty());

        let at = Timestamp::from_micros;
        assert_eq!(index.time_bounds(scoped(1)).0, Some((at(1_002), at(1_040))));
        assert_eq!(index.time_bounds(scoped(2)).0, Some((at(1_001), at(1_039))));
        assert_eq!(index.time_bounds(scoped(3)).0, None);

        let (several, _) = index.traces(scoped(1), &[trace_of(2), trace_of(3), trace_of(4)]);
        assert_eq!(lsns(&several), BTreeSet::from([2, 4]));
    });
}

#[test]
fn a_quarantined_pair_is_reported_until_it_is_requeued() {
    once(|| {
        let Rig {
            clock,
            index,
            mut writer,
            ..
        } = rig(100);
        let day = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
        writer.apply(spans(1));
        writer.apply(Frame::Quarantine {
            lsn: Lsn::new(2),
            pairs: vec![QuarantinedPair {
                batch_seq: Lsn::new(1),
                table: TableId::SpanCore,
                partition: day,
                reach: QuarantineReach::Projects(vec![project(2)]),
            }],
        });
        // The pair counts as resolved, so the checkpoint passes it and the entry leaves the index while the
        // span is absent from the store. The read still says so.
        writer.apply(Frame::Checkpoint {
            lsn: Lsn::new(3),
            last_lsn: Lsn::new(2),
        });
        clock.advance(GRACE);
        writer.maintain();
        let (found, state) = index.trace(scoped(2), trace_of(1));
        assert!(found.is_empty() && !state.gap);
        assert!(state.quarantined(&[TableId::SpanCore]));
        assert!(!state.quarantined(&[TableId::TraceHeads, TableId::TraceStats]));
        assert!(
            !index
                .read_state(scoped(1))
                .quarantined(&[TableId::SpanCore])
        );

        writer.requeued(Lsn::new(1), TableId::SpanCore, day);
        assert!(
            !index
                .read_state(scoped(2))
                .quarantined(&[TableId::SpanCore])
        );
    });
}

#[test]
fn a_tombstone_frame_removes_matching_entries() {
    once(|| {
        let Rig {
            index, mut writer, ..
        } = rig(100);
        (1..=4).for_each(|lsn| writer.apply(spans(lsn)));
        writer.apply(Frame::Tombstone {
            lsn: Lsn::new(5),
            project_id: project(1),
            requested_at: WALL.saturating_add_micros(1),
            selector: DeletionSelector::Traces(vec![trace_of(2), trace_of(3)]),
        });
        // Trace 3 belongs to project 2, so the tombstone of project 1 does not reach it.
        assert!(holds(&index, 1) && !holds(&index, 2) && holds(&index, 3) && holds(&index, 4));
        assert_eq!(index.applied(), Lsn::new(5));
        assert_eq!(index.stats().bytes, 3 * ENTRY_BYTES);
    });
}

#[test]
fn a_rebuild_from_the_replay_answers_like_the_index_before_the_restart() {
    once(|| {
        let Rig {
            clock,
            wal,
            index,
            mut writer,
        } = rig(100);
        let checkpoint = Frame::Checkpoint {
            lsn: Lsn::new(5),
            last_lsn: Lsn::new(2),
        };
        (1..=4).for_each(|lsn| writer.apply(spans(lsn)));
        writer.apply(checkpoint.clone());
        writer.apply(spans(6));
        let before: Vec<_> = (1..=6)
            .map(|lsn| index.trace(scoped(project_of(lsn)), trace_of(lsn)))
            .collect();
        drop((index, writer));

        // The replay: every frame above the watermark, plus LSN 2, which is below it and younger than the grace.
        let replay = vec![spans(2), spans(3), spans(4), checkpoint, spans(6)];
        let (index, mut writer) = RecentIndex::rebuild(
            RecentConfig::new(100 * ENTRY_BYTES, StoreFlavour::SingleNode),
            Arc::new(clock.clone()),
            Arc::new(wal.clone()),
            Lsn::new(2),
            replay,
        );
        assert_eq!(index.applied(), Lsn::new(6));
        for lsn in 2..=6 {
            let after = index.trace(scoped(project_of(lsn)), trace_of(lsn));
            assert_eq!(after, before[lsn as usize - 1], "lsn {lsn}");
        }
        // LSN 1 was not replayed: it is below the watermark and older than the grace, so the store serves it.
        assert!(!holds(&index, 1) && !index.read_state(scoped(2)).gap);
        // The grace of the recovered checkpoint restarts at the rebuild.
        clock.advance(GRACE - Duration::from_nanos(1));
        writer.maintain();
        assert!(holds(&index, 2));
        clock.advance(Duration::from_nanos(1));
        writer.maintain();
        assert!(!holds(&index, 2) && holds(&index, 3));
        // A rebuild of an empty log is caught up at the watermark.
        let (empty, _) = RecentIndex::rebuild(
            *index.config(),
            Arc::new(clock.clone()),
            Arc::new(wal.clone()),
            Lsn::new(9),
            Vec::new(),
        );
        assert_eq!(empty.applied(), Lsn::new(9));
    });
}

#[test]
fn the_payload_of_a_recent_span_is_read_through_its_locator() {
    once(|| {
        let Rig {
            wal,
            index,
            mut writer,
            ..
        } = rig(100);
        writer.apply(spans(1));
        writer.apply(spans(3));
        let entry = entry(1);
        let record = span_record(&entry, Lsn::new(1), WALL);
        assert_eq!(SpanSummary::of(&record.core), entry.summary);
        wal.put(entry.locator, record.clone());
        let (trace, id) = (entry.summary.trace_id, entry.summary.span_id);
        let read =
            |scope, trace, id| shuttle::future::block_on(index.span_record(scope, trace, id));
        assert_eq!(read(scoped(2), trace, id), Ok(Some(record.clone())));
        // Not in the index: nothing to re-read. Another project never reaches the frame.
        assert_eq!(read(scoped(2), trace_of(9), id), Ok(None));
        assert_eq!(read(scoped(1), trace, id), Ok(None));
        // The WAL no longer has the frame of LSN 3, and a frame that holds another span is not the span.
        let other = self::entry(3).summary;
        assert_eq!(
            read(scoped(2), other.trace_id, other.span_id),
            Err(WalReadError::Gone)
        );
        wal.put(self::entry(3).locator, record.clone());
        assert_eq!(
            read(scoped(2), other.trace_id, other.span_id),
            Err(WalReadError::Gone)
        );
    });
}

#[test]
fn the_defaults_are_the_contract_values() {
    let single = RecentConfig::new(1, StoreFlavour::SingleNode);
    assert_eq!(single.visibility_grace, Duration::from_secs(2));
    assert_eq!(single.read_barrier, Duration::from_millis(250));
    for flavour in [StoreFlavour::Clustered, StoreFlavour::Cloud] {
        assert_eq!(
            RecentConfig::visibility_grace(flavour),
            Duration::from_secs(10)
        );
    }
    assert_eq!(RecentConfig::default_max_bytes(4 << 30), (4 << 30) / 10);
    assert_eq!(RecentConfig::default_max_bytes(64 << 30), 1 << 30);
    // "About 200 bytes" per entry, before the maps that index it.
    assert!(std::mem::size_of::<crate::entry::Entry>() <= 200);
    const { assert!(ENTRY_BYTES <= 400) };
}
