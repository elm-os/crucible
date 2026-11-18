//! Stage A (contract 3.2): one frame to its records, purely.

use bytes::Bytes;
use crucible_compat::Confidence;
use crucible_otlp::{SpanView, TraceRequest};
use crucible_records::{
    CostSource, Ingestion, IoShape, Level, Lsn, McpSide, NanoUsd, ObservationType, SourceFormat,
    SpanCore, SpanId, SpanPayload, SpanRecord, StatusCode, TracePublic, Truncation, caps,
    truncate_str,
};

use crate::mapper::{Hints, MapCtx, NATIVE, SpanDraft, SpanMapper};
use crate::{
    FrameSummary, IngestFrame, NormCtx, NormalizeError, NormalizedFrame, cost, digest, fill, json,
};

/// A `start_time` further than this ahead of `received_at` is flagged `clock_skew` (contract 1.2).
const MAX_CLOCK_LEAD_MICROS: i64 = 24 * 60 * 60 * 1_000_000;

/// The value of an absent `environment` (contract 3.1).
const DEFAULT_ENVIRONMENT: &str = "default";

/// Normalizes one frame with the production mappers ([`NATIVE`]).
pub fn normalize(
    frame: &IngestFrame<'_>,
    ctx: &NormCtx<'_>,
) -> Result<NormalizedFrame, NormalizeError> {
    normalize_with(&[&NATIVE], frame, ctx)
}

/// Normalizes one frame with the given mappers.
///
/// 1. Re-scan the OTLP body; a body that does not scan fails the whole frame.
/// 2. Skip each span the envelope's bitmap marks, and any span with a missing id.
/// 3. Build each draft: ids, kind, name, times, status, scope, links, trace state and the envelope and ingestion
///    values; then the chosen mapper fills the rest from the attributes.
/// 4. Forward-fill trace-level attributes within the request (contract 3.3).
/// 5. Finish each record: level, the default environment, usage and cost, payload caps, then the row digest.
///
/// Times: `start_time` and `end_time` are clamped into the valid range, `end_time` is raised to at least
/// `start_time`, and `clock_skew` is set when a time was clamped or `start_time` is more than 24 h ahead of
/// `received_at` (contract 1.2).
pub fn normalize_with(
    mappers: &[&dyn SpanMapper],
    frame: &IngestFrame<'_>,
    ctx: &NormCtx<'_>,
) -> Result<NormalizedFrame, NormalizeError> {
    let request = TraceRequest::scan(frame.otlp, None)?;
    let map_ctx = MapCtx {
        frame,
        normalizer_version: ctx.normalizer_version,
    };
    let mut summary = FrameSummary {
        spans: request.span_count(),
        ..FrameSummary::default()
    };
    let mut drafts = Vec::with_capacity(request.span_count() as usize);
    for span in request.spans() {
        if frame.is_rejected(span.ordinal()) {
            summary.rejected_before_ack += 1;
            continue;
        }
        if span.rejection().is_some() {
            summary.missing_ids += 1;
            continue;
        }
        let mut draft = draft(frame, ctx, &span);
        if let Some(mapper) = choose(mappers, frame.compat_dialect, &span) {
            mapper.map(&map_ctx, &span, &mut draft);
            let core = &mut draft.record.core;
            core.compat_dialect = mapper.dialect();
            core.source_format = if mapper.dialect() == 0 {
                SourceFormat::Otel
            } else {
                SourceFormat::Compat
            };
        }
        drafts.push(draft);
    }
    fill::forward_fill(&mut drafts);
    let spans = drafts.into_iter().map(|d| finish(d, ctx)).collect();
    Ok(NormalizedFrame {
        lsn: frame.lsn,
        spans,
        summary,
    })
}

/// The mapper of a span (contract 10.2): the request hint's dialect when a mapper for it exists, else the mapper
/// with the highest detection confidence (ties to the lowest dialect id), else dialect 0's. A hint naming a
/// dialect without a mapper in this build is ignored, so a record always names the dialect that mapped it.
fn choose<'m>(
    mappers: &[&'m dyn SpanMapper],
    hint: u8,
    span: &SpanView<'_>,
) -> Option<&'m dyn SpanMapper> {
    if hint != 0
        && let Some(&mapper) = mappers.iter().find(|m| m.dialect() == hint)
    {
        return Some(mapper);
    }
    let hints = Hints { dialect: hint };
    let mut best: Option<(Confidence, u8, &'m dyn SpanMapper)> = None;
    for &mapper in mappers {
        let Some(confidence) = mapper.detect(span.resource(), span.scope(), span, &hints) else {
            continue;
        };
        let id = mapper.dialect();
        if best.is_none_or(|(c, best_id, _)| confidence > c || (confidence == c && id < best_id)) {
            best = Some((confidence, id, mapper));
        }
    }
    best.map(|(.., mapper)| mapper)
        .or_else(|| mappers.iter().find(|m| m.dialect() == 0).copied())
}

fn text(s: &str, cap: usize) -> Box<str> {
    truncate_str(s, cap).into()
}

/// The draft of a span before any mapper runs.
fn draft(frame: &IngestFrame<'_>, ctx: &NormCtx<'_>, span: &SpanView<'_>) -> SpanDraft {
    let (start_time, start_clamped) = span.start_time().clamp_valid();
    let (end_time, end_clamped) = span.end_time().clamp_valid();
    let ahead = start_time.micros()
        > frame
            .received_at
            .micros()
            .saturating_add(MAX_CLOCK_LEAD_MICROS);
    let parent = span.parent_span_id();
    let (status, scope) = (span.status(), span.scope());
    let core = SpanCore {
        project_id: frame.project_id,
        trace_id: span.trace_id(),
        span_id: span.span_id(),
        parent_span_id: parent.unwrap_or(SpanId::ZERO),
        is_root: parent.is_none(),
        is_app_root: false,
        kind: span.kind(),
        name: text(span.name(), caps::SPAN_NAME_BYTES),
        start_time,
        end_time: end_time.max(start_time),
        completion_start_time: None,
        status_code: status.code(),
        status_message: text(status.message(), caps::STATUS_MESSAGE_BYTES),
        service_name: "".into(),
        service_version: "".into(),
        scope_name: text(scope.name(), caps::SERVICE_BYTES),
        scope_version: text(scope.version(), caps::SERVICE_BYTES),
        telemetry_sdk_name: "".into(),
        telemetry_sdk_language: "".into(),
        telemetry_sdk_version: "".into(),
        sdk_name: text(frame.sdk_name, caps::SDK_BYTES),
        sdk_version: text(frame.sdk_version, caps::SDK_BYTES),
        observation_type: ObservationType::Span,
        level: Level::Default,
        environment: "".into(),
        release: "".into(),
        version: "".into(),
        trace_name: "".into(),
        user_id: "".into(),
        session_id: "".into(),
        trace_tags: Vec::new(),
        trace_public: TracePublic::Unset,
        tags: Vec::new(),
        model_requested: "".into(),
        model_response: "".into(),
        provider: "".into(),
        finish_reasons: Vec::new(),
        usage: Vec::new(),
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        total_cost_nusd: NanoUsd::ZERO,
        cost_source: CostSource::None,
        price_model_id: None,
        price_tier_id: None,
        prompt_name: "".into(),
        prompt_version: "".into(),
        tool_names: Vec::new(),
        tool_call_count: 0,
        mcp_side: McpSide::None,
        io_shape: IoShape::Raw,
        input_preview: "".into(),
        output_preview: "".into(),
        input_bytes: 0,
        output_bytes: 0,
        input_truncation: Truncation::None,
        output_truncation: Truncation::None,
        input_sha256: None,
        output_sha256: None,
        source_format: SourceFormat::Otel,
        compat_dialect: 0,
        normalizer_version: ctx.normalizer_version,
        clock_skew: start_clamped || end_clamped || ahead,
        ingestion: Ingestion {
            wal_epoch: frame.wal_epoch,
            batch_seq: Lsn::ZERO,
            lsn: frame.lsn,
            received_at: frame.received_at,
        },
        api_key_id: frame.api_key_id,
        row_digest: 0,
        event_bytes: u32::try_from(span.encoded_len()).unwrap_or(u32::MAX),
    };
    let payload = SpanPayload {
        links: json::links(span.links()),
        trace_state: Bytes::copy_from_slice(span.trace_state().as_bytes()),
        ..SpanPayload::default()
    };
    SpanDraft {
        record: SpanRecord { core, payload },
        level: None,
    }
}

/// The finished record of a filled draft.
fn finish(draft: SpanDraft, ctx: &NormCtx<'_>) -> SpanRecord {
    let SpanDraft { mut record, level } = draft;
    let core = &mut record.core;
    core.level = level.unwrap_or(if core.status_code == StatusCode::Error {
        Level::Error
    } else {
        Level::Default
    });
    if core.environment.is_empty() {
        core.environment = DEFAULT_ENVIRONMENT.into();
    }
    cost::apply(&mut record, &ctx.prices);
    record.enforce_payload_caps(&ctx.caps);
    record.core.row_digest = digest::row_digest(&record);
    record
}
