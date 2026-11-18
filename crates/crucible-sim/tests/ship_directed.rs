//! Directed tests of `crucible-shipper` over `crucible-wal` on `SimDisk` and the store model: one scripted
//! situation each, with the calls the store received checked in order (contract sections 5.1 and 5.4 to 5.7).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_env::metrics::registry;
use crucible_env::{
    DiskPath, InsertError, InsertSettings, RetryReason, Root, StoreFlavour, Stores,
};
use crucible_records::{BatchToken, Lsn, TableId};
use crucible_schema::Granularity;
use crucible_shipper::{
    FRAMES_FILE, Hooks, MANIFEST_FILE, ShipError, Shipper, ShipperConfig, StatsSnapshot,
};
use crucible_sim::ch_model::{Call, ChFaults, ChModel, Script, u64_at};
use crucible_sim::ship_sim::request_payload;
use crucible_sim::{Host, SIM_EPOCH_MICROS, Sim, SimDisk, run};
use crucible_wal::{RecoveryReport, Wal, WalConfig};
use crucible_wal_codec::{Compression, PreparedFrame, Record, RecordType};

/// The four tables a span yields rows in.
const TABLES: [TableId; 4] = [
    TableId::SpanCore,
    TableId::SpanPayload,
    TableId::TraceHeads,
    TableId::TraceStats,
];

struct Rig {
    sim: Sim,
    disk: SimDisk,
    model: ChModel,
    boots: Mutex<u64>,
}

type Ended = Arc<Mutex<Option<Result<(), ShipError>>>>;

struct Node {
    host: Host,
    wal: Wal,
    ended: Ended,
    stats: Arc<crucible_shipper::ShipperStats>,
}

impl Node {
    async fn sleep(&self, millis: u64) {
        self.host
            .env
            .clock
            .sleep(Duration::from_millis(millis))
            .await;
    }

    /// Appends one request with `spans` root spans of fresh traces and returns its LSN once durable.
    async fn ingest(&self, tag: u8, spans: u8) -> Lsn {
        let specs: Vec<_> = (0..spans)
            .map(|i| {
                (
                    [tag; 16],
                    [tag, i, 1, 1, 1, 1, 1, 1],
                    SIM_EPOCH_MICROS + i64::from(i),
                    true,
                )
            })
            .collect();
        let payload = request_payload([1; 16], &specs, 0).unwrap();
        let frame = PreparedFrame::new(RecordType::IngestRequest, payload, Compression::Lz4);
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    fn watermark(&self) -> u64 {
        self.wal.watermark().map_or(0, |w| w.last_lsn().get())
    }

    fn snapshot(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    fn ended(&self) -> Option<Result<(), ShipError>> {
        self.ended.lock().unwrap().clone()
    }
}

fn config() -> ShipperConfig {
    ShipperConfig {
        linger: Duration::from_millis(10),
        tail_poll: Duration::from_millis(1),
        backoff_min: Duration::from_millis(1),
        backoff_max: Duration::from_millis(8),
        gc_interval: Duration::from_millis(10),
        ..ShipperConfig::default()
    }
}

impl Rig {
    /// Starts a node on the disk: recovers the WAL and runs the shipper.
    async fn boot(&self) -> (Node, RecoveryReport) {
        self.boot_with(config()).await
    }

    /// [`Rig::boot`] with these shipper settings.
    async fn boot_with(&self, shipper_config: ShipperConfig) -> (Node, RecoveryReport) {
        let boot = {
            let mut boots = self.boots.lock().unwrap();
            *boots += 1;
            *boots
        };
        let host = self.sim.host(&format!("node/boot-{boot}"), &self.disk, 0);
        let wal_config = WalConfig {
            segment_bytes: 64 << 10,
            max_bytes: 64 << 20,
            min_free_bytes: 0,
            ..WalConfig::default()
        };
        let (wal, report) = Box::pin(Wal::open(&host.env, wal_config)).await.unwrap();
        let stores = Stores::new(Arc::new(self.model.clone()));
        let shipper = Shipper::new(
            &host.env,
            &stores,
            wal.clone(),
            &report,
            shipper_config,
            Hooks::default(),
        )
        .unwrap();
        let stats = shipper.stats();
        let ended: Ended = Arc::default();
        let slot = Arc::clone(&ended);
        drop(host.env.spawn("shipper", async move {
            let result = Box::pin(shipper.run()).await;
            *slot.lock().unwrap() = Some(result);
        }));
        let node = Node {
            host,
            wal,
            ended,
            stats,
        };
        (node, report)
    }

    fn rows(&self, table: TableId) -> usize {
        self.model.parts(table).iter().map(|p| p.block.rows).sum()
    }

    /// The calls of each pair, in order: `I` for an insert, `P` for a probe.
    fn calls_by_pair(&self) -> BTreeMap<(TableId, u64), String> {
        let mut by_pair: BTreeMap<(TableId, u64), String> = BTreeMap::new();
        for call in self.model.calls() {
            let (key, mark) = match call {
                Call::Insert { token, .. } => ((token.table(), token.batch_seq().get()), 'I'),
                Call::Probe { pair } => ((pair.0, pair.2), 'P'),
            };
            by_pair.entry(key).or_default().push(mark);
        }
        by_pair
    }
}

fn directed<F, Fut>(body: F)
where
    F: FnOnce(Rig) -> Fut,
    Fut: Future<Output = ()>,
{
    run(7, |sim| {
        let disk = sim.disk("disk");
        let model = ChModel::new(
            sim.rng("store").as_ref(),
            StoreFlavour::SingleNode,
            Granularity::Day,
            Duration::from_secs(60),
        );
        body(Rig {
            sim,
            disk,
            model,
            boots: Mutex::new(0),
        })
    });
}

fn retry() -> InsertError {
    InsertError::Retry {
        reason: RetryReason::TooManyQueries,
        message: "busy".to_owned(),
    }
}

fn unknown() -> InsertError {
    InsertError::UnknownOutcome {
        code: None,
        message: "timeout".to_owned(),
    }
}

#[test]
fn a_batch_lands_once_per_table_and_is_checkpointed() {
    directed(|rig| async move {
        rig.model.set_script(Script::new(|_, _| None));
        let (node, _) = rig.boot().await;
        node.ingest(1, 3).await;
        let last = node.ingest(2, 2).await;
        node.sleep(200).await;
        assert_eq!(node.ended(), None);
        assert!(
            node.watermark() >= last.get(),
            "watermark {}",
            node.watermark()
        );
        assert_eq!(rig.rows(TableId::SpanCore), 5);
        assert_eq!(rig.rows(TableId::SpanPayload), 5);
        assert_eq!(rig.rows(TableId::TraceHeads), 5);
        // One combined row per trace (each request is one trace), and one marker row per block.
        let stats_blocks = rig.model.parts(TableId::TraceStats).len();
        assert_eq!(rig.rows(TableId::TraceStats), 2 + stats_blocks);
        // Not in doubt: one insert per pair, no probe.
        for (pair, calls) in rig.calls_by_pair() {
            assert_eq!(calls, "I", "{pair:?}");
        }
        // Every insert carries the token of its pair and exactly the settings of contract 5.4.
        for call in rig.model.calls() {
            let Call::Insert {
                token, settings, ..
            } = call
            else {
                continue;
            };
            let expected = InsertSettings::new(token, StoreFlavour::SingleNode).pairs();
            assert_eq!(settings, expected);
            assert_eq!(settings[2].1, token.text().as_str());
        }
        assert!(
            rig.model.violations().is_empty(),
            "{:?}",
            rig.model.violations()
        );
        node.host.stop().await;
    });
}

#[test]
fn every_token_names_a_sealed_range_and_ranges_are_adjacent() {
    directed(|rig| async move {
        rig.model.set_script(Script::new(|_, _| None));
        let (node, _) = rig.boot().await;
        for tag in 1..6 {
            node.ingest(tag, 1).await;
            node.sleep(25).await;
        }
        node.sleep(200).await;
        // The log in LSN order: every token's range is named by a `BatchSeal` frame.
        let mut replay = node.wal.replay(Lsn::FIRST);
        let mut seals = Vec::new();
        while let Some(frame) = replay.next().await.unwrap() {
            if frame.record_type() == RecordType::BatchSeal {
                let payload = frame.payload().unwrap();
                let Record::BatchSeal(seal) =
                    Record::decode(RecordType::BatchSeal, &payload).unwrap()
                else {
                    panic!("not a seal");
                };
                assert_eq!(
                    seal.normalizer_version,
                    crucible_normalize::NORMALIZER_VERSION
                );
                assert_eq!(
                    seal.price_table_version,
                    crucible_normalize::BUNDLED_PRICE_TABLE_VERSION
                );
                assert!(seal.last_lsn < frame.lsn());
                seals.push((seal.first_lsn, seal.last_lsn));
            }
        }
        assert!(seals.len() >= 2, "{seals:?}");
        // Ranges of consecutive batches are adjacent.
        for pair in seals.windows(2) {
            assert_eq!(pair[1].0.get(), pair[0].1.get() + 1);
        }
        let mut inserts = 0;
        for call in rig.model.calls() {
            if let Call::Insert { token, .. } = call {
                inserts += 1;
                assert!(
                    seals.contains(&(token.first_lsn(), token.last_lsn())),
                    "{token:?}"
                );
            }
        }
        assert!(inserts >= 8);
        node.host.stop().await;
    });
}

#[test]
fn a_retry_sends_the_same_token_and_the_same_bytes() {
    directed(|rig| async move {
        // Every pair is rejected twice before it is accepted.
        rig.model
            .set_script(Script::new(|_, attempt| (attempt <= 2).then(retry)));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 4).await;
        node.sleep(300).await;
        assert!(node.watermark() >= lsn.get());
        let mut attempts: BTreeMap<String, Vec<[u8; 32]>> = BTreeMap::new();
        for call in rig.model.calls() {
            match call {
                Call::Insert { token, body, .. } => {
                    attempts
                        .entry(token.text().as_str().to_owned())
                        .or_default()
                        .push(body);
                }
                // A definite rejection leaves no doubt: nothing is probed.
                Call::Probe { pair } => panic!("probe of {pair:?}"),
            }
        }
        assert_eq!(attempts.len(), TABLES.len());
        for (token, bodies) in attempts {
            assert_eq!(bodies.len(), 3, "{token}");
            assert!(bodies.windows(2).all(|w| w[0] == w[1]), "{token}");
        }
        assert_eq!(rig.rows(TableId::SpanCore), 4);
        node.host.stop().await;
    });
}

#[test]
fn once_in_doubt_every_send_follows_a_probe() {
    directed(|rig| async move {
        // Attempt 1 has an unknown outcome and never lands; attempts 2 and 3 are definite rejections.
        rig.model
            .set_script(Script::new(|_, attempt| match attempt {
                1 => Some(unknown()),
                2 | 3 => Some(retry()),
                _ => None,
            }));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 2).await;
        node.sleep(400).await;
        assert!(node.watermark() >= lsn.get());
        let by_pair = rig.calls_by_pair();
        assert_eq!(by_pair.len(), TABLES.len());
        for (pair, calls) in by_pair {
            // The rejections do not clear the doubt: a probe precedes each later send.
            assert_eq!(calls, "IPIPIPI", "{pair:?}");
        }
        assert_eq!(node.snapshot().unknown_outcomes, 4);
        assert_eq!(node.snapshot().probes_present, 0);
        assert_eq!(rig.rows(TableId::SpanCore), 2);
        node.host.stop().await;
    });
}

#[test]
fn an_insert_that_landed_unseen_is_found_by_the_probe() {
    directed(|rig| async move {
        // The block lands, and the caller is told nothing: the probe then reads present.
        rig.model.set_script(Script::new(|_, _| None));
        rig.model.set_faults(ChFaults {
            ack_lost_ppm: 1_000_000,
            ..ChFaults::default()
        });
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 2).await;
        node.sleep(300).await;
        assert!(node.watermark() >= lsn.get());
        for (pair, calls) in rig.calls_by_pair() {
            assert_eq!(calls, "IP", "{pair:?}");
        }
        assert_eq!(node.snapshot().probes_present, 4);
        assert_eq!(rig.rows(TableId::SpanCore), 2);
        node.host.stop().await;
    });
}

#[test]
fn a_restart_probes_every_pair_and_resends_with_the_same_token() {
    directed(|rig| async move {
        // `span_payload` never answers; the other tables land. Then the node crashes.
        rig.model.set_script(Script::new(|token, _| {
            (token.table() == TableId::SpanPayload).then(retry)
        }));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 3).await;
        node.sleep(200).await;
        assert_eq!(node.watermark(), 0);
        assert_eq!(rig.rows(TableId::SpanCore), 3);
        assert_eq!(rig.rows(TableId::SpanPayload), 0);
        let before: Vec<BatchToken> = rig
            .model
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Insert { token, .. } => Some(token),
                Call::Probe { .. } => None,
            })
            .collect();
        let Node { host, wal, .. } = node;
        drop(wal);
        host.crash().await;

        rig.model.set_script(Script::new(|_, _| None));
        let (node, report) = rig.boot().await;
        assert_eq!(report.unchecked_seals.len(), 1);
        node.sleep(300).await;
        assert!(node.watermark() >= lsn.get());
        assert_eq!(node.snapshot().reshipped, 1);
        // Everything before a restart is in doubt: every pair is probed; only the missing one is sent, with
        // the token of the first incarnation.
        assert_eq!(node.snapshot().probes, 4);
        assert_eq!(node.snapshot().probes_present, 3);
        assert_eq!(node.snapshot().inserts, 1);
        let resent: Vec<BatchToken> = rig
            .model
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Insert { token, .. } => Some(token),
                Call::Probe { .. } => None,
            })
            .collect();
        assert_eq!(resent.len(), before.len() + 1);
        let last = resent.last().unwrap();
        assert_eq!(last.table(), TableId::SpanPayload);
        assert!(before.contains(last));
        assert_eq!(rig.rows(TableId::SpanCore), 3);
        assert_eq!(rig.rows(TableId::SpanPayload), 3);
        assert!(
            rig.model.violations().is_empty(),
            "{:?}",
            rig.model.violations()
        );
        node.host.stop().await;
    });
}

#[test]
fn a_poison_pair_is_quarantined_after_three_identical_failures() {
    directed(|rig| async move {
        rig.model.set_script(Script::new(|token, _| {
            (token.table() == TableId::TraceHeads).then(|| InsertError::Poison {
                code: 27,
                message: "cannot parse\ninput".to_owned(),
            })
        }));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 2).await;
        node.sleep(300).await;
        // The quarantined pair is resolved: the watermark passes the batch, and the other pairs landed.
        assert!(node.watermark() >= lsn.get());
        assert_eq!(node.snapshot().quarantined, 1);
        assert_eq!(rig.rows(TableId::SpanCore), 2);
        assert_eq!(rig.rows(TableId::TraceHeads), 0);
        let by_pair = rig.calls_by_pair();
        // Never in doubt: three sends, no probe.
        assert_eq!(by_pair[&(TableId::TraceHeads, lsn.get())], "III");

        // The copy: the frames of the batch and a manifest.
        let files = rig.disk.files(Root::Quarantine);
        assert_eq!(files.len(), 2, "{files:?}");
        let dir = files[0].as_str().rsplit_once('/').unwrap().0.to_owned();
        let token = rig
            .model
            .calls()
            .into_iter()
            .find_map(|call| match call {
                Call::Insert { token, .. } if token.table() == TableId::TraceHeads => Some(token),
                _ => None,
            })
            .unwrap();
        assert_eq!(dir, token.text().as_str().replace(':', "_"));
        let read = |name: &str| {
            let path = DiskPath::new(Root::Quarantine, &format!("{dir}/{name}")).unwrap();
            rig.disk.read_file(&path).unwrap()
        };
        let manifest = String::from_utf8(read(MANIFEST_FILE)).unwrap();
        for line in [
            "version=1".to_owned(),
            format!("first_lsn={}", token.first_lsn().get()),
            format!("last_lsn={}", token.last_lsn().get()),
            "table=trace_heads".to_owned(),
            "table_id=10".to_owned(),
            format!("partition={}", token.partition()),
            format!("token={}", token.text().as_str()),
            "error_code=27".to_owned(),
            "attempts=3".to_owned(),
            "reason=cannot parse input".to_owned(),
            format!("bytes={}", read(FRAMES_FILE).len()),
        ] {
            assert!(
                manifest.lines().any(|l| l == line),
                "no `{line}` in\n{manifest}"
            );
        }

        // The `Quarantine` frame names the pair, and a restart keeps it resolved.
        let Node { host, wal, .. } = node;
        let mut replay = wal.replay(Lsn::FIRST);
        let mut named = false;
        while let Some(frame) = replay.next().await.unwrap() {
            if frame.record_type() == RecordType::Quarantine {
                let payload = frame.payload().unwrap();
                let Record::Quarantine(q) =
                    Record::decode(RecordType::Quarantine, &payload).unwrap()
                else {
                    panic!("not a quarantine");
                };
                assert_eq!(
                    (q.first_lsn, q.last_lsn),
                    (token.first_lsn(), token.last_lsn())
                );
                assert_eq!(q.pairs.len(), 1);
                assert_eq!(q.pairs[0].table_id, TableId::TraceHeads);
                assert_eq!((q.error_code, q.attempts), (27, 3));
                assert_eq!(q.quarantine_ref, dir);
                named = true;
            }
        }
        assert!(named);
        drop(wal);
        host.crash().await;
        let inserts = rig.model.stats().inserts;
        let (node, _) = rig.boot().await;
        node.sleep(200).await;
        assert_eq!(rig.model.stats().inserts, inserts);
        assert_eq!(rig.rows(TableId::TraceHeads), 0);
        node.host.stop().await;
    });
}

#[test]
fn a_block_that_spans_partitions_is_quarantined_at_once() {
    directed(|rig| async move {
        // Code 252 in the poison class is the multi-partition refusal: a routing bug that no retry cures.
        rig.model.set_script(Script::new(|token, _| {
            (token.table() == TableId::TraceStats).then(|| InsertError::Poison {
                code: 252,
                message: "Too many partitions for single INSERT block".to_owned(),
            })
        }));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 2).await;
        node.sleep(300).await;
        assert!(node.watermark() >= lsn.get());
        assert_eq!(node.snapshot().quarantined, 1);
        assert_eq!(rig.calls_by_pair()[&(TableId::TraceStats, lsn.get())], "I");
        node.host.stop().await;
    });
}

#[test]
fn a_pair_in_doubt_is_quarantined_only_after_an_absent_probe() {
    directed(|rig| async move {
        rig.model.set_script(Script::new(|token, attempt| {
            if token.table() != TableId::SpanPayload {
                return None;
            }
            Some(if attempt == 1 {
                unknown()
            } else {
                InsertError::Poison {
                    code: 53,
                    message: "type mismatch".to_owned(),
                }
            })
        }));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 1).await;
        node.sleep(300).await;
        assert!(node.watermark() >= lsn.get());
        assert_eq!(node.snapshot().quarantined, 1);
        // Unknown, then three data errors, each send after a probe, and one more probe before the quarantine.
        assert_eq!(
            rig.calls_by_pair()[&(TableId::SpanPayload, lsn.get())],
            "IPIPIPIP"
        );
        node.host.stop().await;
    });
}

#[test]
fn a_configuration_error_stops_the_batch_and_keeps_the_wal() {
    directed(|rig| async move {
        rig.model.set_script(Script::new(|token, _| {
            (token.table() == TableId::SpanCore).then(|| InsertError::FatalConfig {
                code: 497,
                message: "not enough privileges".to_owned(),
            })
        }));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 2).await;
        node.sleep(2_000).await;
        // Nothing is checkpointed, nothing is retried, the other tables landed, the shipper is up.
        assert_eq!(node.watermark(), 0);
        assert_eq!(node.ended(), None);
        assert_eq!(rig.calls_by_pair()[&(TableId::SpanCore, lsn.get())], "I");
        assert_eq!(rig.rows(TableId::SpanPayload), 2);
        assert_eq!(node.wal.segments().len(), 1);
        let Node { host, wal, .. } = node;
        drop(wal);
        host.crash().await;

        // The operator fixes the grant and restarts: the batch ships, probing first.
        rig.model.set_script(Script::new(|_, _| None));
        let (node, _) = rig.boot().await;
        node.sleep(300).await;
        assert!(node.watermark() >= lsn.get());
        assert_eq!(rig.rows(TableId::SpanCore), 2);
        assert_eq!(rig.rows(TableId::SpanPayload), 2);
        node.host.stop().await;
    });
}

#[test]
fn rows_carry_the_batch_they_were_sealed_into() {
    directed(|rig| async move {
        rig.model.set_script(Script::new(|_, _| None));
        let (node, _) = rig.boot().await;
        let lsn = node.ingest(1, 2).await;
        node.sleep(200).await;
        let parts = rig.model.parts(TableId::SpanCore);
        assert_eq!(parts.len(), 1);
        let part = &parts[0];
        assert_eq!(part.id.batch_seq, part.token.first_lsn().get());
        for row in 0..part.block.rows {
            assert_eq!(u64_at(&part.block, "lsn", row), Some(lsn.get()));
            assert_eq!(
                u64_at(&part.block, "batch_seq", row),
                Some(part.token.first_lsn().get())
            );
        }
        node.host.stop().await;
    });
}

/// The `phase` labels of `crucible_shipper_task_microseconds_total`.
const PHASES: [&str; 8] = [
    "read",
    "stage_a",
    "stage_b",
    "rows",
    "encode",
    "wal_append",
    "wait_inserts",
    "wait_frames",
];

#[test]
fn the_shipper_task_time_adds_up_and_every_insert_is_timed() {
    directed(|rig| async move {
        // Every insert answers after exactly 100 ms, and a batch is full at four rows: with two batches in
        // flight, the third waits for an insert to finish.
        rig.model.set_script(Script::new(|_, _| None));
        rig.model.set_faults(ChFaults {
            latency_micros: (100_000, 100_000),
            ..ChFaults::default()
        });
        let (node, _) = rig
            .boot_with(ShipperConfig {
                seal_rows: 4,
                ..config()
            })
            .await;
        let start = node.host.env.clock.now();
        for tag in 1..=4 {
            node.ingest(tag, 4).await;
        }
        node.sleep(1_000).await;
        let elapsed = node.host.env.clock.now().saturating_duration_since(start);
        let elapsed = u64::try_from(elapsed.as_micros()).unwrap();

        let recorder = &node.host.recorder;
        let phase = |name: &str| recorder.counter_value(&registry::SHIPPER_TASK_TIME, &[name]);
        let charged: u64 = PHASES.iter().map(|name| phase(name)).sum();
        // Every moment of the task's run is in exactly one phase; only the wait in progress, at most one tail
        // poll long, is not charged yet.
        assert!(
            charged <= elapsed && elapsed - charged <= 1_000,
            "charged {charged} of {elapsed} us"
        );
        assert!(phase("wait_inserts") >= 50_000, "{}", phase("wait_inserts"));
        assert!(phase("wait_frames") > 0);

        // Four batches of four pairs, each insert timed at exactly the store's latency.
        let inserts = rig
            .model
            .calls()
            .iter()
            .filter(|call| matches!(call, Call::Insert { .. }))
            .count() as u64;
        assert_eq!(inserts, 16);
        let timed = recorder
            .histogram_value(&registry::SHIPPER_INSERT_DURATION, &[])
            .unwrap();
        assert_eq!((timed.count, timed.sum), (inserts, inserts * 100_000));
        node.host.stop().await;
    });
}
