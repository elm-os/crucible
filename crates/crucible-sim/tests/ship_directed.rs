//! Directed tests of `crucible-shipper` over `crucible-wal` on `SimDisk` and the store model: one scripted
//! situation each, with the calls the store received checked in order (contract sections 5.1 and 5.4 to 5.7).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_env::{
    DiskPath, InsertError, InsertSettings, RetryReason, Root, StoreFlavour, Stores,
};
use crucible_normalize::{NORMALIZER_VERSION, Tombstone, TombstoneSelector};
use crucible_records::{
    AuthorKind, BatchToken, DeletionRequestId, DeletionState, FactId, FactTargetKind, Lsn,
    PartitionId, ProjectId, ScoreDataType, ScoreSource, SpanId, TableId, TagOp, Timestamp, TraceId,
    Uuid,
};
use crucible_schema::Granularity;
use crucible_shipper::{
    Barrier, Barriers, DeletionRequests, DeletionsUnavailable, FRAMES_FILE, FrameManifest, Hooks,
    KnownDeletion, MANIFEST_FILE, Mutant, ShipError, Shipper, ShipperConfig, StatsSnapshot,
    frame_quarantine_ref,
};
use crucible_sim::ch_model::{Call, ChFaults, ChModel, Script, u64_at};
use crucible_sim::ship_sim::{
    QuarantineFault, SpanSpec, named_pairs, read_quarantines, request_payload,
};
use crucible_sim::{Host, SIM_EPOCH_MICROS, Sim, SimDisk, run};
use crucible_wal::{RecoveryReport, Wal, WalConfig};
use crucible_wal_codec::{
    Compression, FRAME_HEADER_LEN, FrameHeader, PreparedFrame, Record, RecordType,
};
use futures::future::{self, BoxFuture};

/// The tables a span without a session or a user yields rows in.
const TABLES: [TableId; 5] = [
    TableId::SpanCore,
    TableId::SpanPayload,
    TableId::TraceHeads,
    TableId::TraceStats,
    TableId::SpanMetrics,
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
        self.ingest_spans(&root_spans(tag, spans)).await
    }

    /// Appends one request with these spans and returns its LSN once durable.
    async fn ingest_spans(&self, specs: &[SpanSpec]) -> Lsn {
        let payload = request_payload([1; 16], specs, 0).unwrap();
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

/// `spans` root spans of the traces of `tag`.
fn root_spans(tag: u8, spans: u8) -> Vec<SpanSpec> {
    (0..spans)
        .map(|i| {
            (
                [tag; 16],
                [tag, i, 1, 1, 1, 1, 1, 1],
                SIM_EPOCH_MICROS + i64::from(i),
                true,
            )
        })
        .collect()
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
        self.boot_retaining(WalConfig::default().replay_retention)
            .await
    }

    /// As [`Rig::boot`], with a WAL that keeps checkpointed segments for `replay_retention`.
    async fn boot_retaining(&self, replay_retention: Duration) -> (Node, RecoveryReport) {
        self.boot_with(replay_retention, config(), Hooks::default())
            .await
    }

    /// As [`Rig::boot_retaining`], with this shipper configuration and these hooks.
    async fn boot_with(
        &self,
        replay_retention: Duration,
        config: ShipperConfig,
        hooks: Hooks,
    ) -> (Node, RecoveryReport) {
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
            replay_retention,
            ..WalConfig::default()
        };
        let (wal, report) = Box::pin(Wal::open(&host.env, wal_config)).await.unwrap();
        let stores = Stores::new(Arc::new(self.model.clone()));
        let shipper =
            Shipper::new(&host.env, &stores, wal.clone(), &report, config, hooks).unwrap();
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

    /// The spans an aggregating table counts: the sum of its count column, which is zero in a marker row.
    fn counted(&self, table: TableId, column: &str) -> u64 {
        self.model
            .parts(table)
            .iter()
            .flat_map(|p| (0..p.block.rows).map(|row| u64_at(&p.block, column, row).unwrap()))
            .sum()
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
    directed_seeded(7, body);
}

fn directed_seeded<F, Fut>(seed: u64, body: F) -> Fut::Output
where
    F: FnOnce(Rig) -> Fut,
    Fut: Future,
{
    run(seed, |sim| {
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
    })
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
        assert_eq!(node.snapshot().unknown_outcomes, TABLES.len() as u64);
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
        assert_eq!(node.snapshot().probes_present, TABLES.len() as u64);
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
        assert_eq!(node.snapshot().probes, TABLES.len() as u64);
        assert_eq!(node.snapshot().probes_present, TABLES.len() as u64 - 1);
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

/// A batch whose pairs land on both sides of a restart holds the same spans in every table, although the WAL
/// collected garbage in between: the log keeps what the batch's dropped repeat relied on until the batch is
/// checkpointed, so the restart rebuilds the duplicate window to drop the repeat again. Without that the
/// aggregating tables, which land after the restart, count the repeat that `span_core` does not hold.
#[test]
fn a_restart_drops_the_repeats_that_the_first_attempt_dropped() {
    directed(|rig| async move {
        let retention = Duration::from_secs(60);
        // A request, then others until its segment is closed and wholly checkpointed: only replay retention
        // keeps that segment now.
        let (node, _) = rig.boot_retaining(retention).await;
        let first = node.ingest(1, 3).await;
        let mut tag = 10;
        while node.wal.segments().len() < 3 {
            node.ingest(tag, 250).await;
            tag += 1;
        }
        node.sleep(500).await;
        let segments = node.wal.segments();
        assert!(segments[0].first_lsn <= first && first < segments[1].first_lsn);
        assert!(segments[1].first_lsn.get() <= node.watermark());
        let shipped = rig.rows(TableId::SpanCore);
        assert_eq!(rig.counted(TableId::SpanMetrics, "count"), shipped as u64);

        // One request repeats the three spans of the first and adds two. `span_metrics` never answers: the
        // other tables land without the repeats, and the batch stays unchecked.
        rig.model.set_script(Script::new(|token, _| {
            (token.table() == TableId::SpanMetrics).then(retry)
        }));
        let mut spans = root_spans(1, 3);
        spans.extend(root_spans(2, 5).split_off(3));
        let repeat = node.ingest_spans(&spans).await;
        node.sleep(200).await;
        assert_eq!(rig.rows(TableId::SpanCore), shipped + 2);
        assert_eq!(rig.counted(TableId::SpanMetrics, "count"), shipped as u64);

        // Replay retention ends. The first request's segment stays, because the unchecked batch relies on it.
        node.sleep(retention.as_millis() as u64 + 1_000).await;
        let kept = node.wal.segments()[0].first_lsn <= first;
        let Node { host, wal, .. } = node;
        drop(wal);
        host.crash().await;

        rig.model.set_script(Script::new(|_, _| None));
        let (node, report) = rig.boot_retaining(retention).await;
        assert_eq!(report.unchecked_seals.len(), 1);
        node.sleep(300).await;
        assert!(node.watermark() >= repeat.get());
        assert_eq!(node.snapshot().reshipped, 1);
        // Every table holds the two new spans and no repeat.
        assert_eq!(rig.rows(TableId::SpanCore), shipped + 2);
        assert_eq!(
            rig.counted(TableId::SpanMetrics, "count"),
            shipped as u64 + 2
        );
        assert_eq!(
            rig.counted(TableId::TraceStats, "span_count"),
            shipped as u64 + 2
        );
        assert!(kept, "the segment of the first request was collected");
        assert!(
            rig.model.violations().is_empty(),
            "{:?}",
            rig.model.violations()
        );
        // Once the batch is checkpointed nothing relies on the segment any more.
        node.sleep(100).await;
        assert!(node.wal.segments()[0].first_lsn > first);
        node.host.stop().await;
    });
}

/// A garbage collection that fails holds the admission of frames: the hint cannot be replaced, so the
/// collection after a checkpoint fails before it removes anything. A request acknowledged meanwhile stays in
/// the log. The collection of the next tail poll after the disk heals succeeds, and the request lands.
#[test]
fn a_collection_that_cannot_write_the_hint_holds_admission_until_one_succeeds() {
    directed(|rig| async move {
        let (node, _) = rig.boot().await;
        node.ingest(1, 3).await;
        node.sleep(100).await;
        assert_eq!(rig.rows(TableId::SpanCore), 3);

        rig.disk.fail_renames(true);
        let checked = node.ingest(2, 3).await;
        node.sleep(100).await;
        // The batch is checkpointed; the collection that follows its checkpoint is the one that fails.
        assert!(node.watermark() >= checked.get());
        assert_eq!(rig.rows(TableId::SpanCore), 6);
        let held = node.ingest(3, 3).await;
        node.sleep(200).await;
        assert_eq!(node.ended(), None);
        assert_eq!(rig.rows(TableId::SpanCore), 6);
        assert!(node.watermark() < held.get());

        rig.disk.fail_renames(false);
        node.sleep(100).await;
        assert_eq!(rig.rows(TableId::SpanCore), 9);
        assert_eq!(rig.rows(TableId::SpanPayload), 9);
        assert!(node.watermark() >= held.get());
        assert!(
            rig.model.violations().is_empty(),
            "{:?}",
            rig.model.violations()
        );
        node.host.stop().await;
    });
}

/// The same for a removal that is not durable: the segment is unlinked and the directory sync fails, so a
/// restart may or may not find the segment, and stage B cannot say what a restart would remember. No frame
/// is admitted until the removal is durable.
#[test]
fn a_removal_that_is_not_durable_holds_admission_until_it_is() {
    directed(|rig| async move {
        let retention = Duration::from_secs(60);
        let (node, _) = rig.boot_retaining(retention).await;
        let mut tag = 10;
        while node.wal.segments().len() < 3 {
            node.ingest(tag, 250).await;
            tag += 1;
        }
        node.sleep(500).await;
        let segments = node.wal.segments();
        assert!(segments[1].first_lsn.get() <= node.watermark());
        let oldest = segments[0].segment_no;
        let shipped = rig.rows(TableId::SpanCore);

        // Replay retention ends while directory syncs fail: the oldest segment is unlinked, not durably.
        rig.disk.fail_dir_syncs(true);
        node.sleep(retention.as_millis() as u64 + 1_000).await;
        assert_eq!(node.wal.segments()[0].segment_no, oldest);
        let held = node.ingest(1, 3).await;
        node.sleep(200).await;
        assert_eq!(node.ended(), None);
        assert_eq!(rig.rows(TableId::SpanCore), shipped);
        assert!(node.watermark() < held.get());

        rig.disk.fail_dir_syncs(false);
        node.sleep(100).await;
        assert!(node.wal.segments()[0].segment_no > oldest);
        assert_eq!(rig.rows(TableId::SpanCore), shipped + 3);
        assert_eq!(rig.rows(TableId::SpanPayload), shipped + 3);
        assert!(node.watermark() >= held.get());
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

/// The store refuses every pair as a data error, except that it accepts the first attempt of each and keeps it
/// in flight past the end of the test: from its first answer on, every pair is in doubt.
fn poison_in_flight(model: &ChModel) {
    model.set_faults(ChFaults {
        poison_pair_ppm: 1_000_000,
        poison_first_in_flight_ppm: 1_000_000,
        late_apply_micros: (3_600_000_000, 3_600_000_000),
        ..ChFaults::default()
    });
}

/// A crash at `barrier` (contract 5.9): from there on no barrier completes and the disk is frozen, so that
/// nothing after it is done, and the test crashes the host.
#[derive(Debug)]
struct CrashAt {
    disk: SimDisk,
    barrier: Barrier,
    tripped: AtomicBool,
}

impl CrashAt {
    fn new(disk: &SimDisk, barrier: Barrier) -> Arc<Self> {
        Arc::new(Self {
            disk: disk.clone(),
            barrier,
            tripped: AtomicBool::new(false),
        })
    }

    /// Runs the node until it reaches the barrier, then crashes it.
    async fn crash(&self, node: Node) {
        while !self.tripped.load(Ordering::SeqCst) {
            assert_eq!(node.ended(), None);
            node.sleep(1).await;
        }
        let Node { host, wal, .. } = node;
        drop(wal);
        host.crash().await;
    }
}

impl Barriers for CrashAt {
    fn reached(&self, barrier: Barrier) -> BoxFuture<'static, ()> {
        if barrier == self.barrier || self.tripped.load(Ordering::SeqCst) {
            self.tripped.store(true, Ordering::SeqCst);
            self.disk.freeze();
            return Box::pin(future::pending());
        }
        Box::pin(future::ready(()))
    }
}

/// The deletion requests of a control store.
#[derive(Debug)]
struct Requests(Vec<KnownDeletion>);

impl DeletionRequests for Requests {
    fn configured(&self) -> bool {
        true
    }

    fn requested_after(
        &self,
        received_at: Timestamp,
    ) -> BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>> {
        let known = self
            .0
            .iter()
            .filter(|known| known.tombstone.requested_at > received_at)
            .cloned()
            .collect();
        Box::pin(future::ready(Ok(known)))
    }

    fn request(
        &self,
        project: ProjectId,
        id: DeletionRequestId,
    ) -> BoxFuture<'_, Result<Option<KnownDeletion>, DeletionsUnavailable>> {
        let found = self
            .0
            .iter()
            .find(|known| known.tombstone.project_id == project && known.tombstone.request_id == id)
            .cloned();
        Box::pin(future::ready(Ok(found)))
    }
}

/// The pairs of the quarantine directories whose manifest is whole.
fn copied_pairs(disk: &SimDisk) -> Vec<(TableId, PartitionId)> {
    disk.files(Root::Quarantine)
        .iter()
        .filter(|path| path.as_str().ends_with(&format!("/{MANIFEST_FILE}")))
        .filter_map(|path| {
            let text = String::from_utf8(disk.read_file(path)?).ok()?;
            let field = |name: &str| {
                text.lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            };
            field("bytes")?;
            let table = TableId::from_id(field("table_id")?.parse().ok()?)?;
            Some((table, PartitionId::parse(field("partition")?).ok()?))
        })
        .collect()
}

/// The run of seed 44853202051001 (config `0xea716b1a2a1c1c`), shortened. The first attempt of a poison pair
/// stays in flight; the pair is probed absent before each of its three data errors and before its copy, and
/// the node crashes at `quarantine.copied`, before the `Quarantine` frame. The restart sends the pair again
/// after an absent probe, gets a data error and crashes. A request that completed while the node was down
/// deletes the batch's trace, so the next restart leaves no row in any pair of the batch: it checkpoints the
/// batch without a call to the store, and the copy stays (contracts 5.3 and 5.9). No `Quarantine` frame names
/// the pair, so the copy is no quarantine, whatever the store answered last: here a data error after an
/// accepted attempt, which the system simulation read as a quarantine in doubt (invariant I2).
#[test]
fn a_copy_that_no_quarantine_frame_names_is_no_quarantine() {
    directed(|rig| async move {
        let retain = Duration::from_secs(3_600);
        poison_in_flight(&rig.model);
        let crash = CrashAt::new(&rig.disk, Barrier::QuarantineCopied);
        let hooks = Hooks {
            barriers: crash.clone(),
            ..Hooks::default()
        };
        let (node, _) = rig.boot_with(retain, config(), hooks).await;
        let lsn = node.ingest(1, 2).await;
        crash.crash(node).await;
        // The copies that were whole when the first of them reached the barrier.
        let copied = copied_pairs(&rig.disk);
        let Some(&(table, partition)) = copied.first() else {
            panic!("no whole copy");
        };

        let crash = CrashAt::new(&rig.disk, Barrier::InsertSent(table, partition));
        let hooks = Hooks {
            barriers: crash.clone(),
            ..Hooks::default()
        };
        let (node, report) = rig.boot_with(retain, config(), hooks).await;
        assert_eq!(report.unchecked_seals.len(), 1);
        crash.crash(node).await;

        let requested_at = SIM_EPOCH_MICROS + i64::try_from(rig.sim.elapsed().as_micros()).unwrap();
        let deletion = KnownDeletion {
            tombstone: Tombstone {
                request_id: DeletionRequestId::from_bytes([1; 16]),
                project_id: ProjectId::from_bytes([1; 16]),
                requested_at: Timestamp::from_micros(requested_at),
                deletion_seq: 1,
                selector: TombstoneSelector::Traces(vec![TraceId::from_bytes([1; 16])]),
            },
            state: DeletionState::Completed,
        };
        let calls = (rig.model.stats().inserts, rig.model.stats().probes);
        let hooks = Hooks {
            deletions: Arc::new(Requests(vec![deletion])),
            ..Hooks::default()
        };
        let (node, _) = rig.boot_with(retain, config(), hooks).await;
        node.sleep(300).await;
        let watermark = node.watermark();
        assert!(watermark >= lsn.get());
        assert_eq!((rig.model.stats().inserts, rig.model.stats().probes), calls);
        assert_eq!(copied_pairs(&rig.disk), copied);

        let named = named_in(&node.wal).await;
        assert!(named.is_empty(), "{named:?}");
        let (_, quarantined, faults) =
            read_quarantines(&rig.disk, &rig.model, watermark, Some(&named));
        assert!(quarantined.is_empty() && faults.is_empty(), "{faults:?}");
        // Read from the store's last answer alone, each copy is a quarantine in doubt.
        let (_, quarantined, faults) = read_quarantines(&rig.disk, &rig.model, watermark, None);
        assert_eq!(quarantined.len(), copied.len());
        assert!(
            faults
                .iter()
                .any(|fault| matches!(fault, QuarantineFault::InDoubt { table: t, .. } if *t == table.name())),
            "{faults:?}"
        );
        node.host.stop().await;
    });
}

/// The pairs that the `Quarantine` frames of the log name.
async fn named_in(wal: &Wal) -> BTreeSet<(u64, TableId, PartitionId)> {
    let mut replay = wal.replay(Lsn::FIRST);
    let mut named = BTreeSet::new();
    while let Some(frame) = replay.next().await.unwrap() {
        named.extend(named_pairs(&frame));
    }
    named
}

/// Contract 5.6 as the quarantine check reads it: a pair in doubt that the shipper quarantines after an absent
/// probe is a quarantine; one that it quarantines straight after its third data error, as the seeded defect
/// does, is a quarantine in doubt. Returns the quarantines the check counts and the faults it finds.
fn quarantined_after_data_errors(mutant: Option<Mutant>) -> (usize, Vec<QuarantineFault>) {
    directed_seeded(7, |rig| async move {
        poison_in_flight(&rig.model);
        let config = ShipperConfig { mutant, ..config() };
        let retain = Duration::from_secs(3_600);
        let (node, _) = rig.boot_with(retain, config, Hooks::default()).await;
        let lsn = node.ingest(1, 2).await;
        node.sleep(300).await;
        let watermark = node.watermark();
        assert!(watermark >= lsn.get());
        assert_eq!(node.snapshot().quarantined, TABLES.len() as u64);
        let named = named_in(&node.wal).await;
        assert_eq!(named.len(), TABLES.len());
        let (_, quarantined, faults) =
            read_quarantines(&rig.disk, &rig.model, watermark, Some(&named));
        node.host.stop().await;
        (quarantined.len(), faults)
    })
}

#[test]
fn a_pair_in_doubt_quarantined_without_an_absent_probe_is_a_quarantine_in_doubt() {
    let (counted, faults) = quarantined_after_data_errors(None);
    assert_eq!(counted, TABLES.len());
    assert!(faults.is_empty(), "{faults:?}");
    let (counted, faults) = quarantined_after_data_errors(Some(Mutant::QuarantineWithoutProbe));
    assert_eq!(counted, TABLES.len());
    assert_eq!(faults.len(), TABLES.len(), "{faults:?}");
    assert!(
        faults
            .iter()
            .all(|fault| matches!(fault, QuarantineFault::InDoubt { batch_seq: 1, .. })),
        "{faults:?}"
    );
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

/// Contract 5.6, frame quarantine: a durable frame that does not decode is copied by itself with a manifest,
/// counted, and passed; the frames after it ship, the watermark passes it, and a restart finds the copy and
/// sends nothing.
#[test]
fn a_frame_that_does_not_decode_is_quarantined_and_shipping_continues() {
    directed(|rig| async move {
        let (node, _) = rig.boot().await;
        let garbage = vec![0xff; 40];
        let bad = PreparedFrame::new(
            RecordType::IngestRequest,
            garbage.clone(),
            Compression::None,
        );
        let bad_lsn = node.wal.append(bad.unwrap()).await.unwrap();
        let good_lsn = node.ingest(2, 3).await;
        node.sleep(300).await;
        assert_eq!(node.ended(), None, "the shipper stopped");
        assert!(node.watermark() >= good_lsn.get());
        assert_eq!(rig.rows(TableId::SpanCore), 3);
        assert_eq!(node.snapshot().quarantined_frames, 1);
        assert_eq!(node.snapshot().quarantined, 0);

        let dir = frame_quarantine_ref(node.wal.epoch(), bad_lsn);
        assert_eq!(dir, format!("frame-{}-{}", node.wal.epoch(), bad_lsn.get()));
        let file = |name: &str| {
            let path = DiskPath::new(Root::Quarantine, &format!("{dir}/{name}")).unwrap();
            rig.disk.read_file(&path).unwrap()
        };
        let manifest = String::from_utf8(file(MANIFEST_FILE)).unwrap();
        let parsed = FrameManifest::parse(&manifest).unwrap();
        assert_eq!(
            (parsed.lsn, parsed.project_id, parsed.facts),
            (bad_lsn, None, false)
        );
        for line in [
            "kind=frame",
            "record_type=ingest_request",
            "stage=decode",
            &format!("normalizer_version={NORMALIZER_VERSION}"),
            &format!("wal_epoch={}", node.wal.epoch()),
        ] {
            assert!(manifest.lines().any(|l| l == line), "{line} in {manifest}");
        }
        // The copy is the frame as the log holds it: its 36-byte header, then its stored payload.
        let frames = file(FRAMES_FILE);
        assert_eq!(frames.len() as u64, parsed.bytes);
        assert_eq!(&frames[36..], &garbage[..]);
        let inserts = rig.model.stats().inserts;
        node.host.stop().await;

        // The decision is recomputed on replay; the copy is found and nothing is sent or copied again.
        let (node, _) = rig.boot().await;
        node.sleep(200).await;
        assert_eq!(node.ended(), None);
        assert_eq!(rig.model.stats().inserts, inserts);
        assert_eq!(file(FRAMES_FILE), frames);
        assert_eq!(String::from_utf8(file(MANIFEST_FILE)).unwrap(), manifest);
        node.host.stop().await;
    });
}

/// Contract 3.8 and 6.4: the facts of a `FactBatch` frame land once in their fact tables, by the month of their
/// target time.
#[test]
fn a_fact_batch_frame_lands_in_the_fact_tables() {
    use crucible_wal_codec::{Fact, FactBatch, FactBody, ScoreBody};
    directed(|rig| async move {
        let (node, _) = rig.boot().await;
        let fact = |n: u8, target_micros: i64, body| Fact {
            fact_id: FactId::from_bytes([n; 16]),
            target_kind: FactTargetKind::Trace,
            trace_id: TraceId::from_bytes([9; 16]),
            span_id: SpanId::ZERO,
            session_id: "",
            target_time: Timestamp::from_micros(target_micros),
            key: "key",
            version: u64::from(n),
            retracted: n.is_multiple_of(2),
            created_at: Timestamp::from_micros(SIM_EPOCH_MICROS),
            body,
        };
        let score = FactBody::Score(ScoreBody {
            data_type: ScoreDataType::Numeric,
            value_num: 0.5,
            value_str: "",
            comment: "",
            source: ScoreSource::Api,
            metadata: "{}",
            config_id: None,
        });
        let earlier = SIM_EPOCH_MICROS - 40 * 86_400_000_000;
        let batch = FactBatch {
            project_id: ProjectId::from_bytes([1; 16]),
            author_kind: AuthorKind::User,
            author_id: Uuid::from_bytes([4; 16]),
            facts: vec![
                fact(1, SIM_EPOCH_MICROS, score),
                fact(2, SIM_EPOCH_MICROS, FactBody::Tag { op: TagOp::Add }),
                // Another month: another pair of the same table.
                fact(3, earlier, FactBody::Tag { op: TagOp::Remove }),
                fact(4, SIM_EPOCH_MICROS, FactBody::Flag { value: true }),
                fact(
                    5,
                    SIM_EPOCH_MICROS,
                    FactBody::Correction {
                        corrected: "1",
                        note: "",
                    },
                ),
            ],
        };
        let payload = Record::FactBatch(batch).encode().unwrap();
        let frame = PreparedFrame::new(RecordType::FactBatch, payload, Compression::Lz4);
        let lsn = node.wal.append(frame.unwrap()).await.unwrap();
        node.sleep(300).await;
        assert_eq!(node.ended(), None);
        assert!(node.watermark() >= lsn.get());
        let rows = [
            TableId::FactScores,
            TableId::FactTags,
            TableId::FactFlags,
            TableId::FactCorrections,
        ]
        .map(|table| (rig.model.parts(table).len(), rig.rows(table)));
        assert_eq!(rows, [(1, 1), (2, 2), (1, 1), (1, 1)]);
        assert!(
            rig.model.violations().is_empty(),
            "{:?}",
            rig.model.violations()
        );
        for part in rig.model.parts(TableId::FactTags) {
            assert_eq!(u64_at(&part.block, "lsn", 0), Some(lsn.get()));
            assert_eq!(u64_at(&part.block, "batch_seq", 0), Some(lsn.get()));
        }
        let inserts = rig.model.stats().inserts;
        node.host.stop().await;
        let (node, _) = rig.boot().await;
        node.sleep(200).await;
        assert_eq!(rig.model.stats().inserts, inserts);
        node.host.stop().await;
    });
}

/// Virtual time in which a node ships a frame through to its quarantine and its checkpoint.
const SHIPPING_MILLIS: u64 = 60;

/// What a quarantine copy is made of in the crash sweeps below.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Copied {
    /// A frame that does not decode: the frame quarantine.
    Frame,
    /// A pair that the store refuses as poison: the pair quarantine.
    Pair,
}

impl Copied {
    fn script(self, model: &ChModel) {
        if self == Self::Pair {
            model.set_script(Script::new(|token, _| {
                (token.table() == TableId::TraceHeads).then(|| InsertError::Poison {
                    code: 27,
                    message: "cannot parse".to_owned(),
                })
            }));
        }
    }

    /// Appends the frame whose shipping ends in the copy.
    async fn write(self, node: &Node) {
        match self {
            Self::Frame => {
                let bad = PreparedFrame::new(
                    RecordType::IngestRequest,
                    vec![0xff; 40],
                    Compression::None,
                );
                node.wal.append(bad.unwrap()).await.unwrap();
            }
            Self::Pair => {
                node.ingest(1, 2).await;
            }
        }
    }

    fn copies(self, node: &Node) -> u64 {
        match self {
            Self::Frame => node.snapshot().quarantined_frames,
            Self::Pair => node.snapshot().quarantined,
        }
    }
}

/// Whether `bytes` is one or more whole WAL frames, as a quarantine copy holds them.
fn whole_frames(bytes: &[u8]) -> bool {
    let mut at = 0;
    while at < bytes.len() {
        let header = bytes
            .get(at..at + FRAME_HEADER_LEN)
            .and_then(|header| FrameHeader::decode(header.try_into().ok()?).ok());
        let Some(header) = header else {
            return false;
        };
        let end = at + FRAME_HEADER_LEN + header.payload_len as usize;
        match bytes.get(at + FRAME_HEADER_LEN..end) {
            Some(stored) if header.payload_matches(stored) => at = end,
            _ => return false,
        }
    }
    !bytes.is_empty()
}

/// The quarantine directories whose manifest is whole (it ends with its `bytes` line), and those of them
/// whose frames file is not the whole copy the manifest declares.
fn whole_and_broken_copies(disk: &SimDisk) -> (usize, Vec<String>) {
    let mut whole = 0;
    let mut broken = Vec::new();
    for path in disk.files(Root::Quarantine) {
        let Some(dir) = path.as_str().strip_suffix(&format!("/{MANIFEST_FILE}")) else {
            continue;
        };
        let manifest = disk.read_file(&path).unwrap();
        let declared = String::from_utf8_lossy(&manifest)
            .lines()
            .find_map(|line| line.strip_prefix("bytes=")?.parse::<usize>().ok());
        let Some(declared) = declared else {
            continue;
        };
        whole += 1;
        let frames = DiskPath::new(Root::Quarantine, &format!("{dir}/{FRAMES_FILE}")).unwrap();
        let frames = disk.read_file(&frames).unwrap_or_default();
        if frames.len() != declared || !whole_frames(&frames) {
            broken.push(format!(
                "{dir}: the manifest declares {declared} bytes, the frames file holds {}",
                frames.len()
            ));
        }
    }
    (whole, broken)
}

/// Runs the node until the armed crash point of the disk is reached, then crashes it: first the append of
/// the frame of `copied`, if any, then [`SHIPPING_MILLIS`] of shipping. `false` when all of that ended before the
/// crash point; the node is then stopped. Returns the watermark the node had reached.
async fn run_to_crash(rig: &Rig, node: Node, copied: Option<Copied>) -> u64 {
    let reached = {
        let work = async {
            if let Some(copied) = copied {
                copied.write(&node).await;
            }
            node.sleep(SHIPPING_MILLIS).await;
        };
        tokio::select! {
            biased;
            () = rig.disk.crash_point_reached() => true,
            () = work => false,
        }
    };
    rig.disk.disarm_crash();
    let watermark = node.watermark();
    let Node { host, wal, .. } = node;
    drop(wal);
    if reached {
        host.crash().await;
    } else {
        host.stop().await;
    }
    watermark
}

/// The disk operations from the append of the frame until its copy is announced, in a run without a crash.
fn copy_operations(copied: Copied) -> u64 {
    directed_seeded(0, |rig| async move {
        copied.script(&rig.model);
        let (node, _) = rig.boot().await;
        let before = rig.disk.ops();
        copied.write(&node).await;
        while copied.copies(&node) == 0 {
            assert_eq!(node.ended(), None);
            node.sleep(1).await;
        }
        let operations = rig.disk.ops() - before;
        node.host.stop().await;
        operations
    })
}

/// Crashes the node at disk operation `first` after the frame is appended and, with `second`, the next
/// incarnation at that operation; then lets one more run to the end. After each of them, every whole manifest
/// must stand beside the whole copy it declares. Returns whether the first crash left a quarantine file, and
/// whether the incarnation of the second crash had passed the frame before it.
fn crash_in_the_copy(copied: Copied, seed: u64, first: u64, second: Option<u64>) -> (bool, bool) {
    directed_seeded(seed, |rig| async move {
        let check = |when: &str| {
            let (whole, broken) = whole_and_broken_copies(&rig.disk);
            assert!(
                broken.is_empty(),
                "{copied:?}, seed {seed}, crashes at operations {first} and {second:?}, {when}: {broken:?}"
            );
            whole
        };
        copied.script(&rig.model);
        let (node, _) = rig.boot().await;
        rig.disk.arm_crash_after_ops(first);
        run_to_crash(&rig, node, Some(copied)).await;
        check("after the first crash");
        let started = !rig.disk.files(Root::Quarantine).is_empty();
        let mut passed = false;
        if let Some(second) = second {
            let (node, _) = rig.boot().await;
            rig.disk.arm_crash_after_ops(second);
            passed = run_to_crash(&rig, node, None).await >= Lsn::FIRST.get();
            check("after the second crash");
        }

        let (node, report) = rig.boot().await;
        node.sleep(SHIPPING_MILLIS).await;
        assert_eq!(node.ended(), None);
        let whole = check("at the end");
        if report.last_lsn >= Lsn::FIRST {
            // The frame survived the crashes: its copy is whole and the watermark has passed it.
            assert_eq!(whole, 1, "{copied:?}, seed {seed}, {first} and {second:?}");
            assert!(node.watermark() >= Lsn::FIRST.get());
        }
        node.host.stop().await;
        (started, passed)
    })
}

/// Sweeps a crash over every disk operation of the copy, and a second crash over every operation of the copy
/// that the next incarnation repeats, for each of `seeds` seeds: the seed decides what each crash keeps.
fn sweep_crashes_in_the_copy(copied: Copied, seeds: u64) {
    let operations = copy_operations(copied);
    for seed in 0..seeds {
        for first in 1..=operations {
            if !crash_in_the_copy(copied, seed, first, None).0 {
                continue;
            }
            // Until the second crash comes after the checkpoint that passes the frame.
            let mut second = 1;
            while !crash_in_the_copy(copied, seed, first, Some(second)).1 {
                second += 1;
                assert!(
                    second < 8 * operations,
                    "{copied:?}, seed {seed}: no checkpoint"
                );
            }
        }
    }
}

/// Contracts 5.6 and 5.9: a manifest that reads whole names a copy that is durable. A crash keeps any part of
/// the directory entries that were not synced, so the copy syncs its directory, and with it the entry of the
/// frames file, before it fills in the manifest. Without that, a crash between the manifest's sync and the
/// directory's leaves a whole manifest beside no frames file.
#[test]
fn a_whole_frame_manifest_names_a_durable_copy_at_every_crash_point() {
    sweep_crashes_in_the_copy(Copied::Frame, 16);
}

/// The same for the copy of a quarantined pair.
#[test]
fn a_whole_pair_manifest_names_a_durable_copy_at_every_crash_point() {
    sweep_crashes_in_the_copy(Copied::Pair, 8);
}
