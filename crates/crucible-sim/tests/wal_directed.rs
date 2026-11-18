//! Directed tests of `crucible-wal` on a fault-free `SimDisk`: recovery (contract section 4.6), group
//! commit and the failure path (4.5), garbage collection and replay retention (4.7), the admission signals
//! (4.8) and replay.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use bytes::Bytes;
use crucible_env::metrics::registry;
use crucible_env::{Disk, DiskPath, OpenMode, Root};
use crucible_records::{Lsn, Timestamp};
use crucible_sim::{DiskFaults, Host, SIM_EPOCH_MICROS, Sim, SimDisk, run};
use crucible_wal::{
    AppendError, Durability, RecoveryError, RecoveryReport, ReplayError, SegmentHeaderIssue, Wal,
    WalConfig, WalFrame, WalGap,
};
use crucible_wal_codec::{
    BatchSeal, Checkpoint, CheckpointHint, Compression, FRAME_HEADER_LEN, FrameHeaderError,
    PreparedFrame, Record, RecordType, SEGMENT_HEADER_LEN, Scan, SegmentHeader, scan_frame,
};

/// The segment size of most tests.
const SEGMENT: u64 = 1024;
/// Encoded length of the frames these tests write: four of them fill a 1,024-byte segment exactly.
const FRAME: usize = 240;
const HEADER: usize = SEGMENT_HEADER_LEN;

fn config(segment_bytes: u64) -> WalConfig {
    WalConfig {
        segment_bytes,
        max_bytes: segment_bytes * 1_000,
        min_free_bytes: 0,
        ..WalConfig::default()
    }
}

fn data(tag: u8) -> PreparedFrame {
    let payload = vec![tag; FRAME - FRAME_HEADER_LEN];
    PreparedFrame::new(RecordType::Noop, payload, Compression::None).unwrap()
}

fn control(record: &Record<'_>) -> PreparedFrame {
    PreparedFrame::new(
        record.record_type(),
        record.encode().unwrap(),
        Compression::Lz4,
    )
    .unwrap()
}

fn seal(first: u64, last: u64) -> BatchSeal {
    BatchSeal {
        first_lsn: Lsn::new(first),
        last_lsn: Lsn::new(last),
        frame_count: (last - first + 1) as u32,
        normalizer_version: 1,
        price_table_version: 1,
        deletion_seq: 0,
    }
}

fn checkpoint(last: u64) -> Checkpoint {
    Checkpoint {
        batch_seq: 1,
        last_lsn: Lsn::new(last),
        table_bitmap: Checkpoint::ALL_TABLES,
    }
}

fn entry(name: &str) -> DiskPath {
    DiskPath::new(Root::Wal, name).unwrap()
}

fn seg(segment_no: u64) -> DiskPath {
    entry(&format!("seg-{segment_no:020}.wal"))
}

fn wal_dir() -> DiskPath {
    DiskPath::root_dir(Root::Wal)
}

/// A running WAL on a simulated host.
struct Node {
    host: Host,
    wal: Wal,
    report: RecoveryReport,
}

async fn try_start(
    sim: &Sim,
    disk: &SimDisk,
    name: &str,
    config: &WalConfig,
) -> Result<Node, RecoveryError> {
    let host = sim.host(name, disk, 0);
    match Wal::open(&host.env, config.clone()).await {
        Ok((wal, report)) => Ok(Node { host, wal, report }),
        Err(e) => {
            host.stop().await;
            Err(e)
        }
    }
}

async fn start(sim: &Sim, disk: &SimDisk, name: &str, config: &WalConfig) -> Node {
    try_start(sim, disk, name, config).await.unwrap()
}

impl Node {
    /// Stops the process; the disk keeps everything, synced or not.
    async fn stop(self) {
        drop(self.wal);
        self.host.stop().await;
    }

    async fn crash(self) {
        drop(self.wal);
        self.host.crash().await;
    }

    /// Appends one data frame per tag and returns the LSNs.
    async fn fill(&self, tags: std::ops::RangeInclusive<u8>) -> Vec<u64> {
        let mut lsns = Vec::new();
        for tag in tags {
            lsns.push(self.wal.append(data(tag)).await.unwrap().get());
        }
        lsns
    }

    async fn append(&self, record: &Record<'_>) -> u64 {
        self.wal.append(control(record)).await.unwrap().get()
    }

    async fn frames(&self, from: u64) -> Vec<WalFrame> {
        let mut replay = self.wal.replay(Lsn::new(from));
        let mut frames = Vec::new();
        while let Some(frame) = replay.next().await.unwrap() {
            frames.push(frame);
        }
        frames
    }

    /// The first payload byte of every frame from LSN 1: the tag of a data frame.
    async fn tags(&self) -> Vec<u8> {
        let frames = self.frames(1).await;
        frames.iter().map(|f| f.stored_payload()[0]).collect()
    }

    fn syncs(&self) -> u64 {
        self.host
            .recorder
            .histogram_value(&registry::WAL_SYNC_DURATION, &[])
            .map_or(0, |h| h.count)
    }
}

fn segment_files(disk: &SimDisk) -> Vec<(String, Vec<u8>)> {
    disk.names(&wal_dir())
        .into_iter()
        .map(|name| {
            let bytes = disk.read_file(&entry(&name)).unwrap();
            (name, bytes)
        })
        .collect()
}

/// CRC32C, bit by bit, to forge frames that only a checksum-valid header can express.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82F6_3B78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Three segments of four frames each: LSN 1 to 12, tags equal to LSNs.
async fn three_segments(sim: &Sim, disk: &SimDisk) {
    three_segments_on(sim, disk, "build").await;
}

async fn three_segments_on(sim: &Sim, disk: &SimDisk, host: &str) {
    let node = start(sim, disk, host, &config(SEGMENT)).await;
    assert_eq!(node.fill(1..=12).await, (1..=12).collect::<Vec<u64>>());
    assert_eq!(node.wal.segments().len(), 3);
    node.stop().await;
}

#[test]
fn appends_resolve_in_lsn_order_and_survive_a_crash() {
    run(1, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "a", &config(SEGMENT)).await;
        assert!(node.report.created);
        assert_eq!(node.report.last_lsn, Lsn::ZERO);
        assert_eq!(node.fill(1..=10).await, (1..=10).collect::<Vec<u64>>());
        assert_eq!(node.wal.durable_lsn(), Lsn::new(10));
        let epoch = node.wal.epoch();

        let frames = node.frames(7).await;
        assert_eq!(
            frames.iter().map(|f| f.lsn().get()).collect::<Vec<_>>(),
            [7, 8, 9, 10]
        );
        // The locator of a frame: its segment and offset.
        assert_eq!((frames[0].segment_no(), frames[0].offset()), (2, 544));
        assert_eq!((frames[2].segment_no(), frames[2].offset()), (3, 64));
        for frame in &frames {
            assert_eq!(frame.record_type(), RecordType::Noop);
            assert!(frame.received_at() >= Timestamp::from_micros(SIM_EPOCH_MICROS));
            assert_eq!(frame.payload().unwrap().len(), FRAME - FRAME_HEADER_LEN);
        }
        node.crash().await;

        let node = start(&sim, &disk, "b", &config(SEGMENT)).await;
        assert!(!node.report.created);
        assert_eq!(node.report.epoch, epoch);
        assert_eq!(node.report.last_lsn, Lsn::new(10));
        assert_eq!(node.report.tail_repair, None);
        assert_eq!(node.report.unresolved_range(), (Lsn::new(1), Lsn::new(10)));
        assert_eq!(node.tags().await, (1..=10).collect::<Vec<u8>>());
        assert_eq!(node.fill(11..=11).await, [11]);
        node.stop().await;
    });
}

#[test]
fn a_torn_tail_at_every_byte_offset_of_the_final_frame_is_truncated() {
    run(2, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "build", &config(SEGMENT)).await;
        node.fill(1..=3).await;
        node.stop().await;
        let start_of_last = HEADER + 2 * FRAME;
        let end_of_last = HEADER + 3 * FRAME;

        for fill in [0u8, 0xA5] {
            for cut in start_of_last..end_of_last {
                // The final frame is on disk up to `cut`; the rest never arrived.
                let name = format!("cut-{fill}-{cut}");
                let copy = disk.fork(sim.rng(&name).as_ref());
                assert!(copy.patch_file(&seg(1), cut, &vec![fill; end_of_last - cut]));

                let node = start(&sim, &copy, &name, &config(SEGMENT)).await;
                assert_eq!(node.report.last_lsn, Lsn::new(2), "{name}");
                let repaired = node.report.tail_repair.map(|r| (r.segment_no, r.offset));
                if cut == start_of_last && fill == 0 {
                    // Zero bytes where the frame's magic was: a clean end, nothing to repair.
                    assert_eq!(repaired, None, "{name}");
                } else {
                    assert_eq!(repaired, Some((1, start_of_last as u64)), "{name}");
                }
                let bytes = copy.read_file(&seg(1)).unwrap();
                assert_eq!(bytes.len() as u64, SEGMENT);
                assert!(bytes[start_of_last..].iter().all(|&b| b == 0), "{name}");
                // The LSN of the torn frame is assigned again.
                assert_eq!(node.fill(9..=9).await, [3], "{name}");
                node.crash().await;

                let node = start(&sim, &copy, &format!("{name}-again"), &config(SEGMENT)).await;
                assert_eq!(node.report.tail_repair, None, "{name}");
                assert_eq!(node.tags().await, [1, 2, 9], "{name}");
                node.stop().await;
            }
        }
    });
}

#[test]
fn stale_frames_after_a_clean_end_are_zeroed() {
    run(3, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "build", &config(SEGMENT)).await;
        node.fill(1..=3).await;
        node.stop().await;
        // A crash lost the block of frame 2 and kept the block of frame 3.
        assert!(disk.patch_file(&seg(1), HEADER + FRAME, &[0; FRAME]));

        let node = start(&sim, &disk, "a", &config(SEGMENT)).await;
        assert_eq!(node.report.last_lsn, Lsn::new(1));
        let repair = node.report.tail_repair.unwrap();
        assert_eq!(repair.offset, (HEADER + FRAME) as u64);
        let bytes = disk.read_file(&seg(1)).unwrap();
        assert!(bytes[HEADER + FRAME..].iter().all(|&b| b == 0));
        // Without the repair this frame would be followed by the old frame 3, with the right LSN.
        assert_eq!(node.fill(7..=7).await, [2]);
        node.crash().await;
        let node = start(&sim, &disk, "b", &config(SEGMENT)).await;
        assert_eq!(node.tags().await, [1, 7]);
        node.stop().await;
    });
}

/// Asserts that recovery refuses the directory with a fatal error and changes no byte of it.
async fn assert_fatal(sim: &Sim, disk: &SimDisk, name: &str) -> RecoveryError {
    let before = segment_files(disk);
    let Err(error) = try_start(sim, disk, name, &config(SEGMENT)).await else {
        panic!("{name}: recovery accepted a damaged directory");
    };
    assert!(error.is_fatal(), "{name}: {error}");
    assert_eq!(
        segment_files(disk),
        before,
        "{name}: recovery changed the directory"
    );
    error
}

#[test]
fn an_lsn_gap_is_fatal_and_nothing_is_truncated() {
    run(4, |sim| async move {
        // A missing segment number.
        let disk = sim.disk("missing");
        three_segments(&sim, &disk).await;
        disk.remove(&seg(2)).await.unwrap();
        disk.sync_dir(&wal_dir()).await.unwrap();
        let error = assert_fatal(&sim, &disk, "missing").await;
        assert_eq!(
            error,
            RecoveryError::Gap(WalGap::MissingSegment {
                expected: 2,
                found: 3
            })
        );
        assert_eq!(error.code(), "wal_gap");

        // An earlier segment whose valid prefix ends below the next segment's first LSN minus 1.
        let disk = sim.disk("short");
        three_segments(&sim, &disk).await;
        assert!(disk.patch_file(&seg(1), HEADER + 3 * FRAME, &[0; FRAME]));
        assert_eq!(
            assert_fatal(&sim, &disk, "short").await,
            RecoveryError::Gap(WalGap::ShortSegment {
                segment_no: 1,
                valid_through: Lsn::new(3),
                next_first_lsn: Lsn::new(5),
            })
        );

        // An LSN inside a segment that is not its predecessor plus 1.
        let disk = sim.disk("jump");
        three_segments(&sim, &disk).await;
        let mut forged = Vec::new();
        data(11).write_to(Lsn::new(13), Timestamp::from_micros(0), &mut forged);
        assert!(disk.patch_file(&seg(3), HEADER + 2 * FRAME, &forged));
        assert_eq!(
            assert_fatal(&sim, &disk, "jump").await,
            RecoveryError::Gap(WalGap::LsnMismatch {
                segment_no: 3,
                offset: (HEADER + 2 * FRAME) as u64,
                expected: Lsn::new(11),
                found: Lsn::new(13),
            })
        );

        // The oldest segment starts above the watermark plus 1: unresolved frames were removed.
        let disk = sim.disk("prefix");
        three_segments(&sim, &disk).await;
        disk.remove(&seg(1)).await.unwrap();
        disk.sync_dir(&wal_dir()).await.unwrap();
        assert_eq!(
            assert_fatal(&sim, &disk, "prefix").await,
            RecoveryError::Gap(WalGap::MissingPrefix {
                oldest_first_lsn: Lsn::new(5),
                watermark: Lsn::ZERO,
            })
        );
    });
}

#[test]
fn a_corrupt_frame_is_fatal_and_nothing_is_truncated() {
    run(5, |sim| async move {
        // In the last segment a torn frame would be truncated; a frame with valid checksums and an
        // unknown record type must not be. The same holds in an earlier segment.
        for (segment_no, slot) in [(3, 3), (3, 0), (1, 1)] {
            let name = format!("corrupt-{segment_no}-{slot}");
            let disk = sim.disk(&name);
            three_segments(&sim, &disk).await;
            let offset = HEADER + slot * FRAME;
            let mut header =
                disk.read_file(&seg(segment_no)).unwrap()[offset..offset + 36].to_vec();
            header[5] = 99;
            let crc = crc32c(&header[..32]);
            header[32..].copy_from_slice(&crc.to_le_bytes());
            assert!(disk.patch_file(&seg(segment_no), offset, &header));

            let error = assert_fatal(&sim, &disk, &name).await;
            assert_eq!(
                error,
                RecoveryError::CorruptFrame {
                    segment_no,
                    offset: offset as u64,
                    error: FrameHeaderError::UnknownRecordType(99),
                }
            );
            assert_eq!(error.code(), "wal_corrupt");
        }
    });
}

#[test]
fn superseded_frames_after_a_failed_group_are_ignored() {
    run(6, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "a", &config(SEGMENT)).await;
        node.fill(1..=3).await;

        // The sync of the next group fails: its waiter fails, the LSN is assigned again, and the
        // committer moves to a new segment whose first LSN supersedes what the group left behind.
        disk.fail_next_syncs(1);
        assert_eq!(
            node.wal.append(data(90)).await,
            Err(AppendError::Unavailable)
        );
        assert_eq!(node.wal.durable_lsn(), Lsn::new(3));
        assert_eq!(node.fill(4..=4).await, [4]);
        let first_lsns =
            |wal: &Wal| -> Vec<u64> { wal.segments().iter().map(|s| s.first_lsn.get()).collect() };
        assert_eq!(first_lsns(&node.wal), [1, 4]);
        // The failed frame is still in segment 1, valid, with LSN 4.
        let segment = disk.read_file(&seg(1)).unwrap();
        let Scan::Frame(left_behind) = scan_frame(&segment, HEADER + 3 * FRAME) else {
            panic!("the failed group left no frame");
        };
        assert_eq!(
            (left_behind.lsn(), left_behind.stored_payload()[0]),
            (Lsn::new(4), 90)
        );
        assert_eq!(node.tags().await, [1, 2, 3, 4]);

        // A failed write, then a failed first group of a fresh segment: that segment is abandoned empty
        // and shares its first LSN with its successor.
        disk.fail_next_writes(1);
        assert_eq!(
            node.wal.append(data(91)).await,
            Err(AppendError::Unavailable)
        );
        disk.fail_next_syncs(1);
        assert_eq!(
            node.wal.append(data(92)).await,
            Err(AppendError::Unavailable)
        );
        assert_eq!(node.fill(5..=5).await, [5]);
        assert_eq!(first_lsns(&node.wal), [1, 4, 5, 5]);
        assert_eq!(
            node.host
                .recorder
                .counter_value(&registry::WAL_GROUP_FAILURES, &[]),
            3
        );
        assert!(!node.wal.admission().failed);
        // The process stops without a crash, so the superseded frames are still on disk.
        node.stop().await;

        let node = start(&sim, &disk, "b", &config(SEGMENT)).await;
        assert_eq!(node.report.last_lsn, Lsn::new(5));
        assert_eq!(first_lsns(&node.wal), [1, 4, 5, 5]);
        assert_eq!(node.tags().await, [1, 2, 3, 4, 5]);
        node.crash().await;
        let node = start(&sim, &disk, "c", &config(SEGMENT)).await;
        assert_eq!(node.tags().await, [1, 2, 3, 4, 5]);
        node.stop().await;
    });
}

#[test]
fn a_failed_group_whose_rotation_also_fails_puts_the_wal_in_the_failed_state() {
    run(7, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "a", &config(SEGMENT)).await;
        node.fill(1..=2).await;
        // The group's sync fails, and so does the sync of the next segment's header.
        disk.fail_next_syncs(2);
        assert_eq!(
            node.wal.append(data(90)).await,
            Err(AppendError::Unavailable)
        );
        assert!(node.wal.admission().failed);
        assert!(!node.wal.admission().accepting());
        assert_eq!(node.wal.append(data(91)).await, Err(AppendError::Failed));
        assert_eq!(
            node.host.recorder.gauge_value(&registry::WAL_FAILED, &[]),
            Some(1)
        );
        node.crash().await;

        // Until restart: the next process recovers everything that was acknowledged.
        let node = start(&sim, &disk, "b", &config(SEGMENT)).await;
        assert!(!node.wal.admission().failed);
        assert_eq!(node.report.last_lsn, Lsn::new(2));
        assert_eq!(node.fill(3..=3).await, [3]);
        assert_eq!(node.tags().await, [1, 2, 3]);
        node.stop().await;
    });
}

#[test]
fn a_missing_or_invalid_checkpoint_hint_only_lengthens_recovery() {
    run(8, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "build", &config(SEGMENT)).await;
        node.fill(1..=12).await;
        let sealed_a = seal(1, 4);
        let sealed_b = seal(5, 12);
        assert_eq!(node.append(&Record::BatchSeal(sealed_a)).await, 13);
        node.fill(14..=21).await;
        assert_eq!(node.append(&Record::BatchSeal(sealed_b)).await, 22);
        node.fill(23..=26).await;
        // Batch A is resolved, batch B is sealed and unchecked. The checkpoint frame lands several
        // segments after the first unresolved frame (LSN 5, in segment 2).
        assert_eq!(node.append(&Record::Checkpoint(checkpoint(4))).await, 27);
        let watermark = node.wal.watermark().unwrap();
        assert_eq!(watermark.last_lsn(), Lsn::new(4));
        assert!(watermark.segment_no > 2);
        // Replay retention keeps segment 1; the collection writes the hint.
        let collected = node.wal.gc(None).await.unwrap();
        assert_eq!(
            (collected.removed.len(), collected.retained_for_replay),
            (0, 1)
        );
        assert!(disk.names(&wal_dir()).contains(&"CHECKPOINT".to_owned()));
        let segments = node.wal.segments().len() as u64;
        node.stop().await;

        let check = |report: &RecoveryReport, used_hint: bool| {
            assert_eq!(report.used_hint, used_hint);
            let skipped = u64::from(used_hint);
            assert_eq!(report.scanned_segments, segments - skipped);
            assert_eq!(report.last_lsn, Lsn::new(27));
            assert_eq!(report.watermark.map(|w| w.checkpoint), Some(checkpoint(4)));
            assert_eq!(report.unchecked_seals, [sealed_b]);
            assert_eq!(report.sealed_through(), Lsn::new(12));
            assert_eq!(report.unresolved_range(), (Lsn::new(5), Lsn::new(27)));
            assert_eq!(report.first_retained_lsn(), Lsn::new(1));
        };

        // With the hint the scan starts at the segment that holds LSN 5.
        let node = start(&sim, &disk, "hinted", &config(SEGMENT)).await;
        check(&node.report, true);
        let with_hint = node.report.clone();
        node.stop().await;

        // Without it, from the oldest segment; the result is the same.
        disk.remove(&entry("CHECKPOINT")).await.unwrap();
        disk.sync_dir(&wal_dir()).await.unwrap();
        let node = start(&sim, &disk, "missing", &config(SEGMENT)).await;
        check(&node.report, false);
        assert!(node.report.scanned_bytes > with_hint.scanned_bytes);
        assert_eq!(node.report.segments, with_hint.segments);
        node.stop().await;

        // A hint that fails its checksum, and one that claims a watermark no checkpoint frame confirms.
        let lying = CheckpointHint {
            batch_seq: 1,
            last_lsn: Lsn::new(20),
            segment_no: segments,
        };
        for (name, bytes) in [
            ("garbage", vec![0x55; 28]),
            ("lying", lying.encode().to_vec()),
        ] {
            let file = disk
                .open(&entry("CHECKPOINT"), OpenMode::Write)
                .await
                .unwrap();
            file.append(0, vec![Bytes::from(bytes)]).await.unwrap();
            file.sync_data().await.unwrap();
            disk.sync_dir(&wal_dir()).await.unwrap();
            let node = start(&sim, &disk, name, &config(SEGMENT)).await;
            check(&node.report, false);
            node.stop().await;
        }
    });
}

#[test]
fn segments_without_a_valid_epoch_are_a_hard_error() {
    run(9, |sim| async move {
        let disk = sim.disk("missing");
        three_segments(&sim, &disk).await;
        disk.remove(&entry("EPOCH")).await.unwrap();
        disk.sync_dir(&wal_dir()).await.unwrap();
        let error = assert_fatal(&sim, &disk, "missing").await;
        assert_eq!(error, RecoveryError::MissingEpoch);
        assert_eq!(error.code(), "wal_epoch");
        // Recovery must not have minted a new epoch next to the segments.
        assert!(!disk.names(&wal_dir()).contains(&"EPOCH".to_owned()));

        let disk = sim.disk("invalid");
        three_segments(&sim, &disk).await;
        assert!(disk.patch_file(&entry("EPOCH"), 3, &[0xFF]));
        assert!(matches!(
            assert_fatal(&sim, &disk, "invalid").await,
            RecoveryError::InvalidEpoch(_)
        ));

        // A segment of another directory.
        let disk = sim.disk("foreign");
        three_segments(&sim, &disk).await;
        let other = sim.disk("other");
        // Another host name: the epoch is minted from the host's random stream.
        three_segments_on(&sim, &other, "other-build").await;
        let foreign = other.read_file(&seg(2)).unwrap();
        assert!(disk.patch_file(&seg(2), 0, &foreign));
        assert_eq!(
            assert_fatal(&sim, &disk, "foreign").await,
            RecoveryError::BadSegmentHeader {
                segment_no: 2,
                reason: SegmentHeaderIssue::EpochMismatch
            }
        );

        // An initialisation that crashed before the epoch was synced left no segment: start over.
        let disk = sim.disk("unfinished");
        let file = disk
            .open(&entry("EPOCH"), OpenMode::CreateNew)
            .await
            .unwrap();
        file.append(0, vec![Bytes::from_static(&[1, 2, 3])])
            .await
            .unwrap();
        drop(file);
        let node = start(&sim, &disk, "unfinished", &config(SEGMENT)).await;
        assert!(node.report.created);
        assert_eq!(node.fill(1..=1).await, [1]);
        node.stop().await;
    });
}

#[test]
fn an_unfinished_newest_segment_is_removed_and_any_other_bad_header_is_fatal() {
    run(10, |sim| async move {
        let build = |name: &'static str| {
            let sim = &sim;
            async move {
                let disk = sim.disk(name);
                let node = start(sim, &disk, "build", &config(SEGMENT)).await;
                node.fill(1..=4).await;
                let epoch = node.wal.epoch();
                node.stop().await;
                let header = SegmentHeader {
                    wal_epoch: epoch,
                    segment_no: 2,
                    first_lsn: Lsn::new(5),
                    created_at: Timestamp::from_micros(SIM_EPOCH_MICROS),
                };
                (disk, header.encode())
            }
        };
        let create = |disk: SimDisk, bytes: Vec<u8>| async move {
            let file = disk.open(&seg(2), OpenMode::CreateNew).await.unwrap();
            if !bytes.is_empty() {
                file.append(0, vec![Bytes::from(bytes)]).await.unwrap();
            }
            file.sync_data().await.unwrap();
            disk.sync_dir(&wal_dir()).await.unwrap();
        };

        // A rotation that crashed after creating the file: empty, zero header, or a partial header.
        for (name, len, zeros) in [("empty", 0, 0), ("zeros", 0, 64), ("partial", 30, 0)] {
            let (disk, header) = build(name).await;
            let mut bytes = header[..len].to_vec();
            bytes.resize(len + zeros, 0);
            create(disk.clone(), bytes).await;
            let node = start(&sim, &disk, name, &config(SEGMENT)).await;
            assert_eq!(node.report.removed_unfinished_segment, Some(2), "{name}");
            assert_eq!(node.report.last_lsn, Lsn::new(4), "{name}");
            // The next append rotates into a fresh segment 2.
            assert_eq!(node.fill(5..=5).await, [5], "{name}");
            assert_eq!(node.tags().await, [1, 2, 3, 4, 5], "{name}");
            node.stop().await;
        }

        // A bad header with data after it is not an unfinished creation.
        let (disk, header) = build("data-after").await;
        let mut bytes = header[..30].to_vec();
        bytes.resize(200, 0);
        bytes.push(7);
        create(disk.clone(), bytes).await;
        assert!(matches!(
            assert_fatal(&sim, &disk, "data-after").await,
            RecoveryError::BadSegmentHeader { segment_no: 2, .. }
        ));

        // A zero header in a segment that is not the newest.
        let disk = sim.disk("older");
        three_segments(&sim, &disk).await;
        assert!(disk.patch_file(&seg(2), 0, &[0; 64]));
        assert_eq!(
            assert_fatal(&sim, &disk, "older").await,
            RecoveryError::BadSegmentHeader {
                segment_no: 2,
                reason: SegmentHeaderIssue::Unwritten
            }
        );
    });
}

#[test]
fn queued_appends_share_one_group_and_one_sync() {
    run(11, |sim| async move {
        let disk = sim.disk("disk");
        disk.set_faults(DiskFaults {
            sync_latency_micros: (1_000, 1_000),
            ..DiskFaults::default()
        });
        let node = start(&sim, &disk, "a", &config(64 << 10)).await;
        let syncs_at_open = node.syncs();
        let pending: Vec<_> = (0..20).map(|tag| node.wal.append(data(tag))).collect();
        let lsns = futures::future::join_all(pending).await;
        assert_eq!(
            lsns,
            (1..=20).map(|lsn| Ok(Lsn::new(lsn))).collect::<Vec<_>>()
        );
        // Twenty frames, one append, one sync.
        assert_eq!(node.syncs() - syncs_at_open, 1);
        let recorder = &node.host.recorder;
        assert_eq!(
            recorder.counter_value(&registry::WAL_APPENDED_FRAMES, &[]),
            20
        );
        assert_eq!(
            recorder.counter_value(&registry::WAL_APPENDED_BYTES, &[]),
            20 * FRAME as u64
        );
        assert_eq!(
            recorder.gauge_value(&registry::WAL_DURABLE_LSN, &[]),
            Some(20)
        );
        node.stop().await;
    });
}

#[test]
fn a_group_holds_at_most_eight_mebibytes() {
    run(12, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "a", &config(16 << 20)).await;
        let syncs_at_open = node.syncs();
        let big =
            || PreparedFrame::new(RecordType::Noop, vec![7; 2 << 20], Compression::None).unwrap();
        let pending: Vec<_> = (0..5).map(|_| node.wal.append(big())).collect();
        let lsns = futures::future::join_all(pending).await;
        assert_eq!(lsns.len(), 5);
        assert!(lsns.iter().all(Result::is_ok));
        // Three frames fit under 8 MiB; the fourth starts the next group.
        assert_eq!(node.syncs() - syncs_at_open, 2);
        node.stop().await;
    });
}

#[test]
fn gc_removes_only_whole_segments_below_the_watermark_the_pin_and_retention() {
    run(13, |sim| async move {
        let disk = sim.disk("disk");
        let settings = WalConfig {
            replay_retention: Duration::from_secs(60),
            ..config(SEGMENT)
        };
        let node = start(&sim, &disk, "a", &settings).await;
        node.fill(1..=12).await;
        let numbers =
            |wal: &Wal| -> Vec<u64> { wal.segments().iter().map(|s| s.segment_no).collect() };

        // No checkpoint: nothing may go, however old.
        node.host.env.clock.sleep(Duration::from_secs(120)).await;
        let collected = node.wal.gc(None).await.unwrap();
        assert_eq!(
            (collected.watermark, collected.removed.len()),
            (Lsn::ZERO, 0)
        );

        assert_eq!(node.append(&Record::BatchSeal(seal(1, 8))).await, 13);
        assert_eq!(node.append(&Record::Checkpoint(checkpoint(8))).await, 14);
        assert_eq!(numbers(&node.wal), [1, 2, 3, 4]);
        // Segments 1 and 2 are below the watermark. They were closed long ago, but the recent-span index
        // still references LSN 6, in segment 2.
        let collected = node.wal.gc(Some(Lsn::new(6))).await.unwrap();
        assert_eq!(collected.watermark, Lsn::new(8));
        assert_eq!(
            collected
                .removed
                .iter()
                .map(|s| s.segment_no)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(numbers(&node.wal), [2, 3, 4]);
        let collected = node.wal.gc(None).await.unwrap();
        assert_eq!(collected.removed.len(), 1);
        // Segment 3 holds LSN 9 to 12, above the watermark.
        assert_eq!(numbers(&node.wal), [3, 4]);
        assert_eq!(
            disk.names(&wal_dir()),
            [
                "CHECKPOINT",
                "EPOCH",
                &seg(3).to_string()[4..],
                &seg(4).to_string()[4..]
            ]
        );
        assert_eq!(
            node.wal.replay(Lsn::new(1)).next().await,
            Err(ReplayError::Reclaimed {
                lsn: Lsn::new(1),
                oldest: Lsn::new(9)
            })
        );

        // A new checkpoint covers segment 3. It was closed when segment 4 was created, a moment ago, so
        // replay retention keeps it until it is 60 seconds old.
        node.fill(15..=18).await;
        assert_eq!(node.append(&Record::BatchSeal(seal(9, 14))).await, 19);
        assert_eq!(node.append(&Record::Checkpoint(checkpoint(14))).await, 20);
        assert_eq!(numbers(&node.wal), [3, 4, 5]);
        node.host.env.clock.sleep(Duration::from_secs(30)).await;
        let collected = node.wal.gc(None).await.unwrap();
        assert_eq!(
            (collected.removed.len(), collected.retained_for_replay),
            (0, 1)
        );
        assert_eq!(numbers(&node.wal), [3, 4, 5]);
        node.host.env.clock.sleep(Duration::from_secs(31)).await;
        let collected = node.wal.gc(None).await.unwrap();
        assert_eq!(
            (collected.removed.len(), collected.retained_for_replay),
            (1, 0)
        );
        // Segment 4 holds LSN 13 to 17, above the watermark; segment 5 is the current one.
        assert_eq!(numbers(&node.wal), [4, 5]);
        assert_eq!(
            node.host
                .recorder
                .counter_value(&registry::WAL_GC_REMOVED_SEGMENTS, &[]),
            3
        );
        node.crash().await;

        let node = start(&sim, &disk, "b", &settings).await;
        assert_eq!(node.report.last_lsn, Lsn::new(20));
        assert_eq!(node.report.watermark_lsn(), Lsn::new(14));
        assert_eq!(node.report.first_retained_lsn(), Lsn::new(13));
        node.stop().await;
    });
}

#[test]
fn replay_retention_obeys_the_byte_budget_and_yields_under_pressure() {
    run(14, |sim| async move {
        let disk = sim.disk("disk");
        // Budget: a quarter of 16 segments. High-water: 90% of 16 segments, reached with 15.
        let settings = WalConfig {
            max_bytes: 16 * SEGMENT,
            replay_retention: Duration::from_secs(3_600),
            ..config(SEGMENT)
        };
        let node = start(&sim, &disk, "a", &settings).await;
        node.fill(1..=40).await;
        assert_eq!(node.append(&Record::BatchSeal(seal(1, 40))).await, 41);
        assert_eq!(node.append(&Record::Checkpoint(checkpoint(40))).await, 42);
        assert_eq!(node.wal.segments().len(), 11);
        let admission = node.wal.admission();
        assert_eq!(admission.segment_bytes, 11 * SEGMENT);
        assert_eq!(admission.unshipped_bytes, SEGMENT);
        assert!(admission.accepting());

        // Ten young segments are below the watermark; the budget keeps four.
        let collected = node.wal.gc(None).await.unwrap();
        assert_eq!(
            (collected.removed.len(), collected.retained_for_replay),
            (6, 4)
        );

        // Ingest outruns the shipper: at the high-water mark retention is given up first.
        node.fill(43..=82).await;
        assert_eq!(node.wal.segments().len(), 15);
        let admission = node.wal.admission();
        assert!(admission.over_high_water && !admission.accepting());
        assert_eq!(admission.unshipped_bytes, 11 * SEGMENT);
        let collected = node.wal.gc(None).await.unwrap();
        assert_eq!(
            (collected.removed.len(), collected.retained_for_replay),
            (4, 0)
        );
        assert!(node.wal.admission().accepting());
        assert_eq!(
            node.host
                .recorder
                .gauge_value(&registry::WAL_UNSHIPPED_BYTES, &[]),
            Some(11 * SEGMENT as i64)
        );
        node.stop().await;
    });
}

#[test]
fn admission_reports_the_disk_floor_and_a_full_disk_fails_the_wal() {
    run(15, |sim| async move {
        let disk = sim.disk("disk");
        // EPOCH takes 20 bytes of the budget.
        disk.set_faults(DiskFaults {
            capacity_bytes: Some(4 * SEGMENT + 20),
            ..DiskFaults::default()
        });
        let settings = WalConfig {
            min_free_bytes: 2 * SEGMENT,
            ..config(SEGMENT)
        };
        let node = start(&sim, &disk, "a", &settings).await;
        assert_eq!(node.wal.admission().free_bytes, 3 * SEGMENT);
        node.fill(1..=5).await;
        // Two segments: exactly at the floor.
        assert!(node.wal.admission().accepting());
        node.fill(6..=9).await;
        let admission = node.wal.admission();
        assert!(admission.below_disk_floor && !admission.accepting());
        assert_eq!(admission.free_bytes, SEGMENT);

        // Admission is ingest's to enforce. Past it the disk fills up: the rotation fails and so does
        // the WAL.
        node.fill(10..=16).await;
        assert_eq!(node.wal.append(data(17)).await, Err(AppendError::Failed));
        assert!(node.wal.admission().failed);
        node.crash().await;
        let node = start(&sim, &disk, "b", &settings).await;
        assert_eq!(node.tags().await, (1..=16).collect::<Vec<u8>>());
        node.stop().await;
    });
}

#[test]
fn the_unsafe_modes_acknowledge_before_the_sync_and_say_so() {
    run(16, |sim| async move {
        assert!(!Durability::Group.is_unsafe());

        let disk = sim.disk("interval");
        let settings = WalConfig {
            durability: Durability::Interval(Duration::from_millis(50)),
            ..config(SEGMENT)
        };
        let node = start(&sim, &disk, "a", &settings).await;
        assert!(node.wal.admission().durability_unsafe);
        let syncs_at_open = node.syncs();
        assert_eq!(node.fill(1..=3).await, [1, 2, 3]);
        assert_eq!(node.syncs(), syncs_at_open);
        node.host.env.clock.sleep(Duration::from_millis(60)).await;
        assert_eq!(node.syncs(), syncs_at_open + 1);
        // Synced by the timer, so these survive a crash.
        node.crash().await;
        let node = start(&sim, &disk, "b", &settings).await;
        assert_eq!(node.tags().await, [1, 2, 3]);
        node.stop().await;

        let disk = sim.disk("none");
        let settings = WalConfig {
            durability: Durability::None,
            ..config(SEGMENT)
        };
        let node = start(&sim, &disk, "c", &settings).await;
        assert!(node.wal.admission().durability_unsafe);
        let syncs_at_open = node.syncs();
        assert_eq!(node.fill(1..=3).await, [1, 2, 3]);
        node.host.env.clock.sleep(Duration::from_secs(60)).await;
        assert_eq!(node.syncs(), syncs_at_open);
        assert_eq!(node.tags().await, [1, 2, 3]);
        node.stop().await;
    });
}

#[test]
fn invalid_frames_and_settings_are_refused() {
    run(17, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, "a", &config(SEGMENT)).await;
        node.fill(1..=2).await;

        // A watermark above the durable LSN would let garbage collection remove unresolved frames.
        let ahead = control(&Record::Checkpoint(checkpoint(3)));
        assert_eq!(
            node.wal.append(ahead).await,
            Err(AppendError::InvalidCheckpoint)
        );
        assert_eq!(node.wal.watermark(), None);
        assert_eq!(node.append(&Record::Checkpoint(checkpoint(2))).await, 3);
        assert_eq!(node.wal.watermark().unwrap().last_lsn(), Lsn::new(2));
        // Checkpoints are monotonic: an older one does not lower the watermark.
        assert_eq!(node.append(&Record::Checkpoint(checkpoint(1))).await, 4);
        assert_eq!(node.wal.watermark().unwrap().last_lsn(), Lsn::new(2));

        let huge = PreparedFrame::new(RecordType::Noop, vec![1; 2_000], Compression::None).unwrap();
        assert_eq!(
            node.wal.append(huge).await,
            Err(AppendError::TooLarge {
                len: 2_036,
                max: SEGMENT - 64
            })
        );
        assert_eq!(node.wal.durable_lsn(), Lsn::new(4));
        node.stop().await;

        let error = try_start(&sim, &sim.disk("tiny"), "tiny", &config(100))
            .await
            .err()
            .unwrap();
        assert!(matches!(error, RecoveryError::InvalidConfig(_)));
    });
}
