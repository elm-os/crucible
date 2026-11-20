//! Directed tests of `crucible-wal` recovery (contract section 4.6) after a process stop that left directory
//! entries readable and not durable: an entry whose `sync_dir` failed, or whose bytes failed their sync before
//! it. The next start finds such an entry and builds on it, and a crash during that start keeps any subset of
//! the pending entries (`SimDisk`). Whatever the crash keeps, the start after it must open the log with every
//! acknowledged frame.
//!
//! Each case crashes the start that follows the stop at every one of its disk operations, on several disks,
//! because each disk draws what a crash keeps from its own stream.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use futures::FutureExt;

use crucible_env::{DiskPath, Root};
use crucible_records::Lsn;
use crucible_sim::{Host, Sim, SimDisk, run};
use crucible_wal::{AppendError, RecoveryError, RecoveryReport, Wal, WalConfig};
use crucible_wal_codec::{
    Checkpoint, Compression, FRAME_HEADER_LEN, PreparedFrame, Record, RecordType,
};

/// Four frames of 240 bytes fill a segment of 1,024 bytes.
const SEGMENT: u64 = 1024;
const FRAME: usize = 240;
/// The disks tried at each crash point.
const DISKS: u64 = 32;

fn config() -> WalConfig {
    WalConfig {
        segment_bytes: SEGMENT,
        max_bytes: SEGMENT * 1_000,
        min_free_bytes: 0,
        replay_retention: Duration::ZERO,
        ..WalConfig::default()
    }
}

fn wal_dir() -> DiskPath {
    DiskPath::root_dir(Root::Wal)
}

fn segment(segment_no: u64) -> String {
    format!("seg-{segment_no:020}.wal")
}

fn data(tag: u8) -> PreparedFrame {
    let payload = vec![tag; FRAME - FRAME_HEADER_LEN];
    PreparedFrame::new(RecordType::Noop, payload, Compression::None).unwrap()
}

/// A running WAL on a simulated host.
struct Node {
    host: Host,
    wal: Wal,
    report: RecoveryReport,
}

impl Node {
    /// Stops the process; the disk keeps everything, synced or not.
    async fn stop(self) {
        drop(self.wal);
        self.host.stop().await;
    }

    async fn fill(&self, tags: std::ops::RangeInclusive<u8>) {
        for tag in tags {
            self.wal.append(data(tag)).await.unwrap();
        }
    }
}

async fn try_start(sim: &Sim, disk: &SimDisk, name: &str) -> Result<Node, RecoveryError> {
    let host = sim.host(name, disk, 0);
    match Wal::open(&host.env, config()).await {
        Ok((wal, report)) => Ok(Node { host, wal, report }),
        Err(e) => {
            host.stop().await;
            Err(e)
        }
    }
}

async fn start(sim: &Sim, disk: &SimDisk, name: &str) -> Node {
    try_start(sim, disk, name).await.unwrap()
}

/// Frames 1 to 8 in segments 1 and 2, and a checkpoint of LSN 4 at LSN 9, in segment 3.
async fn checkpointed(sim: &Sim, disk: &SimDisk) -> Node {
    let node = start(sim, disk, "build").await;
    node.fill(1..=8).await;
    let checkpoint = Record::Checkpoint(Checkpoint {
        batch_seq: 1,
        last_lsn: Lsn::new(4),
        table_bitmap: Checkpoint::ALL_TABLES,
    });
    let frame = PreparedFrame::new(
        checkpoint.record_type(),
        checkpoint.encode().unwrap(),
        Compression::Lz4,
    )
    .unwrap();
    assert_eq!(node.wal.append(frame).await, Ok(Lsn::new(9)));
    node
}

/// Starts the log with a crash armed at its `at`-th disk operation and crashes the host there. False when the
/// start finished first.
async fn crash_at(sim: &Sim, disk: &SimDisk, name: &str, at: u64) -> bool {
    let host = sim.host(name, disk, 0);
    disk.arm_crash_after_ops(at);
    let reached = {
        let mut open = Box::pin(Wal::open(&host.env, config())).fuse();
        let mut crash_point = Box::pin(disk.crash_point_reached()).fuse();
        futures::select_biased! {
            _opened = open => false,
            () = crash_point => true,
        }
    };
    disk.disarm_crash();
    host.crash().await;
    reached
}

/// The trials of a sweep, and for each name the trials whose crash kept it.
struct Kept {
    trials: u64,
    names: BTreeMap<String, u64>,
}

impl Kept {
    fn count(&self, name: &str) -> u64 {
        self.names.get(name).copied().unwrap_or(0)
    }

    /// Some crashes kept `name` and others lost it.
    fn sometimes(&self, name: &str) -> bool {
        (1..self.trials).contains(&self.count(name))
    }
}

/// Builds a stopped directory with `stopped` on a fresh disk, crashes the next start at one of its disk
/// operations, starts once more and passes the trial, the report and the names the crash kept to `check`.
/// Every operation of that start is tried, on [`DISKS`] disks each.
async fn sweep<S, F>(
    sim: &Sim,
    label: &str,
    stopped: S,
    check: impl Fn(&str, &RecoveryReport, &[String]),
) -> Kept
where
    S: Fn(SimDisk) -> F,
    F: Future<Output = ()>,
{
    // The disk operations of that start, counted on a copy.
    let disk = sim.disk(&format!("{label}/count"));
    stopped(disk.clone()).await;
    let copy = disk.fork(sim.rng(&format!("{label}/copy")).as_ref());
    let before = copy.ops();
    start(sim, &copy, &format!("{label}/copy"))
        .await
        .stop()
        .await;
    let ops = copy.ops() - before;

    let mut kept = Kept {
        trials: 0,
        names: BTreeMap::new(),
    };
    for at in 1..=ops {
        for n in 0..DISKS {
            let trial = format!("{label}/{at}/{n}");
            let disk = sim.disk(&trial);
            stopped(disk.clone()).await;
            let crashed = crash_at(sim, &disk, &format!("{trial}/crash"), at).await;
            assert!(crashed, "{trial}: the start finished before operation {at}");
            let names = disk.names(&wal_dir());
            kept.trials += 1;
            for name in &names {
                *kept.names.entry(name.clone()).or_default() += 1;
            }
            let node = match try_start(sim, &disk, &format!("{trial}/after")).await {
                Ok(node) => node,
                Err(e) => panic!(
                    "{trial}: after a crash at operation {at} of {ops} the log does not open: {e:?}"
                ),
            };
            check(&trial, &node.report, &names);
            node.stop().await;
        }
    }
    kept
}

/// The first start fails before the entry of `EPOCH` is durable and the process stops, so the entry is readable
/// and pending. The next start finds a valid `EPOCH` and creates segment 1. Recovery must make `EPOCH` durable,
/// its directory entry included, before it creates the segment: a crash that kept the segment's entry and lost
/// `EPOCH`'s would make every later start refuse the directory (`wal_epoch`).
#[test]
fn a_readable_epoch_is_durable_before_the_first_segment_is_created() {
    run(1, |sim| async move {
        let sim = &sim;
        let first_start_fails = move |disk: SimDisk, fault: fn(&SimDisk)| async move {
            fault(&disk);
            let error = try_start(sim, &disk, "first").await.err();
            assert!(matches!(error, Some(RecoveryError::Disk(_))), "{error:?}");
            disk.fail_dir_syncs(false);
            assert_eq!(disk.names(&wal_dir()), ["EPOCH"]);
        };
        // The directory sync fails, or the sync of the entry's bytes before it.
        let faults: [(&str, fn(&SimDisk)); 2] = [
            ("dir-sync", |disk| disk.fail_dir_syncs(true)),
            ("sync", |disk| disk.fail_next_syncs(1)),
        ];
        for (label, fault) in faults {
            let kept = sweep(
                sim,
                label,
                |disk| first_start_fails(disk, fault),
                |trial, report, names| {
                    // A segment that survived the crash kept the `EPOCH` it was created under.
                    if names.contains(&segment(1)) {
                        assert!(!report.created, "{trial}: {names:?}");
                    }
                },
            )
            .await;
            assert!(
                kept.count(&segment(1)) > 0,
                "{label}: no crash kept segment 1"
            );
        }
    });
}

/// The other entries recovery relies on, each left pending by a failed directory sync and a process stop: the
/// successor of a segment, a removal by garbage collection, and the `CHECKPOINT` hint. Recovery trusts none of
/// them in a way a crash can break: whatever a crash of the next start keeps, the log opens with every
/// acknowledged frame and the watermark.
#[test]
fn other_pending_entries_leave_a_log_that_opens_whatever_a_crash_keeps() {
    run(2, |sim| async move {
        let sim = &sim;

        // A failed group whose rotation then fails at its directory sync: segment 2, whose first LSN supersedes
        // the unacknowledged frame 3 of segment 1, has a pending entry.
        let kept = sweep(
            sim,
            "successor",
            move |disk| async move {
                let node = start(sim, &disk, "build").await;
                node.fill(1..=2).await;
                disk.fail_next_syncs(1);
                disk.fail_dir_syncs(true);
                assert_eq!(
                    node.wal.append(data(3)).await,
                    Err(AppendError::Unavailable)
                );
                assert!(node.wal.admission().failed);
                disk.fail_dir_syncs(false);
                node.stop().await;
            },
            |trial, report, _| assert_eq!(report.last_lsn, Lsn::new(2), "{trial}"),
        )
        .await;
        assert!(kept.sometimes(&segment(2)));

        let checkpointed_log = |trial: &str, report: &RecoveryReport, _: &[String]| {
            assert_eq!(report.last_lsn, Lsn::new(9), "{trial}");
            assert_eq!(report.watermark_lsn(), Lsn::new(4), "{trial}");
        };

        // A collection removes segment 1, whose frames are all at or below the watermark, and its directory
        // sync fails. The collection before it wrote the hint and removed nothing: the recent-span index still
        // needed LSN 1.
        let kept = sweep(
            sim,
            "removal",
            move |disk| async move {
                let node = checkpointed(sim, &disk).await;
                let collected = node.wal.gc(Some(Lsn::new(1))).await.unwrap();
                assert!(collected.removed.is_empty());
                disk.fail_dir_syncs(true);
                assert!(node.wal.gc(None).await.is_err());
                disk.fail_dir_syncs(false);
                assert!(!disk.names(&wal_dir()).contains(&segment(1)));
                node.stop().await;
            },
            checkpointed_log,
        )
        .await;
        assert!(kept.sometimes(&segment(1)));

        // A collection writes the hint and its directory sync fails: the rename over `CHECKPOINT` is pending.
        let kept = sweep(
            sim,
            "hint",
            move |disk| async move {
                let node = checkpointed(sim, &disk).await;
                disk.fail_dir_syncs(true);
                assert!(node.wal.gc(None).await.is_err());
                disk.fail_dir_syncs(false);
                assert!(disk.names(&wal_dir()).contains(&"CHECKPOINT".to_owned()));
                node.stop().await;
            },
            checkpointed_log,
        )
        .await;
        assert!(kept.sometimes("CHECKPOINT"));
    });
}
