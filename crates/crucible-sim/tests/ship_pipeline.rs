//! The shipper's pipeline against its sequential path (contracts 3.2, 5.2 and 5.3). Stage A of the frames read
//! ahead of stage B and the encoding of each block run as blocking jobs. The simulation's spawner runs them in
//! place, which is the sequential path. The spawner here keeps them instead and runs them later, from runner tasks,
//! in an order drawn from a seeded stream and each after a draw of virtual time, so they end out of order and
//! after the shipper task has moved on. For random sequences of frames the shipper must insert the same blocks,
//! token for token and byte for byte, either way.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_admission::{Offer, PermitPool};
use crucible_env::metrics::registry;
use crucible_env::{
    Clock, InsertError, Instant, RetryReason, Rng, ShutdownSignal, Spawner, StoreFlavour, Stores,
    TaskAbort,
};
use crucible_records::{
    ApiKeyId, AuthorKind, DeletionRequestId, FactId, FactTargetKind, Lsn, ProjectId, SpanId, TagOp,
    Timestamp, TraceId, Uuid,
};
use crucible_schema::Granularity;
use crucible_shipper::{Hooks, ShipError, Shipper, ShipperConfig, StatsSnapshot};
use crucible_sim::ch_model::{Call, ChModel, Script};
use crucible_sim::{SIM_EPOCH_MICROS, run};
use crucible_wal::{Wal, WalConfig};
use crucible_wal_codec::{
    Compression, ContentOrigin, Fact, FactBatch, FactBody, IngestRequest, IngestTransport,
    PreparedFrame, Record, RecordType, Tombstone, TombstoneSelector,
};
use futures::future::BoxFuture;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use proptest::prelude::*;
use prost::Message;
use tokio::sync::Notify;

const PROJECT: [u8; 16] = [1; 16];
const DAY_MICROS: i64 = 86_400_000_000;

/// The phase labels of `crucible_shipper_task_microseconds_total`.
const PHASES: [&str; 9] = [
    "read",
    "stage_a",
    "stage_b",
    "rows",
    "encode",
    "wal_append",
    "wait_inserts",
    "wait_frames",
    "wait_stage_a",
];

/// Blocking jobs a [`Deferred`] spawner keeps for its runners.
type Jobs = Arc<Mutex<Vec<Box<dyn FnOnce() + Send>>>>;

/// How the shipper's run ended, and when it started and stopped.
type Ended = Arc<Mutex<Option<(Result<(), ShipError>, Instant, Instant)>>>;

/// A spawner that runs tasks as the simulation's does but keeps every blocking job for its runners.
struct Deferred {
    inner: Arc<dyn Spawner>,
    jobs: Jobs,
    kept: Arc<Notify>,
}

impl fmt::Debug for Deferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deferred").finish_non_exhaustive()
    }
}

impl Deferred {
    fn new(inner: Arc<dyn Spawner>) -> Self {
        Self {
            inner,
            jobs: Arc::default(),
            kept: Arc::new(Notify::new()),
        }
    }

    /// A runner: takes a kept job drawn at random, lets up to `max_micros` of virtual time pass, runs it.
    fn runner(
        &self,
        rng: Arc<dyn Rng>,
        clock: Arc<dyn Clock>,
        max_micros: u64,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (jobs, kept) = (Arc::clone(&self.jobs), Arc::clone(&self.kept));
        async move {
            loop {
                let job = {
                    let mut jobs = jobs.lock().unwrap();
                    let len = jobs.len() as u64;
                    (len > 0).then(|| jobs.swap_remove((rng.next_u64() % len) as usize))
                };
                match job {
                    Some(job) => {
                        let micros = rng.next_u64() % max_micros;
                        clock.sleep(Duration::from_micros(micros)).await;
                        job();
                    }
                    None => kept.notified().await,
                }
            }
        }
    }
}

#[derive(Debug)]
struct Kept;

impl TaskAbort for Kept {
    fn abort(&self) {}
}

impl Spawner for Deferred {
    fn spawn_boxed(&self, name: &'static str, fut: BoxFuture<'static, ()>) -> Box<dyn TaskAbort> {
        self.inner.spawn_boxed(name, fut)
    }

    fn spawn_blocking_boxed(
        &self,
        _name: &'static str,
        f: Box<dyn FnOnce() + Send>,
    ) -> Box<dyn TaskAbort> {
        self.jobs.lock().unwrap().push(f);
        self.kept.notify_one();
        Box::new(Kept)
    }

    fn shutdown(&self) -> ShutdownSignal {
        self.inner.shutdown()
    }
}

/// One span of a generated request.
#[derive(Clone, Debug)]
struct SpanGen {
    /// Its trace, out of a few, so that traces run across requests and deletions select them.
    trace: u8,
    /// Its id within the trace, out of a few, so that requests repeat spans for the duplicate window.
    span: u8,
    /// Its parent within the trace, or none for a root.
    parent: Option<u8>,
    /// Days before the epoch it started, so that its rows route to several partitions.
    days_back: u8,
    /// Bytes of an attribute, so that a request can be large enough for its stage A to run as a job.
    note: u16,
    /// Input tokens of usage, which mark the span's ancestors (the usage descendant set).
    tokens: Option<u16>,
}

/// One frame of a generated log.
#[derive(Clone, Debug)]
enum Item {
    /// An OTLP request.
    Request(Vec<SpanGen>),
    /// A deletion of one trace.
    Deletion(u8),
    /// Tags on one trace.
    Tags(u8, u8),
}

fn span_gen() -> impl Strategy<Value = SpanGen> {
    (
        0u8..6,
        0u8..12,
        proptest::option::of(0u8..12),
        0u8..3,
        0u16..3_000,
        proptest::option::of(1u16..500),
    )
        .prop_map(|(trace, span, parent, days_back, note, tokens)| SpanGen {
            trace,
            span,
            parent: parent.filter(|p| *p != span),
            days_back,
            note,
            tokens,
        })
}

fn item() -> impl Strategy<Value = Item> {
    prop_oneof![
        8 => proptest::collection::vec(span_gen(), 1..24).prop_map(Item::Request),
        1 => (0u8..6).prop_map(Item::Deletion),
        1 => (0u8..6, 1u8..4).prop_map(|(trace, tags)| Item::Tags(trace, tags)),
    ]
}

fn span_id(trace: u8, span: u8) -> Vec<u8> {
    vec![trace, span, 1, 1, 1, 1, 1, 1]
}

fn attribute(key: &str, value: any_value::Value) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue { value: Some(value) }),
        key_strindex: 0,
    }
}

fn request(spans: &[SpanGen]) -> Vec<u8> {
    let wire = spans
        .iter()
        .map(|s| {
            let start = (SIM_EPOCH_MICROS - i64::from(s.days_back) * DAY_MICROS) as u64 * 1_000;
            let mut attributes = vec![attribute(
                "app.note",
                any_value::Value::StringValue("n".repeat(usize::from(s.note))),
            )];
            if let Some(tokens) = s.tokens {
                attributes.push(attribute(
                    "gen_ai.usage.input_tokens",
                    any_value::Value::IntValue(i64::from(tokens)),
                ));
            }
            Span {
                trace_id: vec![s.trace; 16],
                span_id: span_id(s.trace, s.span),
                parent_span_id: s.parent.map(|p| span_id(s.trace, p)).unwrap_or_default(),
                name: format!("op-{}", s.span % 3),
                kind: 1,
                start_time_unix_nano: start,
                end_time_unix_nano: start + 2_000_000,
                attributes,
                ..Span::default()
            }
        })
        .collect();
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
    Record::IngestRequest(IngestRequest {
        project_id: ProjectId::from_bytes(PROJECT),
        api_key_id: ApiKeyId::from_bytes([9; 16]),
        compat_dialect: 0,
        transport: IngestTransport::Http,
        content_origin: ContentOrigin::Protobuf,
        admin_import: false,
        sdk_name: "sim",
        sdk_version: "1",
        idempotency_key: &[],
        span_count: spans.len() as u32,
        rejected: &[],
        otlp: &otlp,
    })
    .encode()
    .unwrap()
}

/// The frames of a log of items: each with its record type and payload.
fn frames(items: &[Item]) -> Vec<(RecordType, Vec<u8>)> {
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let n = u8::try_from(index + 1).unwrap();
            let record = match item {
                Item::Request(spans) => return (RecordType::IngestRequest, request(spans)),
                // Requested a day after every frame of the log was received, so it selects every span of the
                // trace that stage B admits after it.
                Item::Deletion(trace) => Record::Tombstone(Tombstone {
                    deletion_request_id: DeletionRequestId::from_bytes([n; 16]),
                    project_id: ProjectId::from_bytes(PROJECT),
                    requested_at: Timestamp::from_micros(SIM_EPOCH_MICROS + DAY_MICROS),
                    deletion_seq: u64::from(n),
                    selector: TombstoneSelector::Traces(vec![TraceId::from_bytes([*trace; 16])]),
                }),
                Item::Tags(trace, tags) => Record::FactBatch(FactBatch {
                    project_id: ProjectId::from_bytes(PROJECT),
                    author_kind: AuthorKind::User,
                    author_id: Uuid::from_bytes([4; 16]),
                    facts: (0..*tags)
                        .map(|t| {
                            let mut id = [2; 16];
                            (id[0], id[1]) = (n, t);
                            Fact {
                                fact_id: FactId::from_bytes(id),
                                target_kind: FactTargetKind::Trace,
                                trace_id: TraceId::from_bytes([*trace; 16]),
                                span_id: SpanId::ZERO,
                                session_id: "",
                                target_time: Timestamp::from_micros(SIM_EPOCH_MICROS),
                                key: "tag",
                                version: u64::from(t) + 1,
                                retracted: false,
                                created_at: Timestamp::from_micros(SIM_EPOCH_MICROS),
                                body: FactBody::Tag { op: TagOp::Add },
                            }
                        })
                        .collect(),
                }),
            };
            (record.record_type(), record.encode().unwrap())
        })
        .collect()
}

/// How one run ships the log.
#[derive(Clone, Copy, Debug)]
struct Setup {
    /// Blocking jobs end out of order, after up to this many microseconds each; `None` runs them in place.
    deferred: Option<u64>,
    seal_rows: usize,
    cache_bytes: u64,
    /// The store refuses the first attempt of every pair with a retry, so every pair is sent twice.
    retry_first: bool,
    /// Ingest's share of the CPU budget has every permit taken for the whole run.
    ingest_busy: bool,
}

impl Setup {
    const fn in_place(seal_rows: usize) -> Self {
        Self {
            deferred: None,
            seal_rows,
            cache_bytes: crucible_shipper::DEFAULT_CACHE_BYTES,
            retry_first: false,
            ingest_busy: false,
        }
    }

    const fn deferred(seal_rows: usize, max_micros: u64) -> Self {
        Self {
            deferred: Some(max_micros),
            ..Self::in_place(seal_rows)
        }
    }
}

/// What one run of the shipper did.
#[derive(Debug)]
struct Shipped {
    /// Every insert, in token order: its token, the last LSN of its batch, and the BLAKE3 of its body.
    inserts: Vec<(String, u64, [u8; 32])>,
    /// The LSN of the log's last frame, before the shipper's own records.
    last: Lsn,
    stats: StatsSnapshot,
    /// The microseconds of each phase of the shipper task, and of its whole run.
    phases: Vec<(&'static str, u64)>,
    ran: u64,
    /// The jobs whose permit wait the CPU budget recorded, of stage A and of encoding.
    jobs: (u64, u64),
}

impl Shipped {
    fn tokens(&self) -> Vec<&str> {
        self.inserts.iter().map(|(t, _, _)| t.as_str()).collect()
    }

    /// The inserts of the batches that end within the log. The batch that ends after the log's last frame also
    /// holds the shipper's own records, which the two runs append at different times, so the marker rows of
    /// its blocks carry different times (contract 5.3, `marker_at`).
    fn within_log(&self) -> Vec<&(String, u64, [u8; 32])> {
        self.inserts
            .iter()
            .filter(|(_, last, _)| *last <= self.last.get())
            .collect()
    }

    /// Whether every attempt of a pair sent the same bytes (contract 5.2).
    fn one_body_per_token(&self) -> bool {
        let mut bodies: BTreeMap<&str, &[u8; 32]> = BTreeMap::new();
        self.inserts
            .iter()
            .all(|(token, _, body)| *bodies.entry(token.as_str()).or_insert(body) == body)
    }

    fn phase(&self, name: &str) -> u64 {
        self.phases
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(0, |(_, micros)| *micros)
    }
}

/// Appends the frames, then runs a shipper over them until every batch has sealed at the latest at its linger and
/// landed, and stops it.
fn ship(seed: u64, frames: &[(RecordType, Vec<u8>)], setup: Setup) -> Shipped {
    run(seed, |sim| async move {
        let disk = sim.disk("disk");
        let model = ChModel::new(
            sim.rng("store").as_ref(),
            StoreFlavour::SingleNode,
            Granularity::Day,
            Duration::from_secs(60),
        );
        let retry_first = setup.retry_first;
        model.set_script(Script::new(move |_, attempt| {
            (retry_first && attempt == 1).then(|| InsertError::Retry {
                reason: RetryReason::TooManyQueries,
                message: "busy".to_owned(),
            })
        }));
        let host = sim.host("node", &disk, 0);
        let mut env = host.env.clone();
        if let Some(max_micros) = setup.deferred {
            let deferred = Deferred::new(Arc::clone(&host.env.spawner));
            for n in 0..3 {
                let rng = sim.rng(&format!("runner/{n}"));
                drop(host.env.spawn(
                    "runner",
                    deferred.runner(rng, Arc::clone(&host.env.clock), max_micros),
                ));
            }
            env.spawner = Arc::new(deferred);
        }
        let wal_config = WalConfig {
            segment_bytes: 1 << 20,
            max_bytes: 64 << 20,
            min_free_bytes: 0,
            ..WalConfig::default()
        };
        let (wal, report) = Box::pin(Wal::open(&env, wal_config)).await.unwrap();
        let mut last = Lsn::ZERO;
        for (record_type, payload) in frames {
            let frame = PreparedFrame::new(*record_type, payload.clone(), Compression::Lz4);
            last = wal.append(frame.unwrap()).await.unwrap();
        }
        // Batches seal by rows, which the frames decide; the linger only seals the last one, after every job.
        let config = ShipperConfig {
            linger: Duration::from_secs(10),
            max_linger: Duration::from_secs(10),
            seal_rows: setup.seal_rows,
            max_seal_rows: setup.seal_rows,
            tail_poll: Duration::from_millis(1),
            gc_interval: Duration::from_millis(10),
            stage_a_permits: 4,
            encode_permits: 4,
            cache_bytes: setup.cache_bytes,
            ..ShipperConfig::default()
        };
        let stores = Stores::new(Arc::new(model.clone()));
        // Ingest's share, its one permit held by a job that never runs while `ingest_busy`.
        let ingest = Arc::new(PermitPool::new(1));
        let _taken = setup
            .ingest_busy
            .then(|| ingest.offer(Box::new(|| ())))
            .map(|offer| match offer {
                Offer::Run(permit) => permit,
                Offer::Queued(_) => panic!("the permit is free"),
            });
        let hooks = Hooks {
            ingest: Some(ingest),
            ..Hooks::default()
        };
        let shipper = Shipper::new(&env, &stores, wal.clone(), &report, config, hooks).unwrap();
        let stats = shipper.stats();
        let ended: Ended = Arc::default();
        let (slot, clock) = (Arc::clone(&ended), Arc::clone(&env.clock));
        drop(env.spawn("shipper", async move {
            let started = clock.now();
            let result = Box::pin(shipper.run()).await;
            *slot.lock().unwrap() = Some((result, started, clock.now()));
        }));
        env.clock.sleep(Duration::from_secs(30)).await;
        let early = ended.lock().unwrap().take();
        assert!(early.is_none(), "the shipper stopped: {early:?}");
        let watermark = wal.watermark().map_or(0, |w| w.last_lsn().get());
        assert!(
            watermark >= last.get(),
            "watermark {watermark} below {last}"
        );
        host.spawner.request_shutdown();
        env.clock.sleep(Duration::from_millis(10)).await;
        let (result, started, stopped) = ended.lock().unwrap().take().unwrap();
        assert_eq!(result, Ok(()));
        let mut inserts: Vec<(String, u64, [u8; 32])> = model
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Insert { token, body, .. } => Some((
                    token.text().as_str().to_owned(),
                    token.last_lsn().get(),
                    body,
                )),
                Call::Probe { .. } => None,
            })
            .collect();
        inserts.sort();
        assert!(model.violations().is_empty(), "{:?}", model.violations());
        let recorder = &host.recorder;
        let jobs = |stage| {
            recorder
                .histogram_value(&registry::CPU_PERMIT_WAIT, &[stage])
                .map_or(0, |h| h.count)
        };
        let shipped = Shipped {
            inserts,
            last,
            stats: stats.snapshot(),
            phases: PHASES
                .iter()
                .map(|name| {
                    (
                        *name,
                        recorder.counter_value(&registry::SHIPPER_TASK_TIME, &[name]),
                    )
                })
                .collect(),
            ran: u64::try_from(stopped.saturating_duration_since(started).as_micros()).unwrap(),
            jobs: (jobs("stage_a"), jobs("encode")),
        };
        host.stop().await;
        shipped
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// For any log, the shipper inserts the same pairs with the same bytes whether its jobs run in place or end
    /// out of order: stage B takes the frames in LSN order with the records their stage A made, and each block is
    /// the one its rows make.
    #[test]
    fn the_pipeline_inserts_what_the_sequential_path_inserts(
        items in proptest::collection::vec(item(), 1..40),
        seal_rows in prop_oneof![Just(1usize), Just(8), Just(40), Just(400)],
        max_micros in prop_oneof![Just(50u64), Just(2_000)],
        retry_first in any::<bool>(),
        seed in 0u64..1_000,
    ) {
        let log = frames(&items);
        let in_place = ship(seed, &log, Setup { retry_first, ..Setup::in_place(seal_rows) });
        let deferred = ship(seed, &log, Setup { retry_first, ..Setup::deferred(seal_rows, max_micros) });
        prop_assert!(in_place.one_body_per_token() && deferred.one_body_per_token());
        prop_assert_eq!(in_place.tokens(), deferred.tokens());
        prop_assert_eq!(in_place.within_log(), deferred.within_log());
        // In place, the window holds one frame at a time, as the shipper read before stage A moved off its task.
        prop_assert_eq!(in_place.stats.frames_ahead_peak, 1);
        prop_assert!(deferred.stats.frames_ahead_peak <= 8, "{:?}", deferred.stats);
    }
}

/// Requests of 20 spans with notes of 1,500 bytes, large enough for their stage A to run as jobs.
fn large_requests(count: u8) -> Vec<(RecordType, Vec<u8>)> {
    let items: Vec<Item> = (0..count)
        .map(|r| {
            Item::Request(
                (0..20)
                    .map(|s| SpanGen {
                        trace: r % 6,
                        span: s,
                        parent: s.checked_sub(1),
                        days_back: s % 2,
                        note: 1_500,
                        tokens: (s % 5 == 0).then_some(10),
                    })
                    .collect(),
            )
        })
        .collect();
    frames(&items)
}

/// With jobs that take time, the shipper reads ahead while stage A runs, waits for it only when the window is
/// full, records each job's wait and hold, and still charges every moment of its run to one phase.
#[test]
fn stage_a_runs_ahead_of_stage_b_and_the_phases_add_up() {
    let log = large_requests(24);
    let in_place = ship(3, &log, Setup::in_place(40));
    let deferred = ship(3, &log, Setup::deferred(40, 2_000));
    assert_eq!(in_place.tokens(), deferred.tokens());
    assert_eq!(in_place.within_log(), deferred.within_log());
    assert_eq!(in_place.stats.frames_ahead_peak, 1);
    // Two frames per stage A permit.
    assert_eq!(deferred.stats.frames_ahead_peak, 8, "{:?}", deferred.stats);
    assert!(deferred.phase("wait_stage_a") > 0, "{:?}", deferred.phases);
    assert_eq!(in_place.phase("wait_stage_a"), 0, "{:?}", in_place.phases);
    let charged: u64 = deferred.phases.iter().map(|(_, micros)| micros).sum();
    assert_eq!(charged, deferred.ran, "{:?}", deferred.phases);
    // Every request ran stage A as a job, and every block was encoded by one.
    let (stage_a, encode) = deferred.jobs;
    assert_eq!(stage_a, 24);
    assert_eq!(encode, deferred.inserts.len() as u64);
}

/// A budget of no bytes still reads one frame into the empty window, so the shipper progresses one frame at a
/// time and lands everything.
#[test]
fn a_cache_budget_too_small_for_one_frame_reads_one_at_a_time() {
    let log = large_requests(12);
    let in_place = ship(5, &log, Setup::in_place(40));
    let deferred = ship(
        5,
        &log,
        Setup {
            cache_bytes: 0,
            ..Setup::deferred(40, 2_000)
        },
    );
    assert_eq!(in_place.within_log(), deferred.within_log());
    assert_eq!(deferred.stats.frames_ahead_peak, 1);
}

/// While ingest takes every permit of its share, its requests wait for the CPUs: the shipper normalizes and encodes
/// on its own task, reading one frame at a time, and inserts what it inserts otherwise.
#[test]
fn while_ingest_takes_every_permit_the_shipper_works_on_its_task() {
    let log = large_requests(12);
    let in_place = ship(7, &log, Setup::in_place(40));
    let busy = ship(
        7,
        &log,
        Setup {
            ingest_busy: true,
            ..Setup::deferred(40, 2_000)
        },
    );
    assert_eq!(in_place.within_log(), busy.within_log());
    assert_eq!(busy.stats.frames_ahead_peak, 1);
    assert_eq!(busy.jobs, (0, 0));
    assert_eq!(busy.phase("wait_stage_a"), 0, "{:?}", busy.phases);
}
