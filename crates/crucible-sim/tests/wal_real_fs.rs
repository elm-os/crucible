//! `crucible-wal` on a real filesystem through the production `PortableDisk`: a smoke test (write, sync,
//! reopen, recover) and an ignored measurement of append throughput and durable-ack latency.
//!
//! The measurement:
//!
//! ```sh
//! cargo nextest run -p crucible-sim --cargo-profile release --test wal_real_fs perf --run-ignored all --no-capture
//! ```
//!
//! `CRUCIBLE_WAL_PERF_DIR=<dir>` picks the directory (default: the system temporary directory).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crucible_env::Root;
use crucible_env_prod::{Prod, ProdConfig, WalBackend, build};
use crucible_records::Lsn;
use crucible_wal::{Durability, RecoveryReport, Wal, WalConfig};
use crucible_wal_codec::{BatchSeal, Checkpoint, Compression, PreparedFrame, Record, RecordType};

fn prod(dir: &Path) -> Prod {
    let config = ProdConfig {
        roots: BTreeMap::from([(Root::Wal, dir.to_path_buf())]),
        wal_backend: WalBackend::Portable,
    };
    build(tokio::runtime::Handle::current(), &config).unwrap()
}

/// Payload `index` of the smoke test: every third one compresses, the others do not.
fn payload(index: u64) -> Vec<u8> {
    let len = 40 + (index * 577 % 3_000) as usize;
    let mut x = index.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            if index.is_multiple_of(3) {
                index as u8
            } else {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            }
        })
        .collect()
}

fn frame(record_type: RecordType, payload: Vec<u8>) -> PreparedFrame {
    PreparedFrame::new(record_type, payload, Compression::Lz4).unwrap()
}

async fn open(dir: &Path, config: &WalConfig) -> (Prod, Wal, RecoveryReport) {
    let prod = prod(dir);
    let (wal, report) = Wal::open(&prod.env, config.clone()).await.unwrap();
    (prod, wal, report)
}

/// Drops the WAL and waits for its committer, which stops when the last handle is gone, so that the
/// directory has one writer when it is opened again.
async fn close(prod: Prod, wal: Wal) {
    drop(wal);
    while !prod.spawner.task_names().is_empty() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn read_payloads(wal: &Wal, from: Lsn) -> Vec<(u64, RecordType, Vec<u8>)> {
    let mut replay = wal.replay(from);
    let mut out = Vec::new();
    while let Some(frame) = replay.next().await.unwrap() {
        let payload = frame.payload().unwrap().into_owned();
        out.push((frame.lsn().get(), frame.record_type(), payload));
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_sync_reopen_recover_on_a_real_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let config = WalConfig {
        segment_bytes: 64 << 10,
        max_bytes: 64 << 20,
        min_free_bytes: 0,
        ..WalConfig::default()
    };

    // Write: 300 frames from concurrent submitters, across several segments.
    let (prod, wal, report) = open(dir.path(), &config).await;
    assert!(report.created);
    assert_eq!(report.last_lsn, Lsn::ZERO);
    let epoch = wal.epoch();
    let pending: Vec<_> = (0..300u64)
        .map(|index| wal.append(frame(RecordType::Noop, payload(index))))
        .collect();
    let mut by_lsn = BTreeMap::new();
    for (index, done) in pending.into_iter().enumerate() {
        by_lsn.insert(done.await.unwrap().get(), index as u64);
    }
    assert_eq!(
        by_lsn.keys().copied().collect::<Vec<_>>(),
        (1..=300).collect::<Vec<_>>()
    );
    assert_eq!(wal.durable_lsn(), Lsn::new(300));
    assert!(wal.segments().len() > 3);
    assert!(wal.admission().accepting());

    // Seal and checkpoint the first hundred frames, then collect: replay retention keeps the segments,
    // and the hint is written.
    let seal = BatchSeal {
        first_lsn: Lsn::new(1),
        last_lsn: Lsn::new(100),
        frame_count: 100,
        normalizer_version: 1,
        price_table_version: 1,
        deletion_seq: 0,
    };
    let checkpoint = Checkpoint {
        batch_seq: 1,
        last_lsn: Lsn::new(100),
        table_bitmap: Checkpoint::ALL_TABLES,
    };
    let sealed = Record::BatchSeal(seal);
    let checked = Record::Checkpoint(checkpoint);
    let seal_lsn = wal
        .append(frame(RecordType::BatchSeal, sealed.encode().unwrap()))
        .await
        .unwrap();
    wal.append(frame(RecordType::Checkpoint, checked.encode().unwrap()))
        .await
        .unwrap();
    assert_eq!(seal_lsn, Lsn::new(301));
    let collected = wal.gc(None).await.unwrap();
    assert_eq!(collected.watermark, Lsn::new(100));
    assert!(collected.removed.is_empty() && collected.retained_for_replay > 0);
    let segments = wal.segments();
    close(prod, wal).await;

    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(&names[..2], ["CHECKPOINT", "EPOCH"]);
    assert_eq!(names[2], format!("seg-{:020}.wal", 1));
    assert_eq!(names.len(), 2 + segments.len());
    for name in &names[2..] {
        let len = std::fs::metadata(dir.path().join(name)).unwrap().len();
        assert_eq!(len, config.segment_bytes, "{name} is not preallocated");
    }

    // Reopen and recover.
    let (prod, wal, report) = open(dir.path(), &config).await;
    assert!(!report.created);
    assert_eq!(report.epoch, epoch);
    assert_eq!(report.last_lsn, Lsn::new(302));
    assert_eq!(report.watermark.map(|w| w.checkpoint), Some(checkpoint));
    assert!(report.used_hint);
    assert!(report.unchecked_seals.is_empty());
    assert_eq!(report.tail_repair, None);
    assert_eq!(report.segments, segments);
    let frames = read_payloads(&wal, Lsn::FIRST).await;
    assert_eq!(frames.len(), 302);
    for (lsn, record_type, bytes) in &frames[..300] {
        assert_eq!(*record_type, RecordType::Noop);
        assert_eq!(*bytes, payload(by_lsn[lsn]), "lsn {lsn}");
    }
    assert_eq!(Record::decode(frames[300].1, &frames[300].2), Ok(sealed));
    assert_eq!(Record::decode(frames[301].1, &frames[301].2), Ok(checked));
    let last = wal.replay(Lsn::new(302)).next().await.unwrap().unwrap();
    let (last_segment, tail_offset) = (last.segment_no(), last.end_offset());
    close(prod, wal).await;

    // A torn tail: half a frame after the end of the log, as an interrupted write leaves it.
    let mut torn = Vec::new();
    frame(RecordType::Noop, payload(1)).write_to(Lsn::new(303), last.received_at(), &mut torn);
    let path = dir.path().join(format!("seg-{last_segment:020}.wal"));
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(tail_offset)).unwrap();
    file.write_all(&torn[..torn.len() / 2]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let (prod, wal, report) = open(dir.path(), &config).await;
    assert_eq!(report.last_lsn, Lsn::new(302));
    let repair = report.tail_repair.unwrap();
    assert_eq!(
        (repair.segment_no, repair.offset),
        (last_segment, tail_offset)
    );
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len() as u64, config.segment_bytes);
    assert!(bytes[tail_offset as usize..].iter().all(|&b| b == 0));
    // The log continues at the LSN of the torn frame.
    let next = wal
        .append(frame(RecordType::Noop, payload(7)))
        .await
        .unwrap();
    assert_eq!(next, Lsn::new(303));
    close(prod, wal).await;

    let (prod, wal, report) = open(dir.path(), &config).await;
    assert_eq!((report.last_lsn, report.tail_repair), (Lsn::new(303), None));
    let frames = read_payloads(&wal, Lsn::new(303)).await;
    assert_eq!(frames, [(303, RecordType::Noop, payload(7))]);
    close(prod, wal).await;
}

fn percentile(sorted: &[Duration], per_mille: usize) -> Duration {
    sorted[(sorted.len() - 1) * per_mille / 1000]
}

/// One measurement: `clients` tasks each append `per_client` frames of `frame_bytes`, one at a time.
async fn measure(dir: &Path, frame_bytes: usize, clients: usize, per_client: usize) {
    let run_dir = tempfile::tempdir_in(dir).unwrap();
    let config = WalConfig {
        durability: Durability::Group,
        ..WalConfig::default()
    };
    let (prod, wal, _) = open(run_dir.path(), &config).await;
    let started = Instant::now();
    let mut tasks = Vec::new();
    for client in 0..clients {
        let wal = wal.clone();
        tasks.push(tokio::spawn(async move {
            let mut latencies = Vec::with_capacity(per_client);
            for index in 0..per_client {
                // Incompressible, like a compressed OTLP body; prepared off the committer, as ingest does.
                let mut x = ((client * per_client + index) as u64).wrapping_mul(0x9E37_79B9) | 1;
                let bytes: Vec<u8> = (0..frame_bytes)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        x as u8
                    })
                    .collect();
                let prepared = frame(RecordType::Noop, bytes);
                let submitted = Instant::now();
                wal.append(prepared).await.unwrap();
                latencies.push(submitted.elapsed());
            }
            latencies
        }));
    }
    let mut latencies = Vec::new();
    for task in tasks {
        latencies.extend(task.await.unwrap());
    }
    let elapsed = started.elapsed();
    latencies.sort();
    let frames = latencies.len();
    let syncs = prod
        .recorder
        .histogram_value(&crucible_env::metrics::registry::WAL_SYNC_DURATION, &[])
        .map_or(0, |h| h.count);
    println!(
        "frame {frame_bytes:>6} B  clients {clients:>3}  frames {frames:>6}  {:>9.0} appends/s  {:>7.1} MiB/s  \
         ack p50 {:>8.3} ms  p99 {:>8.3} ms  frames/sync {:>6.1}",
        frames as f64 / elapsed.as_secs_f64(),
        (frames * frame_bytes) as f64 / elapsed.as_secs_f64() / f64::from(1 << 20),
        percentile(&latencies, 500).as_secs_f64() * 1e3,
        percentile(&latencies, 990).as_secs_f64() * 1e3,
        frames as f64 / syncs.max(1) as f64,
    );
    close(prod, wal).await;
}

/// Appends per second and durable-ack latency for 1 KiB and 64 KiB frames on `PortableDisk`, to inform
/// the performance work of a later milestone. Not a gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
#[ignore = "measurement: run with --cargo-profile release --run-ignored all --no-capture"]
async fn perf_durable_appends_on_portable_disk() {
    let dir = std::env::var_os("CRUCIBLE_WAL_PERF_DIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    println!("directory {}", dir.display());
    for (frame_bytes, clients, per_client) in [
        (1 << 10, 1, 500),
        (1 << 10, 16, 500),
        (1 << 10, 256, 200),
        (64 << 10, 1, 200),
        (64 << 10, 16, 100),
        (64 << 10, 64, 40),
    ] {
        measure(&dir, frame_bytes, clients, per_client).await;
    }
}
