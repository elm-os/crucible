//! The rows a span yields (contract sections 3.9, 3.10 and 6.4): `span_core`, `span_payload`, `trace_heads`,
//! `trace_stats` and the rollup partials `session_rollup`, `user_rollup` and `span_metrics`; and the row a fact
//! yields in its fact table (contract 3.8). Each function lists the cells in `TableDef` column order; a test
//! checks every row against its table's column types.

use crucible_normalize::{Admission, NormalizedFrame};
use crucible_records::{
    Fact, FactBody, HeadKind, Histogram, Ingestion, Level, McpSide, NO_HEAD_START, NanoUsd,
    ObservationType, PartitionId, Pick, SessionRollupRow, SpanCore, SpanMetricsRow, SpanRecord,
    StatusCode, TableId, Timestamp, TraceHead, TraceStatsRow, UserRollupRow, Uuid,
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
    cells.push(Value::flag(c.usage_aggregate));
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

/// The tokens and cost a span adds to the trace, session, user and metrics sums (contracts 3.9 and 3.10): its own
/// when it is a generation or an embedding that does not repeat the usage of a descendant
/// ([`SpanCore::counts_usage`]), else nothing. Every model call is so counted once, on the span that names the
/// model it is priced with.
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
/// row of a span routes by the span's `start_time`, read verbatim (contract 5.2). The MCP tables join in
/// milestone 3.
pub fn derive(
    frame: &NormalizedFrame,
    admission: &Admission,
    granularity: Granularity,
) -> Result<Vec<RoutedRow>, OutOfRange> {
    derive_rows(frame, admission, granularity, false)
}

/// [`derive`], with the defect of the mutant that counts the usage of aggregate spans when `count_aggregates` is
/// set: the sums then take every generation and embedding, while `span_core` keeps the flag.
pub(crate) fn derive_rows(
    frame: &NormalizedFrame,
    admission: &Admission,
    granularity: Granularity,
    count_aggregates: bool,
) -> Result<Vec<RoutedRow>, OutOfRange> {
    let mut out = Vec::with_capacity(frame.spans.len() * 7);
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
