//! Alerting end to end under faults (contracts 11.7 and 12.6): the real scheduler, the real evaluations
//! consumer and the real outbox consumer on several nodes, over `PgModel` and over rollup rows in `ChModel`.
//!
//! The telemetry is a seeded scenario: per minute and environment either calm traffic, traffic with errors,
//! or no rows at all. It is written to the store model before the nodes start, with the blocks the shipper
//! builds, so what an evaluation reads is a function of its window alone and the oracle can recompute it.
//! Four rules watch it, with and without a `for` duration, under each no-data policy, with and without
//! dimensions, notifying a webhook channel and an email channel.
//!
//! The faulty phase:
//!
//! - **crashes**: every task of a node is aborted at its await point; the node starts again at once, after
//!   seconds, or after minutes, so that at times no node runs;
//! - **time skips**: the wall clock of the database steps forward by up to four minutes, as after a pause of
//!   its host, so ticks pass that nobody evaluated;
//! - **store faults**: Postgres fails over, loses commits with an unknown outcome, aborts transactions and
//!   delays commits; ClickHouse is unreachable for up to 90 s, and now and then for up to ten minutes, long
//!   enough for a rule to report that its evaluations keep failing;
//! - **receivers**: the webhook receiver and the mail server are down for minutes at a time and refuse a
//!   share of what reaches them; in half of the runs the webhook receiver is down for most of the phase,
//!   long enough for its channel to be turned off after 50 failures.
//!
//! Then everything heals, an administrator turns the channels that were turned off on again, and a
//! fault-free tail follows in which the scenario is steady. The oracle:
//!
//! - **A1, the outbox is exactly-once per transition**: the outbox holds one row for each history row that
//!   says it notified and each channel of its rule, and no other row. A history row and its outbox rows
//!   commit together or not at all.
//! - **A2, the state machine**: the value of every history row is the value of its window in the scenario; a
//!   series fires only on a breach; a rule with a `for` duration fires only from `pending`, after that
//!   duration; at the end of the tail every series is in the state its steady scenario and its no-data
//!   policy give.
//! - **A3, a firing never goes unnotified once a channel heals**: at the end of the tail every outbox row is
//!   done, every done row was delivered unless its channel was turned off, and every series that fires has a
//!   delivered firing notice of its current episode on every channel of its rule, and a recent one when it
//!   has fired for longer than `renotify`. A notification that was dropped while its channel was off is
//!   made good by the next repeat, at most `renotify` after the channel is on again.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_alerts::schedule::window_of;
use crucible_alerts::{
    Alerts, Comparison, Evaluator, NoDataPolicy, QuerySource, RuleInput, RuleSpec, SeriesSource,
    series_values,
};
use crucible_auth::Sealer;
use crucible_chfmt::Compression;
use crucible_control::{
    AlertEvent, AlertEventKind, AlertRule, AlertStatus, Channel, ChannelKind, ControlStore,
    JsonDoc, Project, Queue, Repos as _, TxError,
};
use crucible_egress::{Egress, EgressPolicy};
use crucible_env::{
    Clock, Env, InsertSettings, Instant, MailError, MailMessage, Mailer, Rng, StoreFlavour, Stores,
    TelemetryStore, TlsRoots,
};
use crucible_jobs::Registry;
use crucible_notify::{
    ChannelPatch, Channels, Delivery, NetTransport, NoticeKind, Notification, Notifier, Target,
    Transport, outbox_job,
};
use crucible_plan::{Aggregation, DimensionId, MeasureId, Source};
use crucible_query::QueryService;
use crucible_recent::{RecentConfig, RecentIndex, SpanRead, WalLocator, WalReadError, WalView};
use crucible_records::{
    ChannelId, Histogram, Level, Lsn, NanoUsd, ObservationType, ProjectId, SpanMetricsRow, TableId,
    TimeWindow, Timestamp, Uuid, WalEpoch,
};
use crucible_schema::{Granularity, route};
use crucible_shipper::tables::span_metrics_row;
use crucible_shipper::value::Row;
use crucible_shipper::{BatchId, Pair, pair_block, pair_token};

pub use crucible_alerts::machine::Mutant;

use crate::ch_model::ChModel;
use crate::pg_model::{JobStatus, PgFaults, PgModel};
use crate::runtime::{Host, Sim, run};

const MINUTE_MICROS: i64 = 60_000_000;
/// The environments of the scenario, each one series of a rule with the `environment` dimension.
const ENVS: [&str; 3] = ["prod", "staging", "dev"];
/// The most the database clock is stepped forward in one run.
const MAX_SKIP: Duration = Duration::from_secs(900);
/// `renotify` of every rule: the minimum the contract allows, so that a run sees many repeats.
const RENOTIFY_SECS: u32 = 300;

/// What one run does.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Nodes that run the jobs.
    pub nodes: usize,
    /// How long the faulty phase lasts, in virtual time.
    pub faulty_for: Duration,
    /// How long the fault-free tail lasts. It must cover the largest clock skip, the longest retry backoff
    /// of the outbox (1 h) and a few repeats.
    pub tail: Duration,
    /// The seeded defect, if any.
    pub mutant: Option<Mutant>,
}

impl Options {
    /// Every fault.
    pub const FAULTY: Self = Self {
        nodes: 2,
        faulty_for: Duration::from_secs(45 * 60),
        tail: Duration::from_secs(95 * 60),
        mutant: None,
    };
}

/// What happened in a run, so that a test can require that the interesting cases occurred.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Times a node started.
    pub boots: u64,
    /// Crashes.
    pub crashes: u64,
    /// Crashes after which the node stayed down for minutes.
    pub long_downtimes: u64,
    /// Forward steps of the database clock.
    pub time_skips: u64,
    /// Postgres failovers.
    pub failovers: u64,
    /// ClickHouse outages.
    pub store_outages: u64,
    /// Reads of evaluations that were answered.
    pub reads: u64,
    /// Reads of evaluations that failed.
    pub read_failures: u64,
    /// History rows at the end.
    pub events: u64,
    /// History rows of repeated notifications.
    pub renotifies: u64,
    /// History rows that say the rule's evaluations keep failing.
    pub evaluation_failing: u64,
    /// Ticks the rules counted as skipped.
    pub skipped_ticks: u64,
    /// Outbox rows at the end.
    pub outbox_rows: u64,
    /// Delivery attempts that reached a receiver and were accepted.
    pub deliveries: u64,
    /// Delivery attempts that failed.
    pub delivery_failures: u64,
    /// Outbox rows delivered more than once: a completion that was lost.
    pub redeliveries: u64,
    /// Messages the mail server accepted.
    pub mails: u64,
    /// Channels the outbox consumer turned off after 50 failures in a row.
    pub channels_disabled: u64,
    /// Outbox rows that were done without a delivery, because their channel was off.
    pub dropped: u64,
    /// Series that fire at the end.
    pub firing_at_end: u64,
    /// Series in `no_data` at the end.
    pub no_data_at_end: u64,
}

impl Stats {
    /// Adds another run's counts.
    pub fn add(&mut self, other: &Self) {
        self.boots += other.boots;
        self.crashes += other.crashes;
        self.long_downtimes += other.long_downtimes;
        self.time_skips += other.time_skips;
        self.failovers += other.failovers;
        self.store_outages += other.store_outages;
        self.reads += other.reads;
        self.read_failures += other.read_failures;
        self.events += other.events;
        self.renotifies += other.renotifies;
        self.evaluation_failing += other.evaluation_failing;
        self.skipped_ticks += other.skipped_ticks;
        self.outbox_rows += other.outbox_rows;
        self.deliveries += other.deliveries;
        self.delivery_failures += other.delivery_failures;
        self.redeliveries += other.redeliveries;
        self.mails += other.mails;
        self.channels_disabled += other.channels_disabled;
        self.dropped += other.dropped;
        self.firing_at_end += other.firing_at_end;
        self.no_data_at_end += other.no_data_at_end;
    }
}

/// The result of one run.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The run seed.
    pub seed: u64,
    /// What happened.
    pub stats: Stats,
    /// Every invariant the run broke; empty when alerting held.
    pub violations: Vec<String>,
}

/// A notification that a receiver accepted.
#[derive(Clone, Debug)]
struct Delivered {
    row: Uuid,
    notification: Notification,
}

/// What the harness saw.
#[derive(Default)]
struct Ledger {
    stats: Stats,
    violations: Vec<String>,
    delivered: Vec<Delivered>,
    /// The channels that were found turned off.
    disabled: BTreeSet<ChannelId>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn below(rng: &dyn Rng, bound: u64) -> u64 {
    rng.next_u64() % bound.max(1)
}

fn millis(rng: &dyn Rng, from: u64, to: u64) -> Duration {
    Duration::from_millis(from + below(rng, to - from + 1))
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn id(n: u64) -> Uuid {
    Uuid::from_v7_parts(1_800_000_000_000 + n, u128::from(n))
}

/// What one environment does in one minute.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Traffic with at most one error.
    Calm,
    /// Traffic with six to nine errors.
    Breach,
    /// No rows.
    Silent,
}

/// The telemetry of a run: the span count and the error count of each minute and environment that has rows.
#[derive(Debug)]
struct Scenario {
    cells: BTreeMap<(i64, usize), (u64, u64)>,
}

impl Scenario {
    /// Minutes `first..last` of wall time. Until `steady_from` each environment moves through phases of 3 to
    /// 10 minutes; from then on `prod` breaches, `staging` is silent and `dev` is calm.
    fn new(rng: &dyn Rng, first: i64, steady_from: i64, last: i64) -> Self {
        let mut cells = BTreeMap::new();
        for (env, steady) in [Mode::Breach, Mode::Silent, Mode::Calm]
            .into_iter()
            .enumerate()
        {
            let mut minute = first;
            while minute < last {
                let (mode, until) = if minute >= steady_from {
                    (steady, last)
                } else {
                    let mode = match below(rng, 5) {
                        0 | 1 => Mode::Breach,
                        2 | 3 => Mode::Calm,
                        _ => Mode::Silent,
                    };
                    let length = 3 + i64::try_from(below(rng, 8)).unwrap_or(0);
                    (mode, (minute + length).min(steady_from))
                };
                for m in minute..until {
                    let count = 20 + below(rng, 21);
                    match mode {
                        Mode::Calm => cells.insert((m, env), (count, below(rng, 2))),
                        Mode::Breach => cells.insert((m, env), (count, 6 + below(rng, 4))),
                        Mode::Silent => None,
                    };
                }
                minute = until;
            }
        }
        Self { cells }
    }

    /// What `spec` reads for the series with these dimension values over `window`: `None` without rows.
    fn value(&self, spec: &RuleSpec, series: &[String], window: TimeWindow) -> Option<i64> {
        let envs: Vec<usize> = match series.first() {
            Some(name) => vec![ENVS.iter().position(|env| env == name)?],
            None => (0..ENVS.len()).collect(),
        };
        let minutes = window.from().micros().div_euclid(MINUTE_MICROS)
            ..window.to().micros().div_euclid(MINUTE_MICROS);
        let mut total = None;
        for minute in minutes {
            for env in &envs {
                if let Some((count, errors)) = self.cells.get(&(minute, *env)) {
                    let value = if spec.measure == MeasureId::Count {
                        *count
                    } else {
                        *errors
                    };
                    *total.get_or_insert(0) += i64::try_from(value).unwrap_or(i64::MAX);
                }
            }
        }
        total
    }

    /// Writes the scenario to the store as the shipper writes `span_metrics`: one block per partition.
    async fn write(&self, store: &ChModel, project: ProjectId) -> Result<(), String> {
        let mut pairs: BTreeMap<Pair, Vec<Row>> = BTreeMap::new();
        for (n, ((minute, env), (count, errors))) in self.cells.iter().enumerate() {
            let at = Timestamp::from_micros(minute * MINUTE_MICROS);
            let row = SpanMetricsRow {
                project_id: project,
                minute: at,
                environment: ENVS[*env].into(),
                observation_type: ObservationType::Span,
                model_response: "".into(),
                level: Level::Default,
                release: "".into(),
                count: *count,
                error_count: *errors,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                total_cost_nusd: NanoUsd::ZERO,
                dur_hist: Histogram::new(),
                ttft_hist: Histogram::new(),
            };
            let partition = route(TableId::SpanMetrics, Granularity::Day, at)
                .map_err(|_| "a scenario minute is outside the routable range".to_owned())?;
            pairs
                .entry((TableId::SpanMetrics, partition))
                .or_default()
                .push(Row {
                    cells: span_metrics_row(&row),
                    lsn: Lsn::new(1),
                    ordinal: u32::try_from(n).unwrap_or(u32::MAX),
                });
        }
        let epoch = WalEpoch::from_uuid(id(7));
        for (n, (pair, rows)) in pairs.into_iter().enumerate() {
            let lsn = Lsn::new(1 + n as u64);
            let batch = BatchId {
                epoch,
                first_lsn: lsn,
                last_lsn: lsn,
            };
            let marker_at = Timestamp::from_micros(crate::SIM_EPOCH_MICROS);
            let block = pair_block(&batch, pair, rows, marker_at, Compression::Lz4)
                .map_err(|e| format!("building a rollup block: {e}"))?;
            let token = pair_token(&batch, pair).map_err(|e| format!("a batch token: {e}"))?;
            store
                .insert_block(block, InsertSettings::new(token, StoreFlavour::SingleNode))
                .await
                .map_err(|e| format!("inserting a rollup block: {e:?}"))?;
        }
        Ok(())
    }
}

/// A WAL that holds nothing: the rules read the rollup only.
#[derive(Debug)]
struct EmptyWal;

impl WalView for EmptyWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::new(0)
    }

    fn read_span(&self, _: ProjectId, _: WalLocator) -> SpanRead {
        Box::pin(async { Err(WalReadError::Gone) })
    }
}

/// The read of the evaluations: the production source, counted.
#[derive(Debug)]
struct CountedSource {
    inner: QuerySource,
    ledger: Arc<Mutex<Ledger>>,
}

impl fmt::Debug for Ledger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ledger")
    }
}

#[async_trait]
impl SeriesSource for CountedSource {
    async fn read(
        &self,
        project: &Project,
        spec: &RuleSpec,
        window: TimeWindow,
    ) -> Result<crucible_alerts::Observed, crucible_alerts::ReadFailure> {
        let result = self.inner.read(project, spec, window).await;
        let mut ledger = lock(&self.ledger);
        match &result {
            Ok(_) => ledger.stats.reads += 1,
            Err(_) => ledger.stats.read_failures += 1,
        }
        result
    }
}

/// A receiver the harness can take down.
#[derive(Debug)]
struct Receiver {
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
    down_until: AtomicU64,
    /// The share of attempts that are refused while the receiver is up.
    refuse_ppm: AtomicU64,
}

impl Receiver {
    fn new(clock: &Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Self {
        Self {
            clock: Arc::clone(clock),
            rng,
            down_until: AtomicU64::new(0),
            refuse_ppm: AtomicU64::new(0),
        }
    }

    fn down_for(&self, duration: Duration) {
        let until = self.clock.now().saturating_add(duration).nanos();
        self.down_until.fetch_max(until, Ordering::SeqCst);
    }

    fn heal(&self) {
        self.down_until.store(0, Ordering::SeqCst);
        self.refuse_ppm.store(0, Ordering::SeqCst);
    }

    fn is_down(&self) -> bool {
        self.clock.now().nanos() < self.down_until.load(Ordering::SeqCst)
    }

    /// Whether this attempt is refused although the receiver is up.
    fn refuses(&self) -> bool {
        below(self.rng.as_ref(), 1_000_000) < self.refuse_ppm.load(Ordering::SeqCst)
    }
}

/// The mail server (contract 2.11): an outbox, with seeded transient and permanent failures.
#[derive(Debug)]
struct SimMailer {
    server: Arc<Receiver>,
    accepted: Mutex<Vec<MailMessage>>,
}

#[async_trait]
impl Mailer for SimMailer {
    async fn send(&self, message: MailMessage) -> Result<(), MailError> {
        // A round trip to the server.
        self.server.clock.sleep(Duration::from_millis(20)).await;
        if self.server.is_down() {
            return Err(MailError {
                transient: true,
                reason: "the mail server does not answer".to_owned(),
            });
        }
        if self.server.refuses() {
            return Err(MailError {
                transient: below(self.server.rng.as_ref(), 2) == 0,
                reason: "the mail server refused the message".to_owned(),
            });
        }
        lock(&self.accepted).push(message);
        Ok(())
    }
}

/// The transport of the nodes. Email goes through the production transport, which renders the message,
/// to the simulated mail server; a webhook goes to the scripted receiver.
#[derive(Debug)]
struct SimTransport {
    mail: NetTransport,
    hook: Arc<Receiver>,
    ledger: Arc<Mutex<Ledger>>,
}

#[async_trait]
impl Transport for SimTransport {
    async fn deliver(
        &self,
        channel: &Channel,
        secret: Option<&[u8]>,
        notification: &Notification,
        delivery_id: Uuid,
    ) -> Delivery {
        let delivery = if channel.kind == ChannelKind::Email {
            self.mail
                .deliver(channel, secret, notification, delivery_id)
                .await
        } else {
            self.hook.clock.sleep(Duration::from_millis(30)).await;
            let failed = |status_class, error_code| Delivery {
                ok: false,
                status_class,
                error_code: Some(error_code),
                latency_ms: 30,
            };
            if secret.is_none() {
                failed(None, "secret_missing")
            } else if self.hook.is_down() {
                failed(None, "connect_failed")
            } else if self.hook.refuses() {
                failed(Some("5xx"), "receiver_failed")
            } else {
                Delivery {
                    ok: true,
                    status_class: Some("2xx"),
                    error_code: None,
                    latency_ms: 30,
                }
            }
        };
        let mut ledger = lock(&self.ledger);
        if delivery.ok {
            ledger.stats.deliveries += 1;
            ledger.delivered.push(Delivered {
                row: delivery_id,
                notification: notification.clone(),
            });
        } else {
            ledger.stats.delivery_failures += 1;
        }
        delivery
    }

    fn has_mailer(&self) -> bool {
        true
    }
}

/// What every node and the harness share.
#[derive(Clone)]
struct World {
    model: PgModel,
    store: Arc<ChModel>,
    /// The database host: its wall clock is the database clock.
    database: Env,
    ledger: Arc<Mutex<Ledger>>,
    source: Arc<dyn SeriesSource>,
    transport: Arc<dyn Transport>,
    sealer: Arc<Sealer>,
    hook: Arc<Receiver>,
    mail: Arc<Receiver>,
    options: Options,
}

impl World {
    fn control(&self) -> Arc<dyn ControlStore> {
        Arc::new(self.model.clone())
    }

    /// The jobs of one node, as the binary registers them.
    fn registry(&self) -> Registry {
        let evaluator = Evaluator::new(Arc::clone(&self.source)).with_mutant(self.options.mutant);
        let mut registry = Registry::new();
        for job in crucible_alerts::jobs(evaluator) {
            registry.register(job);
        }
        registry.register(outbox_job(Notifier::new(
            Arc::clone(&self.transport),
            Arc::clone(&self.sealer),
        )));
        registry
    }

    fn channels(&self) -> Channels {
        Channels::new(
            &self.database,
            self.control(),
            Arc::clone(&self.sealer),
            Arc::clone(&self.transport),
            EgressPolicy::default(),
        )
    }
}

/// The rules of the run and the channels they notify.
struct Fixture {
    project: ProjectId,
    rules: Vec<AlertRule>,
}

fn spec(
    measure: MeasureId,
    dimensions: Vec<DimensionId>,
    comparison: Comparison,
    threshold: i64,
) -> RuleSpec {
    RuleSpec {
        source: Source::Spans,
        mcp: None,
        measure,
        aggregation: Aggregation::Sum,
        dimensions,
        comparison,
        threshold,
        window_secs: 60,
        interval_secs: 60,
        for_secs: 0,
        no_data: NoDataPolicy::Keep,
        no_data_for_secs: 180,
        renotify_secs: RENOTIFY_SECS,
        channels: Vec::new(),
    }
}

/// The project, a webhook channel and an email channel, and the four rules.
async fn seed(world: &World) -> Result<Fixture, String> {
    let project = Project {
        id: ProjectId::from_uuid(id(9_001)),
        name: "Web".to_owned(),
        slug: "web".to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    };
    let fail = |what: &str, error: &dyn fmt::Display| format!("seeding {what}: {error}");
    world
        .model
        .projects()
        .insert(&project)
        .await
        .map_err(|e| fail("the project", &e))?;
    let channels = world.channels();
    let hook = channels
        .create(
            project.id,
            "On call",
            Target::Webhook {
                url: "http://10.0.0.8:9000/crucible".to_owned(),
            },
        )
        .await
        .map_err(|e| fail("the webhook channel", &e))?
        .view
        .channel
        .id;
    let mail = channels
        .create(
            project.id,
            "Team",
            Target::Email {
                to: vec!["team@example.com".to_owned()],
            },
        )
        .await
        .map_err(|e| fail("the email channel", &e))?
        .view
        .channel
        .id;
    let environment = || vec![DimensionId::Environment];
    let inputs = [
        (
            "fast",
            RuleSpec {
                channels: vec![hook, mail],
                ..spec(MeasureId::ErrorCount, environment(), Comparison::Gt, 5)
            },
        ),
        (
            "patient",
            RuleSpec {
                window_secs: 120,
                for_secs: 180,
                no_data: NoDataPolicy::NoData,
                channels: vec![hook],
                ..spec(MeasureId::ErrorCount, environment(), Comparison::Gt, 8)
            },
        ),
        (
            "quiet",
            RuleSpec {
                interval_secs: 120,
                no_data: NoDataPolicy::Ok,
                no_data_for_secs: 240,
                channels: vec![mail],
                ..spec(MeasureId::ErrorCount, environment(), Comparison::Gt, 5)
            },
        ),
        (
            "volume",
            RuleSpec {
                for_secs: 60,
                channels: vec![hook],
                ..spec(MeasureId::Count, Vec::new(), Comparison::Lt, 30)
            },
        ),
    ];
    let alerts = Alerts::new(&world.database, world.control(), None);
    let mut rules = Vec::new();
    for (name, spec) in inputs {
        let view = alerts
            .create(
                project.id,
                RuleInput {
                    name: name.to_owned(),
                    enabled: true,
                    spec,
                },
            )
            .await
            .map_err(|e| fail("a rule", &e))?;
        rules.push(view.rule);
    }
    Ok(Fixture {
        project: project.id,
        rules,
    })
}

fn faults(on: bool) -> PgFaults {
    if on {
        PgFaults {
            unavailable_ppm: 10_000,
            commit_unknown_ppm: 20_000,
            spurious_serialization_ppm: 10_000,
            slow_commit_ppm: 20_000,
            slow_commit_micros: (1_000, 1_000_000),
            failover_ppm: 300,
            failover_micros: (200_000, 6_000_000),
            latency_micros: (100, 40_000),
            ..PgFaults::default()
        }
    } else {
        PgFaults::default()
    }
}

/// One node until `end`: it runs the registry, crashes at random moments of the faulty phase and starts again.
async fn node(sim: &Sim, world: World, n: usize, faulty_until: u64, end: u64) {
    let rng = sim.rng(&format!("alerts-sim/node-{n}"));
    // Up to 5 min either way: nothing in alerting may compare a node's wall clock with a tick.
    let skew = i64::try_from(below(rng.as_ref(), 600_000_001)).unwrap_or(0) - 300_000_000;
    let disk = sim.disk(&format!("alerts-node-{n}"));
    let clock = Arc::clone(&world.database.clock);
    let mut boot = 0;
    loop {
        boot += 1;
        let name = format!("node-{n}#{boot}");
        let host: Host = sim.host(&name, &disk, skew);
        let _running = world
            .registry()
            .start(&host.env, Some(world.control()), &name);
        lock(&world.ledger).stats.boots += 1;
        let pause = millis(rng.as_ref(), 20_000, 150_000);
        if clock.now().saturating_add(pause).nanos() >= faulty_until {
            clock.sleep_until(Instant::from_nanos(end)).await;
            host.stop().await;
            return;
        }
        clock.sleep(pause).await;
        host.crash().await;
        let down = match below(rng.as_ref(), 4) {
            0 => {
                lock(&world.ledger).stats.long_downtimes += 1;
                millis(rng.as_ref(), 120_000, 420_000)
            }
            1 => millis(rng.as_ref(), 0, 20_000),
            _ => Duration::ZERO,
        };
        lock(&world.ledger).stats.crashes += 1;
        let wake = clock.now().saturating_add(down).nanos().min(faulty_until);
        clock.sleep_until(Instant::from_nanos(wake)).await;
    }
}

/// The faults of the faulty phase, at random moments until `until`.
async fn chaos(world: World, database: Arc<crate::SimClock>, rng: Arc<dyn Rng>, until: u64) {
    let clock = &world.database.clock;
    let mut skipped = Duration::ZERO;
    // In half of the runs the webhook receiver is down from the fifth minute to the end of the phase: its
    // channel collects 50 failures in a row and is turned off.
    let long_outage = below(rng.as_ref(), 2) == 0;
    if long_outage {
        clock.sleep(Duration::from_secs(300)).await;
        let left = until.saturating_sub(clock.now().nanos());
        world.hook.down_for(Duration::from_nanos(left));
    }
    loop {
        clock.sleep(millis(rng.as_ref(), 15_000, 60_000)).await;
        if clock.now().nanos() >= until {
            return;
        }
        match below(rng.as_ref(), 6) {
            0 => {
                world.model.fail_over(millis(rng.as_ref(), 200, 8_000));
                lock(&world.ledger).stats.failovers += 1;
            }
            1 => {
                // One outage in four outlasts five ticks of a rule, which then says so once.
                let outage = if below(rng.as_ref(), 4) == 0 {
                    millis(rng.as_ref(), 420_000, 600_000)
                } else {
                    millis(rng.as_ref(), 5_000, 90_000)
                };
                world.store.start_outage(outage);
                lock(&world.ledger).stats.store_outages += 1;
            }
            2 => world.hook.down_for(millis(rng.as_ref(), 30_000, 600_000)),
            3 => world.mail.down_for(millis(rng.as_ref(), 30_000, 600_000)),
            4 => {
                let step = millis(rng.as_ref(), 30_000, 240_000);
                if skipped + step <= MAX_SKIP {
                    skipped += step;
                    database.step_wall(i64::try_from(step.as_micros()).unwrap_or(0));
                    lock(&world.ledger).stats.time_skips += 1;
                }
            }
            _ => {}
        }
    }
}

/// The administrator after the receivers are fixed: every channel that the server turned off is turned on
/// again. Looked at three times, because a delivery that failed just before the fix may still be counted.
async fn turn_channels_on(world: &World, project: ProjectId) {
    let channels = world.channels();
    for wait in [0, 30, 120] {
        world.database.clock.sleep(Duration::from_secs(wait)).await;
        let Ok(views) = channels.list(project).await else {
            continue;
        };
        for view in views {
            let channel = view.channel;
            if channel.enabled {
                continue;
            }
            {
                let mut ledger = lock(&world.ledger);
                if ledger.disabled.insert(channel.id) {
                    ledger.stats.channels_disabled += 1;
                }
                if channel.disabled_reason.is_none() {
                    ledger.violations.push(format!(
                        "the channel {} is off without a reason",
                        channel.name
                    ));
                }
            }
            let patch = ChannelPatch {
                enabled: Some(true),
                ..ChannelPatch::default()
            };
            if let Err(error) = channels
                .update(project, channel.id, channel.version, patch)
                .await
            {
                lock(&world.ledger)
                    .violations
                    .push(format!("turning the channel {} on: {error}", channel.name));
            }
        }
    }
}

/// The identity of a notification: the rule, the tick, what it announces, the channel and the series.
type NoticeKey = (Uuid, u64, &'static str, Uuid, Vec<String>);

fn notice_key(n: &Notification) -> NoticeKey {
    (
        n.rule_id,
        n.eval_seq,
        n.kind.as_str(),
        n.channel_id,
        n.series.iter().map(|label| label.value.clone()).collect(),
    )
}

/// What a history row announces to the channels, when it notifies.
fn announced(event: &AlertEvent) -> Option<NoticeKind> {
    match (event.kind, event.to_status) {
        (AlertEventKind::EvaluationFailing, _) => Some(NoticeKind::EvaluationFailing),
        (AlertEventKind::Renotify, _) => Some(NoticeKind::Renotify),
        (AlertEventKind::Transition, AlertStatus::Firing) => Some(NoticeKind::Firing),
        (AlertEventKind::Transition, AlertStatus::Resolved) => Some(NoticeKind::Resolved),
        (AlertEventKind::Transition, AlertStatus::NoData) => Some(NoticeKind::NoData),
        (AlertEventKind::Transition, _) => None,
    }
}

/// A2 over the history of one rule: values, breaches and the `for` duration.
fn audit_history(
    scenario: &Scenario,
    rule: &AlertRule,
    spec: &RuleSpec,
    events: &[AlertEvent],
    violations: &mut Vec<String>,
) {
    // Oldest first, per series.
    let mut by_series: BTreeMap<&str, Vec<&AlertEvent>> = BTreeMap::new();
    for event in events.iter().rev() {
        by_series.entry(&event.series_key).or_default().push(event);
    }
    for (series_key, events) in by_series {
        let series = series_values(series_key);
        let mut pending_since = None;
        for event in events {
            if event.kind == AlertEventKind::EvaluationFailing {
                continue;
            }
            let label = format!(
                "A2: rule {} series {series_key} tick {}",
                rule.name, event.eval_seq
            );
            let Ok(window) = window_of(
                rule.id,
                rule.interval_secs,
                event.eval_seq,
                spec.window_secs,
            ) else {
                violations.push(format!("{label}: the tick has no window"));
                continue;
            };
            let truth = scenario.value(spec, &series, window);
            if event.value.is_some() && event.value != truth {
                violations.push(format!(
                    "{label}: the history says {:?} and the window {window:?} holds {truth:?}",
                    event.value
                ));
            }
            if event.kind != AlertEventKind::Transition {
                continue;
            }
            match event.to_status {
                AlertStatus::Pending => pending_since = Some(event.at),
                AlertStatus::Firing => {
                    if !truth.is_some_and(|v| spec.comparison.holds(v, spec.threshold)) {
                        violations.push(format!(
                            "{label}: the series fired on {truth:?}, which does not breach"
                        ));
                    }
                    if spec.for_secs > 0 && event.from_status != AlertStatus::Resolved {
                        let held = pending_since
                            .filter(|_| event.from_status == AlertStatus::Pending)
                            .map(|since| event.at.micros() - since.micros());
                        if !held.is_some_and(|held| held >= i64::from(spec.for_secs) * 1_000_000) {
                            violations.push(format!(
                                "{label}: the series fired from {} after {held:?} µs of pending, and \
                                 the rule's `for` is {} s",
                                event.from_status, spec.for_secs
                            ));
                        }
                    }
                    pending_since = None;
                }
                _ => pending_since = None,
            }
        }
    }
}

/// Everything the oracle checks at the end of the tail.
async fn audit(
    world: &World,
    scenario: &Scenario,
    fixture: &Fixture,
    ledger: &mut Ledger,
) -> Result<(), TxError> {
    let model = &world.model;
    let now = model.db_now().await?;
    // The outbox: every row, by what it notifies.
    let mut outbox: BTreeMap<NoticeKey, (Uuid, JobStatus)> = BTreeMap::new();
    for row in model.job_states() {
        if row.queue != Queue::NotificationOutbox {
            continue;
        }
        let Some(notification) = Notification::from_payload(&row.payload) else {
            ledger
                .violations
                .push(format!("A1: outbox row {} holds no notification", row.id));
            continue;
        };
        ledger.stats.outbox_rows += 1;
        if outbox
            .insert(notice_key(&notification), (row.id, row.status))
            .is_some()
        {
            ledger.violations.push(format!(
                "A1: the outbox holds two rows for {:?}",
                notice_key(&notification)
            ));
        }
        // A row written in the last two minutes may still be on its way.
        let fresh = now.micros() - notification.at_micros < 2 * MINUTE_MICROS;
        if row.status != JobStatus::Done && !fresh {
            ledger.violations.push(format!(
                "A3: outbox row {} for {:?} is {:?} after {} attempts at the end of the tail",
                row.id,
                notice_key(&notification),
                row.status,
                row.attempts
            ));
        }
    }
    let delivered_rows: BTreeMap<Uuid, usize> =
        ledger
            .delivered
            .iter()
            .fold(BTreeMap::new(), |mut counts, delivered| {
                *counts.entry(delivered.row).or_default() += 1;
                counts
            });
    ledger.stats.redeliveries += delivered_rows.values().filter(|n| **n > 1).count() as u64;
    let outbox_ids: BTreeSet<Uuid> = outbox.values().map(|(row, _)| *row).collect();
    for delivered in &ledger.delivered {
        if !outbox_ids.contains(&delivered.row) {
            ledger.violations.push(format!(
                "A1: a notification was delivered that is in no outbox row: {:?}",
                notice_key(&delivered.notification)
            ));
        }
    }
    for (key, (row, status)) in &outbox {
        if *status == JobStatus::Done && !delivered_rows.contains_key(row) {
            ledger.stats.dropped += 1;
            if !ledger.disabled.contains(&ChannelId::from_uuid(key.3)) {
                ledger.violations.push(format!(
                    "A3: outbox row {row} for {key:?} is done, was never delivered, and its channel was \
                     never turned off"
                ));
            }
        }
    }

    let mut expected: BTreeSet<NoticeKey> = BTreeSet::new();
    let tail_micros = i64::try_from(world.options.tail.as_micros()).unwrap_or(i64::MAX);
    for rule in &fixture.rules {
        let spec = RuleSpec::from_doc(&rule.spec)?;
        let events = model.alert_events().list(rule.id, None, 1_000_000).await?;
        ledger.stats.events += events.len() as u64;
        for event in &events {
            match event.kind {
                AlertEventKind::Renotify => ledger.stats.renotifies += 1,
                AlertEventKind::EvaluationFailing => ledger.stats.evaluation_failing += 1,
                AlertEventKind::Transition => {}
            }
            // A1, one way: every history row that notified has its outbox rows.
            let Some(kind) = announced(event).filter(|_| event.notified) else {
                if event.notified {
                    ledger.violations.push(format!(
                        "A1: rule {} has a history row that notified without a notice: {event:?}",
                        rule.name
                    ));
                }
                continue;
            };
            for channel in &spec.channels {
                expected.insert((
                    rule.id.uuid(),
                    event.eval_seq,
                    kind.as_str(),
                    channel.uuid(),
                    series_values(&event.series_key),
                ));
            }
        }
        audit_history(scenario, rule, &spec, &events, &mut ledger.violations);

        // A2 at the end, and A3 for what fires.
        let stored = model
            .alert_rules()
            .get(rule.id)
            .await?
            .unwrap_or_else(|| rule.clone());
        ledger.stats.skipped_ticks += stored.skipped_ticks;
        if stored.last_eval_seq.is_none() {
            ledger
                .violations
                .push(format!("A2: rule {} was never evaluated", rule.name));
        }
        let states = model.alert_state().list(rule.id).await?;
        let status_of = |env: &str| {
            states
                .iter()
                .find(|state| {
                    series_values(&state.series_key).first().map(String::as_str) == Some(env)
                })
                .map(|state| state.status)
        };
        let mut want = |what: &str, got: Option<AlertStatus>, allowed: &[Option<AlertStatus>]| {
            if !allowed.contains(&got) {
                ledger.violations.push(format!(
                    "A2: at the end of the tail rule {} has {what} in {got:?}, and its scenario and \
                     no-data policy give one of {allowed:?}",
                    rule.name
                ));
            }
        };
        let quiet = [None, Some(AlertStatus::Ok)];
        if spec.dimensions.is_empty() {
            // Two environments with at least 20 spans a minute each: the volume is never below 30.
            want("its one series", states.first().map(|s| s.status), &quiet);
        } else {
            want("prod", status_of("prod"), &[Some(AlertStatus::Firing)]);
            want("dev", status_of("dev"), &quiet);
            let silent: &[Option<AlertStatus>] = match spec.no_data {
                NoDataPolicy::NoData => &[None, Some(AlertStatus::NoData)],
                NoDataPolicy::Ok => &quiet,
                NoDataPolicy::Keep => &[
                    None,
                    Some(AlertStatus::Ok),
                    Some(AlertStatus::Pending),
                    Some(AlertStatus::Firing),
                    Some(AlertStatus::Resolved),
                ],
            };
            want("staging", status_of("staging"), silent);
        }
        for state in &states {
            match state.status {
                AlertStatus::NoData => ledger.stats.no_data_at_end += 1,
                AlertStatus::Firing => ledger.stats.firing_at_end += 1,
                _ => continue,
            }
            if state.status != AlertStatus::Firing {
                continue;
            }
            let series = series_values(&state.series_key);
            let fired_for = now.micros() - state.since.micros();
            // A repeat is due every `renotify`; allow three intervals for its tick and its delivery.
            let recent = i64::from(RENOTIFY_SECS + 3 * rule.interval_secs + 120) * 1_000_000;
            for channel in &spec.channels {
                let notices: Vec<i64> = ledger
                    .delivered
                    .iter()
                    .map(|delivered| &delivered.notification)
                    .filter(|n| {
                        n.rule_id == rule.id.uuid()
                            && n.channel_id == channel.uuid()
                            && matches!(n.kind, NoticeKind::Firing | NoticeKind::Renotify)
                            && n.series
                                .iter()
                                .map(|l| l.value.as_str())
                                .eq(series.iter().map(String::as_str))
                    })
                    .map(|n| n.at_micros)
                    .collect();
                let label = format!(
                    "A3: rule {} series {} fires since {} and channel {channel}",
                    rule.name, state.series_key, state.since
                );
                if fired_for > 2 * MINUTE_MICROS
                    && !notices.iter().any(|at| *at >= state.since.micros())
                {
                    ledger
                        .violations
                        .push(format!("{label} got no firing notice of this episode"));
                }
                if fired_for > recent
                    && tail_micros > 2 * recent
                    && !notices.iter().any(|at| now.micros() - *at <= recent)
                {
                    ledger.violations.push(format!(
                        "{label} got no firing notice in the last {} s",
                        recent / 1_000_000
                    ));
                }
            }
        }
    }
    // A1, the other way: the outbox holds nothing else.
    let rows: BTreeSet<NoticeKey> = outbox.keys().cloned().collect();
    for missing in expected.difference(&rows) {
        ledger.violations.push(format!(
            "A1: the history says {missing:?} was notified and the outbox has no row for it"
        ));
    }
    for extra in rows.difference(&expected) {
        ledger.violations.push(format!(
            "A1: the outbox has a row for {extra:?} and the history has no such transition"
        ));
    }
    Ok(())
}

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let database = sim.host("postgres", &sim.disk("postgres"), 0);
    let clock = Arc::clone(&database.env.clock);
    let model = PgModel::new(
        Arc::clone(&clock),
        sim.rng("alerts-sim/pg").as_ref(),
        PgFaults::default(),
    );
    let store = Arc::new(ChModel::new(
        sim.rng("alerts-sim/ch").as_ref(),
        StoreFlavour::SingleNode,
        Granularity::Day,
        Duration::from_secs(60),
    ));
    let ledger = Arc::new(Mutex::new(Ledger::default()));
    let (index, _writer) = RecentIndex::new(
        RecentConfig::new(1 << 20, StoreFlavour::SingleNode),
        Arc::clone(&clock),
        Arc::new(EmptyWal),
    );
    let telemetry: Arc<dyn TelemetryStore> = Arc::clone(&store) as Arc<dyn TelemetryStore>;
    let service = QueryService::new(
        Stores::new(telemetry),
        Arc::clone(&clock),
        index,
        WalEpoch::from_uuid(id(7)),
    );
    let hook = Arc::new(Receiver::new(&clock, sim.rng("alerts-sim/hook")));
    let mail = Arc::new(Receiver::new(&clock, sim.rng("alerts-sim/mail")));
    let mailer = Arc::new(SimMailer {
        server: Arc::clone(&mail),
        accepted: Mutex::new(Vec::new()),
    });
    let transport = SimTransport {
        mail: NetTransport::new(
            &database.env,
            Egress::new(&database.env, EgressPolicy::default(), TlsRoots::WebPki),
            Some(Arc::clone(&mailer) as Arc<dyn Mailer>),
            "crucible.test",
            None,
        ),
        hook: Arc::clone(&hook),
        ledger: Arc::clone(&ledger),
    };
    let world = World {
        model: model.clone(),
        store: Arc::clone(&store),
        database: database.env.clone(),
        ledger: Arc::clone(&ledger),
        source: Arc::new(CountedSource {
            inner: QuerySource::new(service),
            ledger: Arc::clone(&ledger),
        }),
        transport: Arc::new(transport),
        sealer: Arc::new(Sealer::new("master-0", [7; 32])),
        hook: Arc::clone(&hook),
        mail: Arc::clone(&mail),
        options,
    };
    let failed = |violations: Vec<String>| Outcome {
        seed: sim.seed(),
        stats: Stats::default(),
        violations,
    };
    let fixture = match seed(&world).await {
        Ok(fixture) => fixture,
        Err(error) => return failed(vec![error]),
    };
    // The scenario covers the run, the clock skips and a margin on both sides. It is steady from the
    // latest wall time at which the faulty phase can end.
    let start = clock.wall_now().micros().div_euclid(MINUTE_MICROS);
    let minutes = |d: Duration| i64::try_from(d.as_secs() / 60).unwrap_or(i64::MAX);
    let steady_from = start + minutes(options.faulty_for) + minutes(MAX_SKIP);
    let last = steady_from + minutes(options.tail) + 30;
    let scenario = Scenario::new(
        sim.rng("alerts-sim/scenario").as_ref(),
        start - 30,
        steady_from,
        last,
    );
    if let Err(error) = scenario.write(&store, fixture.project).await {
        return failed(vec![error]);
    }

    model.set_faults(faults(true));
    hook.refuse_ppm.store(30_000, Ordering::SeqCst);
    mail.refuse_ppm.store(30_000, Ordering::SeqCst);
    let faulty_until = clock.now().saturating_add(options.faulty_for).nanos();
    let end = faulty_until + nanos(options.tail);
    let nodes: Vec<_> = (0..options.nodes)
        .map(|n| node(&sim, world.clone(), n, faulty_until, end))
        .collect();
    let calm = async {
        clock.sleep_until(Instant::from_nanos(faulty_until)).await;
        model.set_faults(faults(false));
        store.end_outage();
        hook.heal();
        mail.heal();
        turn_channels_on(&world, fixture.project).await;
    };
    futures::join!(
        futures::future::join_all(nodes),
        chaos(
            world.clone(),
            Arc::clone(&database.clock),
            sim.rng("alerts-sim/chaos"),
            faulty_until
        ),
        calm,
    );
    let mut ledger = std::mem::take(&mut *lock(&ledger));
    ledger.stats.mails = lock(&mailer.accepted).len() as u64;
    if let Err(error) = audit(&world, &scenario, &fixture, &mut ledger).await {
        ledger
            .violations
            .push(format!("the oracle could not read the store: {error}"));
    }
    database.stop().await;
    Outcome {
        seed: sim.seed(),
        stats: ledger.stats,
        violations: ledger.violations,
    }
}

/// Runs the alerts simulation for one seed.
pub fn run_alerts_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}
