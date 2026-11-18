//! The rows a span yields in the milestone-1 tables (contract sections 3.9 and 6.4): `span_core`, `span_payload`,
//! `trace_heads` and `trace_stats`. Each function lists the cells in `TableDef` column order; a test checks every
//! row against its table's column types.

use crucible_normalize::{Admission, NormalizedFrame};
use crucible_records::{
    HeadKind, Ingestion, Level, McpSide, NO_HEAD_START, ObservationType, PartitionId, Pick,
    SpanRecord, StatusCode, TableId, Timestamp, TraceHead, TraceStatsRow, Uuid,
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

/// The partial one span contributes to its trace's stats (contract 3.9). A span counts as an error when its status
/// or its level is `error`.
pub fn trace_stats(span: &SpanRecord, is_head: bool) -> TraceStatsRow {
    let c = &span.core;
    let pick = |value: &str| Pick::new(c.is_root, c.end_time, c.span_id, value);
    let is_error = c.status_code == StatusCode::Error || c.level == Level::Error;
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
        total_cost_nusd: c.total_cost_nusd,
        input_tokens: c.input_tokens,
        output_tokens: c.output_tokens,
        total_tokens: c.total_tokens,
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
/// `trace_heads` row per root span and per provisional head, and one `trace_stats` partial per span. Every row
/// of a span routes by the span's `start_time`, read verbatim (contract 5.2).
pub fn derive(
    frame: &NormalizedFrame,
    admission: &Admission,
    granularity: Granularity,
) -> Result<Vec<RoutedRow>, OutOfRange> {
    let mut out = Vec::with_capacity(frame.spans.len() * 4);
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
    }
    Ok(out)
}
