//! The read path of the MCP analytics in simulation (invariant I7 per capture source; contracts 9.5 and 9.9).
//!
//! Each run draws, from its seed, a project's MCP traffic: servers on both sides, each observed by a random
//! set of capture sources (an SDK wrapper, the proxy over stdio or HTTP, plain OTLP), so a call is often
//! captured twice; error classes with caller faults; tools; deploys. Every observation is one `McpOpRecord`,
//! turned into the rows of `mcp_ops`, `mcp_tool_rollup` and `mcp_deploys` by the shipper's own row builders and
//! landed in the store model in several batches, whose parts then merge. The real `QueryService` reads it back
//! under every `source` choice and both sides, and every answer must equal the reference computed from the
//! records themselves:
//!
//! - **auto** reads each server from its best-ranked class present on the side (SDK, then proxy, then OTLP), and
//!   a named class reads that class only;
//! - the calls, errors and caller-fault calls of a server and of a tool are those of the chosen class, never a
//!   sum over classes; with `include_caller_faults` the caller-fault calls count;
//! - the `mcp_ops` metrics source gives the same calls per server;
//! - the operation list holds exactly the chosen class's operations of the side.
//!
//! A failure is a violation of I7 that names the read and the server.

use std::collections::{BTreeMap, BTreeSet};

use std::sync::Arc;
use std::time::Duration;

use crucible_chfmt::Compression;
use crucible_env::{InsertSettings, StoreFlavour, Stores, TelemetryStore};
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
    mcp_deploy, mcp_deploys_row, mcp_ops_row, mcp_tool_rollup, mcp_tool_rollup_row,
};
use crucible_shipper::value::Row;
use crucible_shipper::{BatchId, pair_block, pair_token};
use rand_chacha::ChaCha20Rng;
use rand_core::{Rng, SeedableRng};

use crate::ch_model::ChModel;

/// 2026-11-17T09:00:00Z: the start of every run's hour of traffic.
const HOUR_START: i64 = 1_794_906_000_000_000;
const MINUTE: i64 = 60_000_000;
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
            op.start_time = Timestamp::from_micros(HOUR_START + minute * MINUTE + call as i64);
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
        let Ok(window) = TimeWindow::new(
            Timestamp::from_micros(HOUR_START),
            Timestamp::from_micros(HOUR_START + 60 * MINUTE),
        ) else {
            return outcome;
        };
        check(&service, &ctx, window, &ops, &mut outcome).await;
        drop(host);
        outcome
    })
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
