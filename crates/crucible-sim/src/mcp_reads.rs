//! The read path of the MCP analytics in simulation (invariant I7 per capture source; contracts 9.5 and 9.9).
//!
//! Each run draws, from its seed, a project's MCP traffic over three days: servers on both sides, each observed
//! by a random set of capture sources (an SDK wrapper, the proxy over stdio or HTTP, plain OTLP), so a call is
//! often captured twice; error classes with caller faults; tools; deploys. Every observation is one
//! `McpOpRecord`, turned into the rows of `mcp_ops`, `mcp_tool_rollup`, `mcp_tool_rollup_hourly` and
//! `mcp_deploys` by the shipper's own row builders and landed in the store model in several batches, whose parts
//! then merge. The real `QueryService` reads it back over windows of mixed lengths and offsets (three hours,
//! which read minute rows only; 24 hours that start and end inside an hour; the three days, all whole hours),
//! the long ones taking their whole hours from the hourly tier, under every `source` choice and both sides, and
//! every answer must equal the reference computed from the records of the window themselves:
//!
//! - **auto** reads each server from its best-ranked class present on the side (SDK, then proxy, then OTLP), and
//!   a named class reads that class only;
//! - the calls, errors and caller-fault calls of a server and of a tool are those of the chosen class, never a
//!   sum over classes; with `include_caller_faults` the caller-fault calls count;
//! - the `mcp_ops` metrics source gives the same calls per server;
//! - the totals an alert reads give each server's calls, errors and error rate of the chosen class, a server
//!   with calls and no error at zero;
//! - the operation list holds exactly the chosen class's operations of the side.
//!
//! A failure is a violation of I7 that names the read and the server. [`run_mcp_alert`] then follows an
//! `mcp.error_rate` alert rule through one evaluation by the production evaluator.

use std::collections::{BTreeMap, BTreeSet};

use std::sync::Arc;
use std::time::Duration;

use crucible_alerts::schedule::{current_seq, window_of};
use crucible_alerts::{
    Alerts, Comparison, EVALUATION_POLICY, Evaluator, McpRuleRead, NO_DATA_FOR_DEFAULT_SECS,
    NoDataPolicy, QuerySource, RENOTIFY_DEFAULT_SECS, RuleInput, RuleSpec, SeriesSource,
    evaluation_job,
};
use crucible_chfmt::Compression;
use crucible_control::{AlertStatus, ControlStore, JsonDoc, Project, Queue, Repos as _};
use crucible_env::{InsertSettings, StoreFlavour, Stores, TelemetryStore};
use crucible_jobs::ClaimCtx;
use crucible_plan::{
    Aggregation, CaptureClass, DimensionId, Fill, Filter, Granularity, McpOpsFilter, MeasureId,
    MetricsQuery, Source, SourceChoice,
};
use crucible_query::{
    McpMetricsRequest, McpOperationsQuery, McpQuery, MetricValue, QueryService, ReadContext,
};
use crucible_recent::{RecentConfig, RecentIndex, SpanRead, WalLocator, WalReadError, WalView};
use crucible_records::{
    CancelSource, CaptureSource, Direction, Era, ErrorClass, ErrorSubclass, GroupKeyKind,
    IdentitySource, Ingestion, Lsn, McpOpRecord, McpSide, McpStatus, McpTargetKind, ProjectId,
    ResultType, SpanId, TableId, TimeWindow, Timestamp, TraceId, Transport, Truncation, WalEpoch,
};
use crucible_schema::{Granularity as Partitioning, RetentionSettings, route};
use crucible_shipper::tables::{
    mcp_deploy, mcp_deploys_row, mcp_ops_row, mcp_tool_rollup, mcp_tool_rollup_hourly_row,
    mcp_tool_rollup_row,
};
use crucible_shipper::value::Row;
use crucible_shipper::{BatchId, pair_block, pair_token};
use rand_chacha::ChaCha20Rng;
use rand_core::{Rng, SeedableRng};

use crate::ch_model::ChModel;
use crate::pg_model::{PgFaults, PgModel};
use crate::runtime::Sim;

/// 2026-11-17T09:00:00Z: the start of every run's three days of traffic.
const HOUR_START: i64 = 1_794_906_000_000_000;
const MINUTE: i64 = 60_000_000;
const HOUR: i64 = 60 * MINUTE;
/// The hours of traffic of a run.
const TRAFFIC_HOURS: u32 = 72;
/// The windows every read runs over, from the start of the traffic: three hours that start ten minutes into an
/// hour (minute rows only), 24 hours that start and end 25 minutes into an hour (whole hours from the hourly
/// tier, the minutes of the first and last hour from the minute rollup), and the three days (whole hours only).
const WINDOWS: [(i64, i64); 3] = [
    (30 * HOUR + 10 * MINUTE, 33 * HOUR + 10 * MINUTE),
    (7 * HOUR + 25 * MINUTE, 31 * HOUR + 25 * MINUTE),
    (0, 72 * HOUR),
];
const SERVERS: [&str; 3] = ["alpha", "beta", "gamma"];
const TOOLS: [&str; 3] = ["read", "write", "list"];

/// What one run did and found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpReadOutcome {
    /// The seed.
    pub seed: u64,
    /// The operations landed: one per observation of a call.
    pub operations: usize,
    /// The (server, side) pairs whose calls more than one capture class observed.
    pub double_captured: usize,
    /// The reads compared with the reference.
    pub reads: usize,
    /// The violations of I7, empty when the run passed.
    pub violations: Vec<String>,
}

/// The WAL of a node that acknowledged nothing: the recent index is caught up and empty, so every answer comes
/// from the store.
#[derive(Debug)]
struct NoWal;

impl WalView for NoWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::ZERO
    }

    fn read_span(&self, _: ProjectId, _: WalLocator) -> SpanRead {
        Box::pin(async {
            Err(WalReadError::Failed(
                "the simulation holds no WAL".to_owned(),
            ))
        })
    }
}

/// The classes that observe one (server, side), and which proxy transports.
#[derive(Clone, Debug)]
struct Observers {
    sdk: bool,
    proxy: Vec<CaptureSource>,
    otlp: bool,
}

impl Observers {
    fn draw(rng: &mut ChaCha20Rng) -> Self {
        loop {
            let bits = rng.next_u32();
            let proxy = match bits >> 3 & 3 {
                0 => Vec::new(),
                1 => vec![CaptureSource::ProxyStdio],
                2 => vec![CaptureSource::ProxyHttp],
                _ => vec![CaptureSource::ProxyStdio, CaptureSource::ProxyHttp],
            };
            let observers = Self {
                sdk: bits & 1 == 1,
                otlp: bits & 2 == 2,
                proxy,
            };
            if observers.sdk || observers.otlp || !observers.proxy.is_empty() {
                return observers;
            }
        }
    }

    /// The capture source each class observes one call with: a call travels over one proxy transport.
    fn sources(&self, rng: &mut ChaCha20Rng) -> Vec<CaptureSource> {
        let mut out = Vec::new();
        if self.sdk {
            out.push(CaptureSource::Sdk);
        }
        if !self.proxy.is_empty() {
            let at = rng.next_u32() as usize % self.proxy.len();
            out.push(self.proxy[at]);
        }
        if self.otlp {
            out.push(CaptureSource::Otlp);
        }
        out
    }
}

fn record(project: ProjectId, n: u64, side: McpSide, source: CaptureSource) -> McpOpRecord {
    let trace = TraceId::from_bytes([0x7e; 16]);
    McpOpRecord {
        project_id: project,
        trace_id: trace,
        span_id: SpanId::from_bytes(n.to_be_bytes()),
        parent_span_id: SpanId::ZERO,
        transport_link_trace_id: TraceId::ZERO,
        transport_link_span_id: SpanId::ZERO,
        server_name: "".into(),
        server_version: "".into(),
        deploy_id: "".into(),
        instance_id: "".into(),
        side,
        capture_source: source,
        era: Era::Modern,
        protocol_version: "2026-07-28".into(),
        transport: Transport::Stdio,
        method: "tools/call".into(),
        target_kind: McpTargetKind::Tool,
        target_name: "".into(),
        jsonrpc_id: "".into(),
        direction: Direction::ClientToServer,
        is_notification: false,
        is_batch: false,
        result_type: ResultType::Complete,
        mrtr_round: 0,
        mrtr_state_in: None,
        mrtr_state_out: None,
        task_id: "".into(),
        subscription_id: "".into(),
        start_time: Timestamp::from_micros(HOUR_START),
        duration_us: 0,
        progress_count: None,
        cancel_source: CancelSource::None,
        timed_out: false,
        status: McpStatus::Ok,
        error_class: ErrorClass::None,
        error_subclass: ErrorSubclass::None,
        jsonrpc_code: None,
        http_status: None,
        caller_fault: false,
        error_type: "".into(),
        error_message: "".into(),
        client_name: "agent".into(),
        client_version: "1".into(),
        client_capabilities_hash: 0,
        oauth_client_id: "".into(),
        user_subject_hash: None,
        user_issuer: "".into(),
        user_id: "".into(),
        tenant: "".into(),
        identity_source: IdentitySource::None,
        session_key: None,
        handle_id: "".into(),
        group_key: "".into(),
        group_key_kind: GroupKeyKind::Process,
        ip_prefix: "".into(),
        user_agent: "".into(),
        request_bytes: 0,
        response_bytes: 0,
        content_blocks: Vec::new(),
        content_captured: false,
        content_sample_rate: 0.0,
        content_truncation: Truncation::None,
        args: bytes::Bytes::new(),
        result: bytes::Bytes::new(),
        args_bytes: 0,
        result_bytes: 0,
        ingestion: Ingestion {
            wal_epoch: WalEpoch::from_bytes([0xe1; 16]),
            batch_seq: Lsn::new(1),
            lsn: Lsn::new(n),
            received_at: Timestamp::from_micros(HOUR_START),
        },
        normalizer_version: 1,
    }
}

/// The traffic of one run: every observation as a record.
fn traffic(seed: u64, project: ProjectId) -> Vec<McpOpRecord> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut observers = BTreeMap::new();
    for server in SERVERS {
        for side in [McpSide::Client, McpSide::Server] {
            observers.insert((server, side.value()), Observers::draw(&mut rng));
        }
    }
    let calls = 20 + rng.next_u32() as usize % 180;
    let mut out = Vec::new();
    for call in 0..calls {
        let server = SERVERS[rng.next_u32() as usize % SERVERS.len()];
        let side = if rng.next_u32().is_multiple_of(2) {
            McpSide::Client
        } else {
            McpSide::Server
        };
        let tool = TOOLS[rng.next_u32() as usize % TOOLS.len()];
        let hour = i64::from(rng.next_u32() % TRAFFIC_HOURS);
        let minute = i64::from(rng.next_u32() % 60);
        let (class, code) = match rng.next_u32() % 10 {
            0 => (ErrorClass::ToolExecution, None),
            1 => (ErrorClass::Validation, Some(-32602)),
            2 => (ErrorClass::Timeout, None),
            _ => (ErrorClass::None, None),
        };
        let deploy = if minute < 30 { "v1" } else { "v2" };
        let duration = 100 + u64::from(rng.next_u32() % 50_000);
        let sources = observers[&(server, side.value())].sources(&mut rng);
        for source in sources {
            let n = out.len() as u64 + 1;
            let mut op = record(project, n, side, source);
            op.server_name = server.into();
            op.deploy_id = deploy.into();
            op.server_version = deploy.into();
            op.target_name = tool.into();
            op.start_time =
                Timestamp::from_micros(HOUR_START + hour * HOUR + minute * MINUTE + call as i64);
            op.duration_us = duration;
            op.error_class = class;
            op.jsonrpc_code = code;
            op.status = match class {
                ErrorClass::None => McpStatus::Ok,
                ErrorClass::Timeout => McpStatus::TransportError,
                ErrorClass::Validation => McpStatus::ProtocolError,
                _ => McpStatus::ToolError,
            };
            // The caller-fault rule of contract 9.2: invalid parameters, on the server side.
            op.caller_fault = side == McpSide::Server && code == Some(-32602);
            op.ingestion.received_at = op.start_time;
            out.push(op);
        }
    }
    out
}

/// What a read must give for one server: its calls, errors and caller-fault calls under one class.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Expected {
    calls: u64,
    errors: u64,
    caller_faults: u64,
}

/// The reference: per (side, server), the classes present, and per class the totals of its records.
fn reference(
    ops: &[McpOpRecord],
    side: McpSide,
    include_caller_faults: bool,
) -> BTreeMap<String, BTreeMap<CaptureClass, Expected>> {
    let mut out: BTreeMap<String, BTreeMap<CaptureClass, Expected>> = BTreeMap::new();
    for op in ops.iter().filter(|op| op.side == side) {
        let entry = out
            .entry(op.server_name.to_string())
            .or_default()
            .entry(CaptureClass::of(op.capture_source))
            .or_default();
        if op.caller_fault {
            entry.caller_faults += 1;
            if !include_caller_faults {
                continue;
            }
        }
        entry.calls += 1;
        if op.error_class != ErrorClass::None {
            entry.errors += 1;
        }
    }
    out
}

/// The class a choice reads a server from, given the classes it has.
fn chosen(
    choice: SourceChoice,
    classes: &BTreeMap<CaptureClass, Expected>,
) -> Option<CaptureClass> {
    match choice {
        SourceChoice::Auto => classes.keys().next().copied(),
        SourceChoice::Class(class) => classes.contains_key(&class).then_some(class),
    }
}

/// Lands the records in the store model in `batches` batches, each one block per (table, partition) pair, as the
/// shipper would.
async fn land(model: &ChModel, ops: &[McpOpRecord], batches: usize) -> Result<(), String> {
    let epoch = WalEpoch::from_bytes([0xe1; 16]);
    let size = ops.len().div_ceil(batches.max(1)).max(1);
    for (n, chunk) in ops.chunks(size).enumerate() {
        let first = Lsn::new(n as u64 * 1_000 + 1);
        let batch = BatchId {
            epoch,
            first_lsn: first,
            last_lsn: Lsn::new(first.get() + 999),
        };
        let mut pairs: BTreeMap<(TableId, crucible_records::PartitionId), Vec<Row>> =
            BTreeMap::new();
        for (ordinal, op) in chunk.iter().enumerate() {
            // Each row carries the identity of its batch and an LSN inside it, as the shipper's rows do.
            let mut op = op.clone();
            op.ingestion.batch_seq = first;
            op.ingestion.lsn = Lsn::new(first.get() + ordinal as u64);
            let op = &op;
            let at = |table: TableId, time: Timestamp| route(table, Partitioning::Day, time);
            let row = |cells| Row {
                cells,
                lsn: op.ingestion.lsn,
                ordinal: ordinal as u32,
            };
            let ops_pair = (
                TableId::McpOps,
                at(TableId::McpOps, op.start_time).map_err(|e| e.to_string())?,
            );
            pairs
                .entry(ops_pair)
                .or_default()
                .push(row(mcp_ops_row(op)));
            let rollup = (
                TableId::McpToolRollup,
                at(TableId::McpToolRollup, op.start_time).map_err(|e| e.to_string())?,
            );
            pairs
                .entry(rollup)
                .or_default()
                .push(row(mcp_tool_rollup_row(&mcp_tool_rollup(op))));
            let hourly = (
                TableId::McpToolRollupHourly,
                at(TableId::McpToolRollupHourly, op.start_time).map_err(|e| e.to_string())?,
            );
            pairs
                .entry(hourly)
                .or_default()
                .push(row(mcp_tool_rollup_hourly_row(&mcp_tool_rollup(op))));
            if let Some(deploy) = mcp_deploy(op) {
                let pair = (
                    TableId::McpDeploys,
                    at(TableId::McpDeploys, op.start_time).map_err(|e| e.to_string())?,
                );
                pairs
                    .entry(pair)
                    .or_default()
                    .push(row(mcp_deploys_row(&deploy)));
            }
        }
        for (pair, rows) in pairs {
            let marker_at = Timestamp::from_micros(HOUR_START);
            let block = pair_block(&batch, pair, rows, marker_at, Compression::Lz4)
                .map_err(|e| format!("a block did not build: {e}"))?;
            let token = pair_token(&batch, pair).map_err(|e| e.to_string())?;
            model
                .insert_block(block, InsertSettings::new(token, StoreFlavour::SingleNode))
                .await
                .map_err(|e| {
                    format!(
                        "the model refused a block of {}: {e:?}; {:?}",
                        pair.0.name(),
                        model.violations()
                    )
                })?;
        }
    }
    Ok(())
}

/// Runs one seed: lands its traffic and compares every read with the reference.
pub fn run_mcp_reads(seed: u64) -> McpReadOutcome {
    crate::run(seed, |sim| async move {
        let disk = sim.disk("mcp-reads");
        let host = sim.host("mcp-reads/1", &disk, 0);
        let env = host.env.clone();
        let project = ProjectId::from_bytes([0x42; 16]);
        let ops = traffic(seed, project);
        let model = ChModel::new(
            env.rng.as_ref(),
            StoreFlavour::SingleNode,
            Partitioning::Day,
            Duration::from_secs(60),
        );
        let mut outcome = McpReadOutcome {
            seed,
            operations: ops.len(),
            double_captured: double_captured(&ops),
            reads: 0,
            violations: Vec::new(),
        };
        let batches = 1 + (seed % 4) as usize;
        if let Err(error) = land(&model, &ops, batches).await {
            outcome.violations.push(error);
            return outcome;
        }
        if seed.is_multiple_of(2) {
            model.merge_all();
        }
        let (index, _writer) = RecentIndex::new(
            RecentConfig::new(1 << 20, StoreFlavour::SingleNode),
            env.clock.clone(),
            Arc::new(NoWal),
        );
        let service = QueryService::new(
            Stores::new(Arc::new(model.clone()) as Arc<dyn TelemetryStore>),
            env.clock.clone(),
            index,
            WalEpoch::from_bytes([0xe1; 16]),
        );
        let Ok(scope) = crucible_auth::test_support::scope(project) else {
            outcome
                .violations
                .push("the project could not be scoped".to_owned());
            return outcome;
        };
        let ctx = ReadContext::new(
            scope,
            RetentionSettings::default(),
            crucible_plan::Caller::Session,
        );
        for (from, to) in WINDOWS {
            let Ok(window) = TimeWindow::new(
                Timestamp::from_micros(HOUR_START + from),
                Timestamp::from_micros(HOUR_START + to),
            ) else {
                return outcome;
            };
            let inside: Vec<McpOpRecord> = ops
                .iter()
                .filter(|op| in_window(window, op.start_time))
                .cloned()
                .collect();
            check(&service, &ctx, window, &inside, &mut outcome).await;
        }
        drop(host);
        outcome
    })
}

/// Whether a read over `window` counts an operation that started at `start`. The windows are whole minutes, so
/// the minute rollups (a minute row counts when its minute overlaps the window) and `mcp_ops` (by `start_time`)
/// agree on it.
fn in_window(window: TimeWindow, start: Timestamp) -> bool {
    let minute = start.floor_minute();
    window.from().floor_minute() <= minute
        && minute <= window.to().saturating_add_micros(-1).floor_minute()
}

async fn check(
    service: &QueryService,
    ctx: &ReadContext,
    window: TimeWindow,
    ops: &[McpOpRecord],
    outcome: &mut McpReadOutcome,
) {
    let choices = [
        SourceChoice::Auto,
        SourceChoice::Class(CaptureClass::Sdk),
        SourceChoice::Class(CaptureClass::Proxy),
        SourceChoice::Class(CaptureClass::Otlp),
    ];
    for side in [McpSide::Client, McpSide::Server] {
        for include_caller_faults in [false, true] {
            let expected = reference(ops, side, include_caller_faults);
            for choice in choices {
                let query = McpQuery {
                    window,
                    side,
                    source: choice,
                    include_caller_faults,
                    server: None,
                };
                let label = format!(
                    "{} side, source {}, caller faults {include_caller_faults}",
                    side.as_str(),
                    choice.as_str()
                );
                servers(service, ctx, &query, &expected, &label, outcome).await;
                if !include_caller_faults {
                    metrics(service, ctx, &query, &expected, &label, outcome).await;
                    alert_totals(service, ctx, &query, &expected, &label, outcome).await;
                    operations(service, ctx, &query, ops, &label, outcome).await;
                }
            }
        }
    }
}

fn violate(outcome: &mut McpReadOutcome, message: String) {
    outcome.violations.push(format!("I7: {message}"));
}

async fn servers(
    service: &QueryService,
    ctx: &ReadContext,
    query: &McpQuery,
    expected: &BTreeMap<String, BTreeMap<CaptureClass, Expected>>,
    label: &str,
    outcome: &mut McpReadOutcome,
) {
    outcome.reads += 1;
    let found = match service.mcp_servers(ctx, query).await {
        Ok(found) => found,
        Err(error) => {
            return violate(
                outcome,
                format!("the servers read failed ({label}): {error}"),
            );
        }
    };
    let mut want: BTreeMap<&str, Expected> = BTreeMap::new();
    for (server, classes) in expected {
        let Some(class) = chosen(query.source, classes) else {
            continue;
        };
        want.insert(server.as_str(), classes[&class]);
        let named = found
            .capture_sources
            .iter()
            .find(|s| s.server_name == *server)
            .and_then(|s| s.chosen);
        if named != Some(class) {
            violate(
                outcome,
                format!("server {server} was read from {named:?}, not {class:?} ({label})"),
            );
        }
    }
    let got: BTreeMap<&str, Expected> = found
        .servers
        .iter()
        .map(|s| {
            (
                &*s.server_name,
                Expected {
                    calls: s.totals.calls,
                    errors: s.totals.errors,
                    caller_faults: s.totals.caller_faults,
                },
            )
        })
        .collect();
    // A server whose chosen class holds only excluded caller faults has no calls and may be absent.
    let want: BTreeMap<&str, Expected> = want
        .into_iter()
        .filter(|(server, e)| e.calls > 0 || e.caller_faults > 0 || got.contains_key(server))
        .collect();
    if got != want {
        violate(
            outcome,
            format!("servers differ from the reference ({label}): got {got:?}, want {want:?}"),
        );
    }
    // Every server's calls are one class's, never the sum of two.
    let summed: BTreeSet<&str> = expected
        .iter()
        .filter(|(server, classes)| {
            let total: u64 = classes.values().map(|e| e.calls).sum();
            classes.len() > 1
                && got
                    .get(server.as_str())
                    .is_some_and(|g| g.calls == total && total > 0)
                && classes.values().filter(|e| e.calls > 0).count() > 1
        })
        .map(|(server, _)| server.as_str())
        .collect();
    if !summed.is_empty() {
        violate(
            outcome,
            format!("the calls of {summed:?} are summed over capture sources ({label})"),
        );
    }
}

async fn metrics(
    service: &QueryService,
    ctx: &ReadContext,
    query: &McpQuery,
    expected: &BTreeMap<String, BTreeMap<CaptureClass, Expected>>,
    label: &str,
    outcome: &mut McpReadOutcome,
) {
    outcome.reads += 1;
    let request = McpMetricsRequest {
        window: query.window,
        query: MetricsQuery {
            source: Source::McpOps,
            measures: vec![(MeasureId::McpCalls, Aggregation::Sum)],
            dimensions: vec![DimensionId::McpServer],
            filter: Filter::all(),
            granularity: Granularity::Auto,
            fill: Fill::None,
            top_n: None,
        },
        side: query.side,
        source: query.source,
        include_caller_faults: false,
    };
    let found = match service.mcp_metrics(ctx, request).await {
        Ok(found) => found,
        Err(error) => {
            return violate(
                outcome,
                format!("the metrics read failed ({label}): {error}"),
            );
        }
    };
    let mut got: BTreeMap<String, u64> = BTreeMap::new();
    for series in &found.series {
        let total: u64 = series
            .points
            .iter()
            .filter_map(|p| match p.value {
                Some(MetricValue::UInt(v)) => Some(v),
                _ => None,
            })
            .sum();
        if let Some(server) = series.key.first() {
            *got.entry(server.to_string()).or_default() += total;
        }
    }
    got.retain(|_, calls| *calls > 0);
    let want: BTreeMap<String, u64> = expected
        .iter()
        .filter_map(|(server, classes)| {
            let class = chosen(query.source, classes)?;
            let calls = classes[&class].calls;
            (calls > 0).then(|| (server.clone(), calls))
        })
        .collect();
    if got != want {
        violate(
            outcome,
            format!("mcp.calls per server differs ({label}): got {got:?}, want {want:?}"),
        );
    }
}

/// The totals an alert reads (contract 9.9): per server, the calls, errors and error rate of the class the
/// selection names, caller faults out, and a server whose chosen class has calls and no error at zero.
async fn alert_totals(
    service: &QueryService,
    ctx: &ReadContext,
    query: &McpQuery,
    expected: &BTreeMap<String, BTreeMap<CaptureClass, Expected>>,
    label: &str,
    outcome: &mut McpReadOutcome,
) {
    for (measure, aggregation) in [
        (MeasureId::McpCalls, Aggregation::Sum),
        (MeasureId::McpErrorCount, Aggregation::Sum),
        (MeasureId::McpErrorRate, Aggregation::Rate),
    ] {
        outcome.reads += 1;
        let request = McpMetricsRequest {
            window: query.window,
            query: MetricsQuery {
                source: Source::McpOps,
                measures: vec![(measure, aggregation)],
                dimensions: vec![DimensionId::McpServer],
                filter: Filter::all(),
                granularity: Granularity::Auto,
                fill: Fill::None,
                top_n: None,
            },
            side: query.side,
            source: query.source,
            include_caller_faults: false,
        };
        let name = measure.as_str();
        let found = match service.mcp_metrics_totals(ctx, request, true, 50).await {
            Ok(found) => found,
            Err(error) => {
                return violate(
                    outcome,
                    format!("the alert read of {name} failed ({label}): {error}"),
                );
            }
        };
        let got: BTreeMap<String, Option<u64>> = found
            .series
            .iter()
            .filter_map(|series| {
                let value = match series.value {
                    Some(MetricValue::UInt(v)) => Some(v),
                    _ => None,
                };
                Some((series.key.first()?.to_string(), value))
            })
            .collect();
        let want: BTreeMap<String, Option<u64>> = expected
            .iter()
            .filter_map(|(server, classes)| {
                let read = classes[&chosen(query.source, classes)?];
                let value = match measure {
                    MeasureId::McpErrorCount => read.errors,
                    MeasureId::McpErrorRate => {
                        let ppm =
                            u128::from(read.errors) * 1_000_000 / u128::from(read.calls.max(1));
                        u64::try_from(ppm).unwrap_or(u64::MAX)
                    }
                    _ => read.calls,
                };
                (read.calls > 0).then(|| (server.clone(), Some(value)))
            })
            .collect();
        if got != want || found.truncated {
            violate(
                outcome,
                format!(
                    "the alert read of {name} per server differs ({label}): got {got:?}, want {want:?}"
                ),
            );
        }
    }
}

async fn operations(
    service: &QueryService,
    ctx: &ReadContext,
    query: &McpQuery,
    ops: &[McpOpRecord],
    label: &str,
    outcome: &mut McpReadOutcome,
) {
    outcome.reads += 1;
    let classes: BTreeMap<String, BTreeSet<CaptureClass>> = ops
        .iter()
        .filter(|op| op.side == query.side)
        .fold(BTreeMap::new(), |mut acc, op| {
            acc.entry(op.server_name.to_string())
                .or_default()
                .insert(CaptureClass::of(op.capture_source));
            acc
        });
    let read_from = |server: &str| -> Option<CaptureClass> {
        let present = classes.get(server)?;
        match query.source {
            SourceChoice::Auto => present.iter().next().copied(),
            SourceChoice::Class(class) => present.contains(&class).then_some(class),
        }
    };
    let want: BTreeSet<SpanId> = ops
        .iter()
        .filter(|op| op.side == query.side && !op.caller_fault)
        .filter(|op| read_from(&op.server_name) == Some(CaptureClass::of(op.capture_source)))
        .map(|op| op.span_id)
        .collect();
    let mut got = BTreeSet::new();
    let mut cursor = None;
    loop {
        let page = McpOperationsQuery {
            query: query.clone(),
            filter: McpOpsFilter::default(),
            limit: 200,
            cursor,
        };
        let found = match service.mcp_operations(ctx, &page).await {
            Ok(found) => found,
            Err(error) => {
                return violate(
                    outcome,
                    format!("the operation list failed ({label}): {error}"),
                );
            }
        };
        got.extend(found.operations.iter().map(|op| op.span_id));
        match found.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    if got != want {
        violate(
            outcome,
            format!(
                "the operation list holds {} operations, the reference {} ({label})",
                got.len(),
                want.len()
            ),
        );
    }
}

/// Runs `seeds` seeds from `first` and returns the failed ones.
pub fn run_many(first: u64, seeds: u64) -> Vec<McpReadOutcome> {
    (first..first + seeds)
        .map(run_mcp_reads)
        .filter(|outcome| !outcome.violations.is_empty())
        .collect()
}

/// The (server, side) pairs of a run that more than one capture class observed.
fn double_captured(ops: &[McpOpRecord]) -> usize {
    let mut classes: BTreeMap<(&str, u8), BTreeSet<CaptureClass>> = BTreeMap::new();
    for op in ops {
        classes
            .entry((&op.server_name, op.side.value()))
            .or_default()
            .insert(CaptureClass::of(op.capture_source));
    }
    classes.values().filter(|c| c.len() > 1).count()
}

/// What one run of an MCP alert rule found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpAlertOutcome {
    /// The seed.
    pub seed: u64,
    /// The series that fire after the tick, by key.
    pub firing: Vec<String>,
    /// What differs from the scenario, empty when the run passed.
    pub violations: Vec<String>,
}

/// An `mcp.error_rate` rule end to end (contracts 9.2, 9.5, 9.9 and 12.6): the rule is created through the
/// alerts service, server-side calls are landed in the store model as the shipper writes them, and one tick is
/// claimed and handled by the production evaluator through the production read. Over the tick's window, per
/// server: `alpha` fails a share of its calls above the 5% threshold and fires; `beta`'s errors are caller
/// faults and its client side's belong to the other side, so it stays OK; `gamma` is captured by its SDK and by
/// the HTTP proxy, whose rows alone carry errors, and `auto` reads the SDK's, so it stays OK where a sum over
/// the sources would fire.
pub fn run_mcp_alert(seed: u64) -> McpAlertOutcome {
    crate::run(seed, |sim| async move {
        let mut outcome = McpAlertOutcome {
            seed,
            firing: Vec::new(),
            violations: Vec::new(),
        };
        if let Err(error) = alert_run(&sim, seed, &mut outcome).await {
            outcome.violations.push(error);
        }
        outcome
    })
}

/// The share of `alpha`'s calls that fail in a run, out of 20.
const fn alpha_failing(seed: u64) -> u64 {
    2 + seed % 5
}

/// The calls of the alert scenario inside `window`, per server, side and capture source: the calls, how many
/// of them fail, and how many more calls are caller faults.
fn alert_traffic(seed: u64, project: ProjectId, window: TimeWindow) -> Vec<McpOpRecord> {
    let plan = [
        (
            "alpha",
            McpSide::Server,
            CaptureSource::Sdk,
            20,
            alpha_failing(seed),
            0,
        ),
        (
            "beta",
            McpSide::Server,
            CaptureSource::Sdk,
            20,
            0,
            3 + seed % 4,
        ),
        ("beta", McpSide::Client, CaptureSource::Sdk, 20, 20, 0),
        ("gamma", McpSide::Server, CaptureSource::Sdk, 20, 0, 0),
        (
            "gamma",
            McpSide::Server,
            CaptureSource::ProxyHttp,
            20,
            5 + seed % 5,
            0,
        ),
    ];
    let mut out = Vec::new();
    for (server, side, source, calls, errors, caller_faults) in plan {
        for call in 0..calls + caller_faults {
            let n = out.len() as u64 + 1;
            let mut op = record(project, n, side, source);
            op.server_name = server.into();
            op.target_name = "read".into();
            op.start_time = window.from().saturating_add_micros(MINUTE + n as i64);
            op.ingestion.received_at = op.start_time;
            op.duration_us = 1_000;
            if call >= calls {
                op.error_class = ErrorClass::Validation;
                op.jsonrpc_code = Some(-32602);
                op.status = McpStatus::ProtocolError;
                op.caller_fault = true;
            } else if call < errors {
                op.error_class = ErrorClass::ToolExecution;
                op.status = McpStatus::ToolError;
            }
            out.push(op);
        }
    }
    out
}

async fn alert_run(sim: &Sim, seed: u64, outcome: &mut McpAlertOutcome) -> Result<(), String> {
    let database = sim.host("postgres", &sim.disk("postgres"), 0);
    let env = database.env.clone();
    let model = PgModel::new(
        Arc::clone(&env.clock),
        sim.rng("mcp-alert/pg").as_ref(),
        PgFaults::default(),
    );
    let fail = |what: &str, error: &dyn std::fmt::Display| format!("{what}: {error}");
    let project = Project {
        id: ProjectId::from_bytes([0x42; 16]),
        name: "Tools".to_owned(),
        slug: "tools".to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    };
    model
        .projects()
        .insert(&project)
        .await
        .map_err(|e| fail("inserting the project", &e))?;
    let store = ChModel::new(
        sim.rng("mcp-alert/ch").as_ref(),
        StoreFlavour::SingleNode,
        Partitioning::Day,
        Duration::from_secs(60),
    );
    let (index, _writer) = RecentIndex::new(
        RecentConfig::new(1 << 20, StoreFlavour::SingleNode),
        Arc::clone(&env.clock),
        Arc::new(NoWal),
    );
    let service = QueryService::new(
        Stores::new(Arc::new(store.clone()) as Arc<dyn TelemetryStore>),
        Arc::clone(&env.clock),
        index,
        WalEpoch::from_bytes([0xe1; 16]),
    );
    let source: Arc<dyn SeriesSource> = Arc::new(QuerySource::new(service));
    let control: Arc<dyn ControlStore> = Arc::new(model.clone());

    let spec = RuleSpec {
        source: Source::McpOps,
        mcp: Some(McpRuleRead {
            side: McpSide::Server,
            source: SourceChoice::Auto,
        }),
        measure: MeasureId::McpErrorRate,
        aggregation: Aggregation::Rate,
        dimensions: vec![DimensionId::McpServer],
        comparison: Comparison::Gt,
        threshold: 50_000,
        window_secs: 300,
        interval_secs: 60,
        for_secs: 0,
        no_data: NoDataPolicy::Keep,
        no_data_for_secs: NO_DATA_FOR_DEFAULT_SECS,
        renotify_secs: RENOTIFY_DEFAULT_SECS,
        channels: Vec::new(),
    };
    let alerts = Alerts::new(&env, Arc::clone(&control), Some(Arc::clone(&source)));
    let input = RuleInput {
        name: "MCP errors".to_owned(),
        enabled: true,
        spec: spec.clone(),
    };
    let rule = alerts
        .create(project.id, input)
        .await
        .map_err(|e| fail("creating the rule", &e))?
        .rule;

    // The tick that is due now, and the calls of its window.
    let seq = current_seq(rule.id, rule.interval_secs, env.clock.wall_now())
        .ok_or_else(|| "the clock is before the first tick of the rule".to_owned())?;
    let window = window_of(rule.id, rule.interval_secs, seq, spec.window_secs)
        .map_err(|_| "the tick has no window".to_owned())?;
    land(
        &store,
        &alert_traffic(seed, project.id, window),
        1 + (seed % 3) as usize,
    )
    .await?;
    if seed.is_multiple_of(2) {
        store.merge_all();
    }
    let job = evaluation_job(
        crucible_records::Uuid::from_v7_parts(1_800_000_000_000, u128::from(seed)),
        rule.id,
        rule.interval_secs,
        seq,
    );
    model
        .claims()
        .enqueue(Queue::AlertEvaluations, &job)
        .await
        .map_err(|e| fail("enqueueing the tick", &e))?;
    let claim = model
        .claims()
        .claim(Queue::AlertEvaluations, 1, EVALUATION_POLICY.lease_for)
        .await
        .map_err(|e| fail("claiming the tick", &e))?
        .pop()
        .ok_or_else(|| "the tick was not due".to_owned())?;
    Evaluator::new(source)
        .handle(ClaimCtx {
            env: env.clone(),
            store: control,
            claim,
        })
        .await
        .map_err(|e| fail("evaluating the tick", &e))?;

    let want = [
        (
            r#"["alpha"]"#,
            AlertStatus::Firing,
            alpha_failing(seed) * 50_000,
        ),
        (r#"["beta"]"#, AlertStatus::Ok, 0),
        (r#"["gamma"]"#, AlertStatus::Ok, 0),
    ];
    let states = model
        .alert_state()
        .list(rule.id)
        .await
        .map_err(|e| fail("reading the series", &e))?;
    let got: BTreeMap<&str, (AlertStatus, Option<i64>)> = states
        .iter()
        .map(|s| (s.series_key.as_str(), (s.status, s.value)))
        .collect();
    for (key, status, ppm) in want {
        let value = i64::try_from(ppm).ok();
        if got.get(key) != Some(&(status, value)) {
            outcome.violations.push(format!(
                "the series {key} is {:?}, the scenario says {status:?} at {ppm} ppm",
                got.get(key)
            ));
        }
    }
    if got.len() != want.len() {
        outcome.violations.push(format!(
            "the rule tracks {got:?}, the scenario three servers"
        ));
    }
    outcome.firing = states
        .iter()
        .filter(|s| s.status == AlertStatus::Firing)
        .map(|s| s.series_key.clone())
        .collect();
    database.stop().await;
    Ok(())
}
