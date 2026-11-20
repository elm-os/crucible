//! Directed tests of stage B's deletion rules on one node with the real WAL and the real shipper (contract 13.3,
//! step 3): the frames after a `Tombstone` frame ship nothing the deletion selects that arrived before the
//! deletion's `requested_at`, and what arrived at or after it is kept; and the deletion requests of the control
//! store apply before anything ships, also those that completed while the node was down and so left no tombstone
//! in its log. The shipper reads only the requests made after what it ships arrived, again for a frame that
//! arrived before every frame it read for. A `Tombstone` frame also evicts what it selects from the recent-span
//! index, and stage B counts what it drops, by kind.
//!
//! Each scenario also runs with the seeded defect it guards against, which must let the deleted spans through.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ops::Range;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_auth::test_support::scope;
use crucible_env::metrics::registry;
use crucible_env::{StoreFlavour, Stores};
use crucible_normalize::{Tombstone, TombstoneSelector};
use crucible_plan::Value;
use crucible_recent::{
    RecentConfig, RecentIndex as Index, SpanRead, WalLocator, WalReadError, WalView,
};
use crucible_records::{
    ApiKeyId, AuthorKind, DeletionRequestId, DeletionState, FactId, FactTargetKind, Lsn, ProjectId,
    SpanId, TableId, Timestamp, TraceId, Uuid,
};
use crucible_schema::{Granularity, table};
use crucible_shipper::{
    DeletionRequests, DeletionsUnavailable, Hooks, KnownDeletion, Mutant, NoRecent, RecentHook,
    RecentIndex, Shipper, ShipperConfig,
};
use crucible_sim::ch_model::ChModel;
use crucible_sim::ship_sim::{SpanSpec, request_payload};
use crucible_sim::{Host, SIM_EPOCH_MICROS, run};
use crucible_wal::{RecoveryReport, Wal, WalConfig};
use crucible_wal_codec::{
    self as codec, Compression, ContentOrigin, IngestTransport, PreparedFrame, Record, RecordType,
};
use futures::future::BoxFuture;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

const PROJECT: [u8; 16] = [1; 16];

/// The deletion requests of a control store: a list the test fills, whose first reads may fail.
#[derive(Debug, Default)]
struct Requests {
    known: Mutex<Vec<KnownDeletion>>,
    /// Reads that fail before one answers.
    failures: AtomicU32,
    /// Reads that were answered.
    answered: AtomicU32,
    /// For each answered read, the arrival it was for and the requests it gave.
    reads: Mutex<Vec<(Timestamp, Vec<DeletionRequestId>)>>,
}

impl DeletionRequests for Requests {
    fn configured(&self) -> bool {
        true
    }

    fn requested_after(
        &self,
        received_at: Timestamp,
    ) -> BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>> {
        let failed = self
            .failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        // As the control store reads them: the requests made after the instant.
        let known: Vec<KnownDeletion> = self
            .known
            .lock()
            .unwrap()
            .iter()
            .filter(|known| known.tombstone.requested_at > received_at)
            .cloned()
            .collect();
        if !failed {
            self.answered.fetch_add(1, Ordering::SeqCst);
            let ids = known.iter().map(|k| k.tombstone.request_id).collect();
            self.reads.lock().unwrap().push((received_at, ids));
        }
        Box::pin(async move {
            if failed {
                Err(DeletionsUnavailable(
                    "the control store did not answer".to_owned(),
                ))
            } else {
                Ok(known)
            }
        })
    }

    fn request(
        &self,
        project: ProjectId,
        id: DeletionRequestId,
    ) -> BoxFuture<'_, Result<Option<KnownDeletion>, DeletionsUnavailable>> {
        let found = self
            .known
            .lock()
            .unwrap()
            .iter()
            .find(|known| known.tombstone.project_id == project && known.tombstone.request_id == id)
            .cloned();
        Box::pin(async move { Ok(found) })
    }
}

struct Node {
    host: Host,
    wal: Wal,
    report: RecoveryReport,
    ch: ChModel,
    requests: Arc<Requests>,
}

impl Node {
    async fn sleep(&self, d: Duration) {
        self.host.env.clock.sleep(d).await;
    }

    fn now(&self) -> Timestamp {
        self.host.env.clock.wall_now()
    }

    /// Starts the shipper, with a seeded defect when one is given.
    fn start(&self, mutant: Option<Mutant>) {
        self.start_with(mutant, Arc::new(NoRecent));
    }

    /// Starts the shipper with the recent-span index `recent` behind stage B.
    fn start_with(&self, mutant: Option<Mutant>, recent: Arc<dyn RecentIndex>) {
        let config = ShipperConfig {
            linger: Duration::from_millis(10),
            tail_poll: Duration::from_millis(1),
            backoff_min: Duration::from_millis(5),
            backoff_max: Duration::from_millis(50),
            mutant,
            ..ShipperConfig::default()
        };
        let hooks = Hooks {
            recent,
            deletions: Arc::clone(&self.requests) as Arc<dyn DeletionRequests>,
            ..Hooks::default()
        };
        let stores = Stores::new(Arc::new(self.ch.clone()));
        let shipper = Shipper::new(
            &self.host.env,
            &stores,
            self.wal.clone(),
            &self.report,
            config,
            hooks,
        )
        .unwrap();
        drop(self.host.env.spawn("shipper", async move {
            Box::pin(shipper.run()).await.unwrap();
        }));
    }

    /// Appends one request with the spans `spans` of the trace `tag` and returns its LSN once durable.
    async fn ingest(&self, tag: u8, spans: Range<u8>) -> Lsn {
        let specs: Vec<SpanSpec> = spans
            .map(|i| {
                (
                    [tag; 16],
                    [tag, i, 1, 1, 1, 1, 1, 1],
                    SIM_EPOCH_MICROS + i64::from(i),
                    i == 0,
                )
            })
            .collect();
        let payload = request_payload(PROJECT, &specs, 0).unwrap();
        let frame = PreparedFrame::new(RecordType::IngestRequest, payload, Compression::Lz4);
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    /// Appends one request with the spans `spans` of the trace `tag`, each an MCP operation (a server span that
    /// carries `mcp.method.name`), and returns its LSN once durable.
    async fn ingest_mcp(&self, tag: u8, spans: Range<u8>) -> Lsn {
        let wire: Vec<Span> = spans
            .map(|i| {
                let start = (SIM_EPOCH_MICROS + i64::from(i)) as u64 * 1_000;
                Span {
                    trace_id: vec![tag; 16],
                    span_id: vec![tag, i, 2, 2, 2, 2, 2, 2],
                    name: "tools/call search".to_owned(),
                    kind: 2,
                    start_time_unix_nano: start,
                    end_time_unix_nano: start + 1_000_000,
                    attributes: vec![KeyValue {
                        key: "mcp.method.name".to_owned(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("tools/call".to_owned())),
                        }),
                        ..KeyValue::default()
                    }],
                    ..Span::default()
                }
            })
            .collect();
        let count = wire.len() as u32;
        let otlp = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: None,
                scope_spans: vec![ScopeSpans {
                    scope: None,
                    spans: wire,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
        .encode_to_vec();
        let record = Record::IngestRequest(codec::IngestRequest {
            project_id: ProjectId::from_bytes(PROJECT),
            api_key_id: ApiKeyId::from_bytes([9; 16]),
            compat_dialect: 0,
            transport: IngestTransport::Http,
            content_origin: ContentOrigin::Protobuf,
            admin_import: false,
            sdk_name: "sim",
            sdk_version: "1",
            idempotency_key: &[],
            span_count: count,
            rejected: &[0],
            otlp: &otlp,
        });
        let frame = PreparedFrame::new(
            record.record_type(),
            record.encode().unwrap(),
            Compression::Lz4,
        );
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    /// Appends one `FactBatch` frame with a flag on each trace of `tags`, and returns its LSN once durable.
    async fn flag(&self, tags: &[u8]) -> Lsn {
        let at = Timestamp::from_micros(SIM_EPOCH_MICROS);
        let facts = tags
            .iter()
            .zip(1u64..)
            .map(|(&tag, n)| codec::Fact {
                fact_id: FactId::from_uuid(Uuid::from_v7_parts(1_800_000_000_000 + n, n.into())),
                target_kind: FactTargetKind::Trace,
                trace_id: TraceId::from_bytes([tag; 16]),
                span_id: SpanId::from_bytes([0; 8]),
                session_id: "",
                target_time: at,
                key: "reviewed",
                version: n,
                retracted: false,
                created_at: at,
                body: codec::FactBody::Flag { value: true },
            })
            .collect();
        let record = Record::FactBatch(codec::FactBatch {
            project_id: ProjectId::from_bytes(PROJECT),
            author_kind: AuthorKind::System,
            author_id: Uuid::from_v7_parts(1_800_000_000_000, 1),
            facts,
        });
        let frame = PreparedFrame::new(
            record.record_type(),
            record.encode().unwrap(),
            Compression::Lz4,
        );
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    /// The control store's request to delete the trace `tag`, requested at `requested_at`.
    fn deletion(tag: u8, requested_at: Timestamp, state: DeletionState) -> KnownDeletion {
        KnownDeletion {
            tombstone: Tombstone {
                request_id: DeletionRequestId::from_bytes([tag; 16]),
                project_id: ProjectId::from_bytes(PROJECT),
                requested_at,
                deletion_seq: u64::from(tag),
                selector: TombstoneSelector::Traces(vec![TraceId::from_bytes([tag; 16])]),
            },
            state,
        }
    }

    /// Appends the `Tombstone` frame of a deletion of the trace `tag` requested at `requested_at`.
    async fn tombstone(&self, tag: u8, requested_at: Timestamp) -> Lsn {
        let record = Record::Tombstone(codec::Tombstone {
            deletion_request_id: DeletionRequestId::from_bytes([tag; 16]),
            project_id: ProjectId::from_bytes(PROJECT),
            requested_at,
            deletion_seq: u64::from(tag),
            selector: codec::TombstoneSelector::Traces(vec![TraceId::from_bytes([tag; 16])]),
        });
        let frame = PreparedFrame::new(
            record.record_type(),
            record.encode().unwrap(),
            Compression::Lz4,
        );
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    /// The spans of the trace `tag` that the store holds in `span_core`.
    fn held(&self, tag: u8) -> usize {
        let at = table(TableId::SpanCore)
            .columns
            .iter()
            .position(|column| column.name == "trace_id")
            .unwrap();
        self.ch
            .held_rows(TableId::SpanCore)
            .iter()
            .filter(|(_, _, cells)| cells[at] == Value::Bytes(vec![tag; 16]))
            .count()
    }
}

/// One node: the real WAL on a simulated disk, the store model and a control store's deletion requests. The test
/// starts the shipper.
fn directed<F, Fut, T>(body: F) -> T
where
    F: FnOnce(Node) -> Fut,
    Fut: Future<Output = T>,
{
    run(13, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let ch = ChModel::new(
            sim.rng("store").as_ref(),
            StoreFlavour::SingleNode,
            Granularity::Day,
            Duration::from_secs(60),
        );
        let (wal, report) = Box::pin(Wal::open(&host.env, WalConfig::default()))
            .await
            .unwrap();
        body(Node {
            host,
            wal,
            report,
            ch,
            requests: Arc::default(),
        })
        .await
    })
}

/// A deletion accepted on a node whose clock runs ahead of this one: spans that reach this node after the
/// tombstone can still have arrived before `requested_at`, and the deletion selects them. Returns what trace 1 and
/// trace 2 hold after the tombstone, and trace 1 after `requested_at`.
fn after_a_tombstone(mutant: Option<Mutant>) -> ((usize, usize), usize) {
    directed(|node| async move {
        node.start(mutant);
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        assert_eq!(node.held(1), 2);

        let requested_at = node.now().saturating_add_micros(10_000_000);
        node.tombstone(1, requested_at).await;
        node.ingest(1, 2..4).await;
        node.ingest(2, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        let after_tombstone = (node.held(1), node.held(2));

        // Spans that arrive at or after `requested_at` are new data.
        node.sleep(Duration::from_secs(10)).await;
        node.ingest(1, 4..5).await;
        node.sleep(Duration::from_secs(1)).await;
        (after_tombstone, node.held(1))
    })
}

#[test]
fn the_frames_after_a_tombstone_ship_nothing_it_selects_that_arrived_before_it() {
    // The two spans of trace 1 that arrived after the tombstone are deleted; trace 2 is not.
    assert_eq!(after_a_tombstone(None), ((2, 2), 3));
    assert_eq!(
        after_a_tombstone(Some(Mutant::SkipTombstoneFilter)),
        ((4, 2), 5)
    );
}

/// The frames of traces 3 and 4 are in the log and were never shipped, and a request deleted trace 3 while the
/// node was down: it completed without this node, which never logged its tombstone. Returns what traces 3 and 4
/// hold once the shipper has run.
fn completed_while_down(mutant: Option<Mutant>) -> (usize, usize) {
    directed(|node| async move {
        node.ingest(3, 0..3).await;
        node.ingest(4, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        let deletion = Node::deletion(3, node.now(), DeletionState::Completed);
        node.requests.known.lock().unwrap().push(deletion);
        node.start(mutant);
        node.sleep(Duration::from_secs(2)).await;
        (node.held(3), node.held(4))
    })
}

#[test]
fn a_request_that_completed_while_the_node_was_down_applies_before_anything_ships() {
    assert_eq!(completed_while_down(None), (0, 2));
    // Reading only the open requests misses it, and the shipper inserts what it deleted.
    assert_eq!(
        completed_while_down(Some(Mutant::SeedOnlyOpenRequests)),
        (3, 2)
    );
    assert_eq!(
        completed_while_down(Some(Mutant::SkipTombstoneFilter)),
        (3, 2)
    );
}

#[test]
fn nothing_ships_until_the_deletion_requests_can_be_read() {
    let (before, after, answered) = directed(|node| async move {
        node.requests.failures.store(4, Ordering::SeqCst);
        node.ingest(5, 0..2).await;
        node.ingest(6, 0..2).await;
        let deletion = Node::deletion(
            5,
            node.now().saturating_add_micros(1),
            DeletionState::Pending,
        );
        node.requests.known.lock().unwrap().push(deletion);
        node.start(None);
        // The reads fail for a while: trace 6, which no request deletes, does not ship either.
        node.sleep(Duration::from_millis(12)).await;
        let before = (node.held(5), node.held(6));
        node.sleep(Duration::from_secs(2)).await;
        let after = (node.held(5), node.held(6));
        (before, after, node.requests.answered.load(Ordering::SeqCst))
    });
    assert_eq!(before, (0, 0));
    assert_eq!(after, (0, 2));
    assert_eq!(answered, 1);
}

/// The node's WAL as the recent-span index sees it; the test reads summaries only, never a span's record.
#[derive(Debug)]
struct View(Wal);

impl WalView for View {
    fn durable_lsn(&self) -> Lsn {
        self.0.durable_lsn()
    }

    fn read_span(&self, _project_id: ProjectId, _locator: WalLocator) -> SpanRead {
        Box::pin(async { Err(WalReadError::Gone) })
    }
}

#[test]
fn a_tombstone_evicts_what_it_selects_from_the_recent_index() {
    let (before, after) = directed(|node| async move {
        let clock = node.host.env.clock.clone();
        let config = RecentConfig::new(64 << 20, StoreFlavour::SingleNode);
        let view = Arc::new(View(node.wal.clone()));
        let (index, writer) =
            Index::rebuild(config, clock.clone(), view, Lsn::ZERO, std::iter::empty());
        node.start_with(None, Arc::new(RecentHook::new(writer, clock, Lsn::ZERO)));
        let scope = scope(ProjectId::from_bytes(PROJECT)).unwrap();
        let held = |tag: u8| index.trace(scope, TraceId::from_bytes([tag; 16])).0.len();
        node.ingest(8, 0..2).await;
        node.ingest(9, 0..1).await;
        // Well inside the 2 s visibility grace, after which the index lets the store serve them.
        node.sleep(Duration::from_millis(200)).await;
        let before = (held(8), held(9));
        node.tombstone(8, node.now().saturating_add_micros(1)).await;
        node.sleep(Duration::from_millis(200)).await;
        (before, (held(8), held(9)))
    });
    assert_eq!(before, (2, 1));
    // Trace 8's entries arrived before the deletion and go at its tombstone; trace 9 is not deleted.
    assert_eq!(after, (0, 1));
}

const DAY: i64 = 86_400_000_000;

/// The shipper reads only the requests that can select what it ships, those made after it arrived. A request that
/// completed thirty days ago, whose data is gone, selects none of the frames in the log, which arrived after it, so
/// the read leaves it out; a request that completed while the node was down is read and applied. Then the node's
/// wall clock steps back forty days: a frame that arrives now arrived before the old request by the clock, so the
/// request selects it, and the shipper reads again for that arrival before the frame ships.
#[test]
fn the_shipper_reads_only_the_requests_that_can_select_what_it_ships() {
    let old = DeletionRequestId::from_bytes([10; 16]);
    let recent = DeletionRequestId::from_bytes([11; 16]);
    let (started, stepped, reads) = directed(|node| async move {
        let long_ago = node.now().saturating_add_micros(-30 * DAY);
        let deletion = Node::deletion(10, long_ago, DeletionState::Completed);
        node.requests.known.lock().unwrap().push(deletion);
        // New spans of trace 10, which the old request does not select, and spans of trace 11, which a request
        // made while the node was down deletes.
        node.ingest(10, 0..2).await;
        node.ingest(11, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        let deletion = Node::deletion(11, node.now(), DeletionState::Completed);
        node.requests.known.lock().unwrap().push(deletion);
        node.start(None);
        node.sleep(Duration::from_secs(2)).await;
        let started = (node.held(10), node.held(11));
        node.host.clock.step_wall(-40 * DAY);
        node.ingest(10, 2..4).await;
        node.sleep(Duration::from_secs(2)).await;
        let reads = node.requests.reads.lock().unwrap().clone();
        (started, node.held(10), reads)
    });
    assert_eq!(started, (2, 0));
    // The spans that arrived before the old request by the clock are deleted; the two before them stay.
    assert_eq!(stepped, 2);
    // The first read, for the oldest frame, left the old request out. The frame that arrived before every frame
    // read for so far read again, down to its own arrival.
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert_eq!(reads[0].1, [recent]);
    assert!(reads[1].0 < reads[0].0, "{reads:?}");
    assert_eq!(reads[1].1, [old, recent]);
}

/// Stage B counts what it drops for a deletion, by kind: the spans of the deleted trace, their MCP operations and
/// the facts on the trace. What no request selects ships and is not counted.
#[test]
fn stage_b_counts_what_a_deletion_drops_by_kind() {
    let (counts, held) = directed(|node| async move {
        node.ingest_mcp(20, 0..3).await;
        node.ingest(21, 0..2).await;
        node.flag(&[20, 20, 21]).await;
        node.sleep(Duration::from_secs(1)).await;
        let deletion = Node::deletion(20, node.now(), DeletionState::Completed);
        node.requests.known.lock().unwrap().push(deletion);
        node.start(None);
        node.sleep(Duration::from_secs(2)).await;
        let drops = |kind| {
            node.host
                .recorder
                .counter_value(&registry::SHIPPER_DELETION_DROPS, &[kind])
        };
        (
            (drops("span"), drops("mcp_operation"), drops("fact")),
            (node.held(20), node.held(21)),
        )
    });
    assert_eq!(counts, (3, 3, 2));
    assert_eq!(held, (0, 2));
}
