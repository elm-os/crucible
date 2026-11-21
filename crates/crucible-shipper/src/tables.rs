//! The rows a span yields (contract sections 3.9, 3.10 and 6.4): `span_core`, `span_payload`, `trace_heads`,
//! `trace_stats` and the rollup partials `session_rollup`, `user_rollup` and `span_metrics`; the rows an MCP
//! operation yields (`mcp_ops`, `mcp_content` when content was captured, the partials `mcp_tool_rollup` and
//! `mcp_tool_rollup_hourly` and, on the server side, `mcp_deploys`); the `mcp_tool_inventory` partial of a tool
//! inventory entry; and the row a fact yields in its fact table (contract 3.8). Each function lists the cells in `TableDef` column order; a
//! test checks every row against its table's column types.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crucible_normalize::{Admission, NormalizedFrame};
use crucible_records::{
    BucketCounts, CaptureSource, Fact, FactBody, HeadKind, Histogram, Ingestion, Level,
    McpDeployRecord, McpOpRecord, McpSide, McpToolInventoryRecord, McpToolInventoryRow,
    McpToolRollupRow, NO_HEAD_START, NanoUsd, ObservationType, PartitionId, Pick, SemconvHistogram,
    SessionRollupRow, SpanCore, SpanMetricsRow, SpanRecord, StatusCode, TableId, Timestamp,
    TraceHead, TraceStatsRow, UserRollupRow, Uuid,
};
use crucible_schema::{Granularity, OutOfRange, route};

use crate::value::{Row, Value};

fn uuid(id: Uuid) -> Value {
    Value::Uuid(id.to_bytes())
}

fn en(value: u8) -> Value {
    Value::I8(i8::try_from(value).unwrap_or(i8::MAX))
}

fn u64s(items: impl Iterator<Item = u64>) -> Value {
    Value::Array(items.map(Value::U64).collect())
}

fn i64s(items: impl Iterator<Item = i64>) -> Value {
    Value::Array(items.map(Value::I64).collect())
}

fn date(t: Timestamp) -> Value {
    Value::U16(u16::try_from(t.date().day_number()).unwrap_or(0))
}

/// The ingestion columns of every plain table.
fn ingestion(cells: &mut Vec<Value>, i: &Ingestion) {
    cells.push(uuid(i.wal_epoch.uuid()));
    cells.push(Value::U64(i.batch_seq.get()));
    cells.push(Value::U64(i.lsn.get()));
    cells.push(Value::time(i.received_at));
}

/// The `span_core` row of a span.
pub fn span_core_row(span: &SpanRecord) -> Vec<Value> {
    let c = &span.core;
    let mut cells = vec![
        uuid(c.project_id.uuid()),
        Value::U32(c.start_minute()),
        Value::U32(c.trace_hash()),
        Value::time(c.start_time),
        Value::time(c.end_time),
        Value::bytes(c.span_id.as_bytes()),
        Value::bytes(c.parent_span_id.as_bytes()),
        Value::bytes(c.trace_id.as_bytes()),
        Value::flag(c.is_root),
        Value::flag(c.is_app_root),
        Value::flag(c.clock_skew),
        en(c.kind.value()),
        en(c.status_code.value()),
        en(c.observation_type.value()),
        en(c.level.value()),
        en(c.trace_public.value()),
        en(c.cost_source.value()),
        en(c.mcp_side.value()),
        en(c.io_shape.value()),
        en(c.input_truncation.value()),
        en(c.output_truncation.value()),
        en(c.source_format.value()),
    ];
    for text in [
        &c.name,
        &c.status_message,
        &c.service_version,
        &c.scope_version,
        &c.telemetry_sdk_version,
        &c.sdk_version,
        &c.release,
        &c.version,
        &c.trace_name,
        &c.user_id,
        &c.session_id,
        &c.prompt_name,
        &c.prompt_version,
        &c.service_name,
        &c.scope_name,
        &c.telemetry_sdk_name,
        &c.telemetry_sdk_language,
        &c.sdk_name,
        &c.environment,
        &c.model_requested,
        &c.model_response,
        &c.provider,
    ] {
        cells.push(Value::str(text));
    }
    cells.extend([
        Value::U64(c.duration_us()),
        Value::U64(c.input_tokens),
        Value::U64(c.output_tokens),
        Value::U64(c.total_tokens),
        Value::U64(c.input_bytes),
        Value::U64(c.output_bytes),
        c.completion_start_time.map_or(Value::Null, Value::time),
        Value::strs(&c.trace_tags),
        Value::strs(&c.tags),
        Value::strs(&c.tool_names),
        Value::strs(&c.finish_reasons),
        Value::Array(c.usage.iter().map(|u| Value::str(&u.key)).collect()),
        u64s(c.usage.iter().map(|u| u.units)),
        i64s(c.usage.iter().map(|u| u.cost.nanos())),
        Value::I64(c.total_cost_nusd.nanos()),
        uuid(c.price_model_id.unwrap_or(Uuid::NIL)),
        uuid(c.price_tier_id.unwrap_or(Uuid::NIL)),
        uuid(c.api_key_id.uuid()),
        Value::U32(c.tool_call_count),
        Value::U32(c.normalizer_version),
        Value::U32(c.event_bytes),
        Value::U8(c.compat_dialect),
        Value::str(&c.input_preview),
        Value::str(&c.output_preview),
        Value::bytes(&c.input_sha256.unwrap_or([0; 32])),
        Value::bytes(&c.output_sha256.unwrap_or([0; 32])),
        Value::U64(c.row_digest),
        Value::flag(c.usage_aggregate),
    ]);
    ingestion(&mut cells, &c.ingestion);
    cells
}

/// The `span_payload` row of a span.
pub fn span_payload_row(span: &SpanRecord) -> Vec<Value> {
    let c = &span.core;
    let p = &span.payload;
    let mut cells = vec![
        uuid(c.project_id.uuid()),
        Value::U32(c.start_minute()),
        Value::U32(c.trace_hash()),
        Value::bytes(c.span_id.as_bytes()),
        Value::bytes(c.trace_id.as_bytes()),
        Value::time(c.start_time),
        Value::str(&c.user_id),
        Value::str(&c.session_id),
    ];
    for body in [
        &p.input,
        &p.output,
        &p.system_instructions,
        &p.attributes,
        &p.resource_attributes,
        &p.events,
        &p.links,
        &p.tool_definitions,
        &p.tool_calls,
        &p.model_parameters,
        &p.trace_metadata,
    ] {
        cells.push(Value::bytes(body));
    }
    cells.extend([
        Value::strs(&p.metadata_keys),
        Value::strs(&p.metadata_values),
        Value::strs(&p.metadata_kv),
        Value::Array(
            p.usage_provided
                .iter()
                .map(|(k, _)| Value::str(k))
                .collect(),
        ),
        Value::Array(p.cost_provided.iter().map(|(k, _)| Value::str(k)).collect()),
        u64s(p.usage_provided.iter().map(|(_, units)| *units)),
        i64s(p.cost_provided.iter().map(|(_, cost)| cost.nanos())),
        Value::bytes(&p.trace_state),
        Value::Array(
            p.truncated_fields
                .iter()
                .map(|f| Value::str(f.name()))
                .collect(),
        ),
    ]);
    ingestion(&mut cells, &c.ingestion);
    cells
}

/// The head a span is for its trace (contract 3.9).
pub fn trace_head(span: &SpanRecord, head_kind: HeadKind) -> TraceHead {
    let c = &span.core;
    TraceHead {
        project_id: c.project_id,
        trace_id: c.trace_id,
        head_kind,
        span_id: c.span_id,
        start_time: c.start_time,
        end_time: c.end_time,
        name: c.name.clone(),
        level: c.level,
        status_code: c.status_code,
        user_id: c.user_id.clone(),
        session_id: c.session_id.clone(),
        tags: c.trace_tags.clone(),
        environment: c.environment.clone(),
        release: c.release.clone(),
        version: c.version.clone(),
        input_preview: c.input_preview.clone(),
        output_preview: c.output_preview.clone(),
        ingestion: c.ingestion,
    }
}

/// The `trace_heads` row of a head.
pub fn trace_head_row(head: &TraceHead) -> Vec<Value> {
    let mut cells = vec![
        uuid(head.project_id.uuid()),
        Value::time(head.start_time),
        Value::U32(head.trace_hash()),
        Value::bytes(head.trace_id.as_bytes()),
        Value::bytes(head.span_id.as_bytes()),
        en(head.head_kind.value()),
        Value::time(head.end_time),
        Value::str(&head.name),
        en(head.level.value()),
        en(head.status_code.value()),
        Value::str(&head.user_id),
        Value::str(&head.session_id),
        Value::strs(&head.tags),
        Value::str(&head.environment),
        Value::str(&head.release),
        Value::str(&head.version),
        Value::str(&head.input_preview),
        Value::str(&head.output_preview),
    ];
    ingestion(&mut cells, &head.ingestion);
    cells
}

/// Whether a span counts as an error in every aggregate: its status or its level is `error`.
pub fn is_error(c: &SpanCore) -> bool {
    c.status_code == StatusCode::Error || c.level == Level::Error
}

/// The tokens and cost a span adds to the trace, session, user and metrics sums (contracts 3.9 and 3.10): its
/// own, whatever its type, unless it has a descendant that carries usage or cost ([`SpanCore::counts_usage`]),
/// in which case nothing: the descendant is counted.
struct Counted {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    total_cost_nusd: NanoUsd,
}

fn counted(c: &SpanCore) -> Counted {
    if c.counts_usage() {
        Counted {
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            total_tokens: c.total_tokens,
            total_cost_nusd: c.total_cost_nusd,
        }
    } else {
        Counted {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            total_cost_nusd: NanoUsd::ZERO,
        }
    }
}

/// The partial one span contributes to its trace's stats (contract 3.9).
pub fn trace_stats(span: &SpanRecord, is_head: bool) -> TraceStatsRow {
    let c = &span.core;
    let counted = counted(c);
    let pick = |value: &str| Pick::new(c.is_root, c.end_time, c.span_id, value);
    let is_error = is_error(c);
    TraceStatsRow {
        project_id: c.project_id,
        trace_id: c.trace_id,
        row_day: c.start_time.date(),
        min_start: c.start_time,
        max_end: c.end_time,
        has_root: c.is_root,
        min_head_start: if is_head { c.start_time } else { NO_HEAD_START },
        max_level: c.level,
        span_count: 1,
        error_count: u64::from(is_error),
        generation_count: u64::from(c.observation_type == ObservationType::Generation),
        tool_count: u64::from(c.observation_type == ObservationType::Tool),
        mcp_count: u64::from(c.mcp_side != McpSide::None),
        total_cost_nusd: counted.total_cost_nusd,
        input_tokens: counted.input_tokens,
        output_tokens: counted.output_tokens,
        total_tokens: counted.total_tokens,
        environments: if c.environment.is_empty() {
            Vec::new()
        } else {
            vec![c.environment.clone()]
        },
        trace_name_pick: pick(&c.trace_name),
        user_id_pick: pick(&c.user_id),
        session_id_pick: pick(&c.session_id),
    }
}

/// The `trace_stats` data row of a partial: the marker columns hold zero.
pub fn trace_stats_row(row: &TraceStatsRow) -> Vec<Value> {
    vec![
        uuid(row.project_id.uuid()),
        Value::bytes(row.trace_id.as_bytes()),
        date(row.row_day.start()),
        Value::Uuid([0; 16]),
        Value::U64(0),
        Value::U32(0),
        Value::time(row.min_start),
        Value::time(row.max_end),
        Value::flag(row.has_root),
        Value::time(row.min_head_start),
        Value::U8(row.max_level.value()),
        Value::U64(row.span_count),
        Value::U64(row.error_count),
        Value::U64(row.generation_count),
        Value::U64(row.tool_count),
        Value::U64(row.mcp_count),
        Value::U64(row.input_tokens),
        Value::U64(row.output_tokens),
        Value::U64(row.total_tokens),
        Value::I64(row.total_cost_nusd.nanos()),
        Value::strs(&row.environments),
        Value::bytes(row.trace_name_pick.as_bytes()),
        Value::bytes(row.user_id_pick.as_bytes()),
        Value::bytes(row.session_id_pick.as_bytes()),
    ]
}

/// The zero marker columns of a data row of an aggregating table.
fn no_marker(cells: &mut Vec<Value>) {
    cells.extend([Value::Uuid([0; 16]), Value::U64(0), Value::U32(0)]);
}

/// A histogram as its `sumMap` cell: bucket indexes and counts, ascending by bucket.
fn histogram(h: &Histogram) -> Value {
    let (buckets, counts): (Vec<Value>, Vec<Value>) = h
        .iter()
        .map(|(bucket, count)| (Value::U16(bucket.index()), Value::U64(count)))
        .unzip();
    Value::Tuple(Box::new((Value::Array(buckets), Value::Array(counts))))
}

/// The fixed buckets of the MCP operation-duration metric as their `sumMap` cell.
fn semconv_histogram(h: &SemconvHistogram) -> Value {
    let (buckets, counts): (Vec<Value>, Vec<Value>) = h
        .iter()
        .map(|(bucket, count)| (Value::U8(bucket), Value::U64(count)))
        .unzip();
    Value::Tuple(Box::new((Value::Array(buckets), Value::Array(counts))))
}

/// 16 optional bytes of a `FixedString(16)` column: all zero when absent (contract 6.1).
fn hash16(hash: Option<[u8; 16]>) -> Value {
    Value::bytes(&hash.unwrap_or_default())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

/// The `mcp_ops` row of an MCP operation.
pub fn mcp_ops_row(op: &McpOpRecord) -> Vec<Value> {
    let nullable = |value: Option<Value>| value.unwrap_or(Value::Null);
    let mut cells = vec![
        uuid(op.project_id.uuid()),
        en(op.side.value()),
        en(op.capture_source.value()),
        en(op.era.value()),
        en(op.transport.value()),
        en(op.target_kind.value()),
        en(op.direction.value()),
        en(op.result_type.value()),
        en(op.cancel_source.value()),
        en(op.status.value()),
        en(op.error_class.value()),
        en(op.error_subclass.value()),
        en(op.identity_source.value()),
        en(op.group_key_kind.value()),
        en(op.content_truncation.value()),
        Value::U32(op.start_minute()),
        Value::U32(op.trace_hash()),
        Value::time(op.start_time),
        Value::bytes(op.span_id.as_bytes()),
        Value::bytes(op.parent_span_id.as_bytes()),
        Value::bytes(op.transport_link_span_id.as_bytes()),
        Value::bytes(op.trace_id.as_bytes()),
        Value::bytes(op.transport_link_trace_id.as_bytes()),
        hash16(op.user_subject_hash),
        hash16(op.mrtr_state_in),
        hash16(op.mrtr_state_out),
    ];
    for text in [
        &op.server_name,
        &op.method,
        &op.target_name,
        &op.client_name,
        &op.server_version,
        &op.deploy_id,
        &op.instance_id,
        &op.protocol_version,
        &op.jsonrpc_id,
        &op.task_id,
        &op.subscription_id,
        &op.error_type,
        &op.error_message,
        &op.client_version,
        &op.oauth_client_id,
        &op.user_id,
        &op.user_issuer,
        &op.tenant,
    ] {
        cells.push(Value::str(text));
    }
    cells.push(Value::str(
        &op.session_key.map(|key| hex(&key)).unwrap_or_default(),
    ));
    for text in [&op.handle_id, &op.group_key, &op.ip_prefix, &op.user_agent] {
        cells.push(Value::str(text));
    }
    cells.extend([
        Value::flag(op.is_notification),
        Value::flag(op.is_batch),
        Value::flag(op.timed_out),
        Value::flag(op.caller_fault),
        Value::flag(op.content_captured),
        Value::U16(op.mrtr_round),
        Value::U64(op.duration_us),
        Value::U64(op.request_bytes),
        Value::U64(op.response_bytes),
        Value::U64(op.client_capabilities_hash),
        nullable(op.progress_count.map(Value::U32)),
        nullable(op.jsonrpc_code.map(Value::I32)),
        nullable(op.http_status.map(Value::U16)),
        Value::Array(
            op.content_blocks
                .iter()
                .map(|(kind, _)| Value::str(kind))
                .collect(),
        ),
        Value::Array(
            op.content_blocks
                .iter()
                .map(|(_, count)| Value::U32(*count))
                .collect(),
        ),
        Value::F32(op.content_sample_rate),
        Value::U32(op.normalizer_version),
    ]);
    ingestion(&mut cells, &op.ingestion);
    cells
}

/// The `mcp_content` row of an MCP operation whose content was captured.
pub fn mcp_content_row(op: &McpOpRecord) -> Vec<Value> {
    let mut cells = vec![
        uuid(op.project_id.uuid()),
        en(op.side.value()),
        Value::U32(op.start_minute()),
        Value::U32(op.trace_hash()),
        Value::bytes(op.span_id.as_bytes()),
        Value::bytes(op.trace_id.as_bytes()),
        Value::time(op.start_time),
        Value::str(&op.user_id),
        hash16(op.user_subject_hash),
        Value::str(&op.target_name),
        Value::bytes(&op.args),
        Value::bytes(&op.result),
        Value::U64(op.args_bytes),
        Value::U64(op.result_bytes),
        en(op.content_truncation.value()),
    ];
    ingestion(&mut cells, &op.ingestion);
    cells
}

/// The partial one MCP operation contributes to the tool rollup of its minute (contract 3.10). The capture
/// source is part of the key, so rows of different sources never merge (contract 9.5).
pub fn mcp_tool_rollup(op: &McpOpRecord) -> McpToolRollupRow {
    let mut dur_hist = Histogram::new();
    dur_hist.record(op.duration_us);
    let mut semconv_hist: SemconvHistogram = BucketCounts::new();
    semconv_hist.record(op.duration_us);
    McpToolRollupRow {
        project_id: op.project_id,
        side: op.side,
        minute: Timestamp::from_micros(i64::from(op.start_minute()) * 1_000_000),
        server_name: op.server_name.clone(),
        deploy_id: op.deploy_id.clone(),
        capture_source: op.capture_source,
        era: op.era,
        method: op.method.clone(),
        target_name: op.target_name.clone(),
        client_name: op.client_name.clone(),
        error_class: op.error_class,
        caller_fault: op.caller_fault,
        count: 1,
        request_bytes: op.request_bytes,
        response_bytes: op.response_bytes,
        dur_hist,
        semconv_hist,
        last_seen: op.start_time,
    }
}

/// The `mcp_tool_rollup` data row of a partial.
pub fn mcp_tool_rollup_row(row: &McpToolRollupRow) -> Vec<Value> {
    let mut cells = vec![
        uuid(row.project_id.uuid()),
        en(row.side.value()),
        Value::U32(row.minute.start_minute()),
        Value::str(&row.server_name),
        Value::str(&row.deploy_id),
        en(row.capture_source.value()),
        en(row.era.value()),
        Value::str(&row.method),
        Value::str(&row.target_name),
        Value::str(&row.client_name),
        en(row.error_class.value()),
        Value::flag(row.caller_fault),
    ];
    no_marker(&mut cells);
    cells.extend([
        Value::U64(row.count),
        Value::U64(row.request_bytes),
        Value::U64(row.response_bytes),
        histogram(&row.dur_hist),
        semconv_histogram(&row.semconv_hist),
        Value::time(row.last_seen),
    ]);
    cells
}

/// The `mcp_tool_rollup_hourly` data row of a partial: the cells of its `mcp_tool_rollup` row, with the minute
/// cut to the start of its hour. The hourly tier has the minute rollup's columns in the same order
/// (`crucible_schema::hourly_tier`), so the rows of one hour merge to what its minute rows merge to.
pub fn mcp_tool_rollup_hourly_row(row: &McpToolRollupRow) -> Vec<Value> {
    let mut cells = mcp_tool_rollup_row(row);
    if let Some(time) = cells.get_mut(ROLLUP_TIME_CELL) {
        *time = Value::U32(row.minute.floor_hour().start_minute());
    }
    cells
}

/// The position of the time column (`minute`, or `hour` in the hourly tier) in a row of the MCP tool rollup.
const ROLLUP_TIME_CELL: usize = 2;

/// The deploy a server-side MCP operation was seen on (contract 3.7), or `None` for a client-side operation.
/// The rows of one batch with one key merge when their block is built, so each distinct deploy is one row of
/// the batch.
pub fn mcp_deploy(op: &McpOpRecord) -> Option<McpDeployRecord> {
    (op.side == McpSide::Server).then(|| McpDeployRecord {
        project_id: op.project_id,
        server_name: op.server_name.clone(),
        deploy_id: op.deploy_id.clone(),
        server_version: op.server_version.clone(),
        capture_source: op.capture_source,
        first_seen: op.start_time,
        last_seen: op.start_time,
    })
}

/// The `mcp_deploys` data row of a deploy record.
pub fn mcp_deploys_row(row: &McpDeployRecord) -> Vec<Value> {
    let mut cells = vec![
        uuid(row.project_id.uuid()),
        Value::str(&row.server_name),
        Value::str(&row.deploy_id),
        Value::str(&row.server_version),
        en(row.capture_source.value()),
    ];
    no_marker(&mut cells);
    cells.extend([Value::time(row.first_seen), Value::time(row.last_seen)]);
    cells
}

/// The partial one tool inventory record contributes to the inventory (contract 3.10).
pub fn mcp_tool_inventory(record: &McpToolInventoryRecord) -> McpToolInventoryRow {
    McpToolInventoryRow {
        project_id: record.project_id,
        server_name: record.server_name.clone(),
        deploy_id: record.deploy_id.clone(),
        tool_name: record.tool_name.clone(),
        schema_hash: record.schema_hash,
        first_seen: record.first_seen,
        last_seen: record.last_seen,
        capture_sources: vec![record.capture_source],
    }
}

/// The `mcp_tool_inventory` data row of a partial.
pub fn mcp_tool_inventory_row(row: &McpToolInventoryRow) -> Vec<Value> {
    let mut cells = vec![
        uuid(row.project_id.uuid()),
        Value::str(&row.server_name),
        Value::str(&row.deploy_id),
        Value::str(&row.tool_name),
        Value::U64(row.schema_hash),
    ];
    no_marker(&mut cells);
    cells.extend([
        Value::time(row.first_seen),
        Value::time(row.last_seen),
        Value::Array(
            row.capture_sources
                .iter()
                .map(|source| Value::U8(source.value()))
                .collect(),
        ),
    ]);
    cells
}

/// The partial one span contributes to its session's day (contract 3.10), or `None` for a span without a
/// session. `trace_count` counts root spans only.
pub fn session_rollup(span: &SpanRecord) -> Option<SessionRollupRow> {
    let c = &span.core;
    let counted = counted(c);
    (!c.session_id.is_empty()).then(|| SessionRollupRow {
        project_id: c.project_id,
        day: c.start_time.date(),
        session_id: c.session_id.clone(),
        first_seen: c.start_time,
        last_seen: c.end_time,
        trace_count: u64::from(c.is_root),
        span_count: 1,
        error_count: u64::from(is_error(c)),
        input_tokens: counted.input_tokens,
        output_tokens: counted.output_tokens,
        total_tokens: counted.total_tokens,
        total_cost_nusd: counted.total_cost_nusd,
        user_ids: if c.user_id.is_empty() {
            Vec::new()
        } else {
            vec![c.user_id.clone()]
        },
    })
}

/// The `session_rollup` data row of a partial.
pub fn session_rollup_row(row: &SessionRollupRow) -> Vec<Value> {
    let mut cells = vec![
        uuid(row.project_id.uuid()),
        date(row.day.start()),
        Value::str(&row.session_id),
    ];
    no_marker(&mut cells);
    cells.extend([
        Value::time(row.first_seen),
        Value::time(row.last_seen),
        Value::U64(row.trace_count),
        Value::U64(row.span_count),
        Value::U64(row.error_count),
        Value::U64(row.input_tokens),
        Value::U64(row.output_tokens),
        Value::U64(row.total_tokens),
        Value::I64(row.total_cost_nusd.nanos()),
        Value::strs(&row.user_ids),
    ]);
    cells
}

/// The partial one span contributes to its user's day (contract 3.10), or `None` for a span without a user.
/// `trace_count` counts root spans only.
pub fn user_rollup(span: &SpanRecord) -> Option<UserRollupRow> {
    let c = &span.core;
    let counted = counted(c);
    (!c.user_id.is_empty()).then(|| UserRollupRow {
        project_id: c.project_id,
        day: c.start_time.date(),
        user_id: c.user_id.clone(),
        first_seen: c.start_time,
        last_seen: c.end_time,
        trace_count: u64::from(c.is_root),
        span_count: 1,
        error_count: u64::from(is_error(c)),
        input_tokens: counted.input_tokens,
        output_tokens: counted.output_tokens,
        total_tokens: counted.total_tokens,
        total_cost_nusd: counted.total_cost_nusd,
    })
}

/// The `user_rollup` data row of a partial.
pub fn user_rollup_row(row: &UserRollupRow) -> Vec<Value> {
    let mut cells = vec![
        uuid(row.project_id.uuid()),
        date(row.day.start()),
        Value::str(&row.user_id),
    ];
    no_marker(&mut cells);
    cells.extend([
        Value::time(row.first_seen),
        Value::time(row.last_seen),
        Value::U64(row.trace_count),
        Value::U64(row.span_count),
        Value::U64(row.error_count),
        Value::U64(row.input_tokens),
        Value::U64(row.output_tokens),
        Value::U64(row.total_tokens),
        Value::I64(row.total_cost_nusd.nanos()),
    ]);
    cells
}

/// The partial one span contributes to the metrics of its minute (contract 3.10). `ttft_hist` takes
/// `completion_start_time - start_time` of generations that carry one.
pub fn span_metrics(span: &SpanRecord) -> SpanMetricsRow {
    let c = &span.core;
    let counted = counted(c);
    let mut dur_hist = Histogram::new();
    dur_hist.record(c.duration_us());
    let mut ttft_hist = Histogram::new();
    if let Some(first) = c
        .completion_start_time
        .filter(|_| c.observation_type == ObservationType::Generation)
    {
        let micros = first.micros().saturating_sub(c.start_time.micros());
        ttft_hist.record(u64::try_from(micros).unwrap_or(0));
    }
    SpanMetricsRow {
        project_id: c.project_id,
        minute: Timestamp::from_micros(i64::from(c.start_minute()) * 1_000_000),
        environment: c.environment.clone(),
        observation_type: c.observation_type,
        model_response: c.model_response.clone(),
        level: c.level,
        release: c.release.clone(),
        count: 1,
        error_count: u64::from(is_error(c)),
        input_tokens: counted.input_tokens,
        output_tokens: counted.output_tokens,
        total_tokens: counted.total_tokens,
        total_cost_nusd: counted.total_cost_nusd,
        dur_hist,
        ttft_hist,
    }
}

/// The `span_metrics` data row of a partial.
pub fn span_metrics_row(row: &SpanMetricsRow) -> Vec<Value> {
    let mut cells = vec![
        uuid(row.project_id.uuid()),
        Value::U32(row.minute.start_minute()),
        Value::str(&row.environment),
        en(row.observation_type.value()),
        Value::str(&row.model_response),
        en(row.level.value()),
        Value::str(&row.release),
    ];
    no_marker(&mut cells);
    cells.extend([
        Value::U64(row.count),
        Value::U64(row.error_count),
        Value::U64(row.input_tokens),
        Value::U64(row.output_tokens),
        Value::U64(row.total_tokens),
        Value::I64(row.total_cost_nusd.nanos()),
        histogram(&row.dur_hist),
        histogram(&row.ttft_hist),
    ]);
    cells
}

/// The row of a fact in its fact table (contract 6.4): the shared columns, the body columns of the table, then
/// the ingestion columns.
pub fn fact_row(fact: &Fact) -> Vec<Value> {
    let e = &fact.envelope;
    let mut cells = vec![
        uuid(e.project_id.uuid()),
        Value::str(&e.key),
        date(e.target_time),
        Value::bytes(e.trace_id.as_bytes()),
        Value::bytes(e.span_id.as_bytes()),
        Value::str(&e.session_id),
        Value::U64(e.version),
        uuid(e.fact_id.uuid()),
        en(e.target_kind.value()),
        Value::time(e.target_time),
        Value::flag(e.retracted),
        en(e.author_kind.value()),
        uuid(e.author_id),
        Value::time(e.created_at),
    ];
    match &fact.body {
        FactBody::Score(score) => cells.extend([
            en(score.data_type.value()),
            Value::F64(score.value_num),
            Value::str(&score.value_str),
            Value::str(&score.comment),
            en(score.source.value()),
            Value::bytes(&score.metadata),
            uuid(score.config_id.unwrap_or(Uuid::NIL)),
        ]),
        FactBody::Tag(tag) => cells.push(en(tag.op.value())),
        FactBody::Flag(flag) => cells.push(Value::flag(flag.value)),
        FactBody::Correction(correction) => cells.extend([
            Value::bytes(&correction.corrected),
            Value::str(&correction.note),
        ]),
    }
    ingestion(&mut cells, &e.ingestion);
    cells
}

/// One row with the (table, partition) pair it routes to.
#[derive(Clone, PartialEq, Debug)]
pub struct RoutedRow {
    /// The table.
    pub table: TableId,
    /// The partition, from `crucible_schema::route` over the row's time column.
    pub partition: PartitionId,
    /// The row.
    pub row: Row,
}

/// Every row the spans of one frame yield, after stage B: one `span_core` and one `span_payload` row per span, a
/// `trace_heads` row per root span and per provisional head, one `trace_stats` and one `span_metrics` partial
/// per span, and a `session_rollup` and a `user_rollup` partial per span that names a session or a user. Every
/// row of a span routes by the span's `start_time`, read verbatim (contract 5.2).
///
/// Then the rows of the frame's MCP operations: one `mcp_ops` row and one `mcp_tool_rollup` partial per
/// operation, one `mcp_tool_rollup_hourly` partial per key and hour of the frame's operations, an `mcp_content`
/// row when content was captured and an `mcp_deploys` partial for a server-side operation, routed by the
/// operation's `start_time`, which is its span's (the hourly partial by the start of its hour); and one `mcp_tool_inventory` partial
/// per tool inventory entry, routed by the reporting span's `start_time`.
pub fn derive(
    frame: &NormalizedFrame,
    admission: &Admission,
    granularity: Granularity,
) -> Result<Vec<RoutedRow>, OutOfRange> {
    derive_rows(frame, admission, granularity, Defects::default())
}

/// The key of an hourly partial in [`derive_rows`]: the ORDER BY columns of `mcp_tool_rollup_hourly` before the
/// marker columns, with the operation's strings borrowed and the enums as their stored values.
type HourKey<'a> = (
    crucible_records::ProjectId,
    u8,
    Timestamp,
    &'a str,
    &'a str,
    u8,
    u8,
    &'a str,
    &'a str,
    &'a str,
    u8,
    bool,
);

/// The seeded defects of [`derive_rows`]; none outside the simulation suite.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Defects {
    /// Count the usage of aggregate spans: the sums then take every span, while `span_core` keeps the flag.
    pub(crate) count_aggregates: bool,
    /// Give every `mcp_tool_rollup` partial one capture source, so that rows of different sources are summed.
    pub(crate) sum_capture_sources: bool,
}

/// [`derive`], with the seeded defects of `defects`.
pub(crate) fn derive_rows(
    frame: &NormalizedFrame,
    admission: &Admission,
    granularity: Granularity,
    defects: Defects,
) -> Result<Vec<RoutedRow>, OutOfRange> {
    let count_aggregates = defects.count_aggregates;
    let mut out = Vec::with_capacity(frame.spans.len() * 7 + frame.mcp_ops.len() * 5);
    for (index, span) in frame.spans.iter().enumerate() {
        let ordinal = u32::try_from(index).unwrap_or(u32::MAX);
        let start = span.core.start_time;
        let mut push = |table: TableId, cells: Vec<Value>| -> Result<(), OutOfRange> {
            out.push(RoutedRow {
                table,
                partition: route(table, granularity, start)?,
                row: Row {
                    cells,
                    lsn: frame.lsn,
                    ordinal,
                },
            });
            Ok(())
        };
        push(TableId::SpanCore, span_core_row(span))?;
        push(TableId::SpanPayload, span_payload_row(span))?;
        let unflagged;
        let span = if count_aggregates && span.core.usage_aggregate {
            let mut copy = span.clone();
            copy.core.usage_aggregate = false;
            unflagged = copy;
            &unflagged
        } else {
            span
        };
        let head_kind = if span.core.is_root {
            Some(HeadKind::Root)
        } else if admission.provisional_heads.contains(&index) {
            Some(HeadKind::Provisional)
        } else {
            None
        };
        if let Some(kind) = head_kind {
            push(TableId::TraceHeads, trace_head_row(&trace_head(span, kind)))?;
        }
        push(
            TableId::TraceStats,
            trace_stats_row(&trace_stats(span, head_kind.is_some())),
        )?;
        if let Some(row) = session_rollup(span) {
            push(TableId::SessionRollup, session_rollup_row(&row))?;
        }
        if let Some(row) = user_rollup(span) {
            push(TableId::UserRollup, user_rollup_row(&row))?;
        }
        push(TableId::SpanMetrics, span_metrics_row(&span_metrics(span)))?;
    }
    let mut push = |table: TableId, at: Timestamp, ordinal: u32, cells: Vec<Value>| {
        out.push(RoutedRow {
            table,
            partition: route(table, granularity, at)?,
            row: Row {
                cells,
                lsn: frame.lsn,
                ordinal,
            },
        });
        Ok::<(), OutOfRange>(())
    };
    // The hourly partials, combined by key as the operations come: an hour holds many operations of one key, so
    // the frame gives the hourly pair one row per key (at its first operation's ordinal), where it gives the
    // minute pair one per operation. The block combines them further across the frames of a batch.
    let mut hours: BTreeMap<HourKey<'_>, (McpToolRollupRow, u32)> = BTreeMap::new();
    for op in &frame.mcp_ops {
        let (record, at) = (&op.record, op.record.start_time);
        push(TableId::McpOps, at, op.span, mcp_ops_row(record))?;
        if record.content_captured {
            push(TableId::McpContent, at, op.span, mcp_content_row(record))?;
        }
        let mut rollup = mcp_tool_rollup(record);
        if defects.sum_capture_sources {
            rollup.capture_source = CaptureSource::Sdk;
        }
        push(
            TableId::McpToolRollup,
            at,
            op.span,
            mcp_tool_rollup_row(&rollup),
        )?;
        let key = (
            record.project_id,
            record.side.value(),
            at.floor_hour(),
            &*record.server_name,
            &*record.deploy_id,
            rollup.capture_source.value(),
            record.era.value(),
            &*record.method,
            &*record.target_name,
            &*record.client_name,
            record.error_class.value(),
            record.caller_fault,
        );
        match hours.entry(key) {
            Entry::Occupied(mut entry) => entry.get_mut().0.merge(&rollup),
            Entry::Vacant(entry) => {
                rollup.minute = rollup.minute.floor_hour();
                entry.insert((rollup, op.span));
            }
        }
        if let Some(deploy) = mcp_deploy(record) {
            push(TableId::McpDeploys, at, op.span, mcp_deploys_row(&deploy))?;
        }
    }
    for (partial, ordinal) in hours.into_values() {
        // The partial's minute is the start of its hour.
        let row = mcp_tool_rollup_row(&partial);
        push(TableId::McpToolRollupHourly, partial.minute, ordinal, row)?;
    }
    for (index, entry) in frame.inventory.iter().enumerate() {
        let ordinal = u32::try_from(index).unwrap_or(u32::MAX);
        let row = mcp_tool_inventory(&entry.record);
        push(
            TableId::McpToolInventory,
            entry.record.first_seen,
            ordinal,
            mcp_tool_inventory_row(&row),
        )?;
    }
    Ok(out)
}

/// The rows of the facts of one `FactBatch` frame: one row per fact in its fact table, routed by the fact's
/// `target_time`, read verbatim (contract 5.2).
pub fn derive_facts(
    facts: &[Fact],
    granularity: Granularity,
) -> Result<Vec<RoutedRow>, OutOfRange> {
    facts
        .iter()
        .enumerate()
        .map(|(index, fact)| {
            let table = fact.body.table();
            Ok(RoutedRow {
                table,
                partition: route(table, granularity, fact.envelope.target_time)?,
                row: Row {
                    cells: fact_row(fact),
                    lsn: fact.envelope.ingestion.lsn,
                    ordinal: u32::try_from(index).unwrap_or(u32::MAX),
                },
            })
        })
        .collect()
}
