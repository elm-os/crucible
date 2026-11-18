//! From the service's values to the wire DTOs (contract 1.3): times as RFC 3339 strings, ids as hex, costs in
//! USD and counts saturating at 2^53 - 1.
//! The conversions here are the ones the route groups share; each group converts its own responses.

use crucible_api_types as wire;
use crucible_api_types::{Saturation, usd_from_nanos};
use crucible_query as query;
use crucible_records::{SpanId, Timestamp};

pub(crate) fn micros_between(start: Timestamp, end: Timestamp) -> u64 {
    u64::try_from(end.micros().saturating_sub(start.micros())).unwrap_or(0)
}

pub(crate) fn meta(meta: &query::Meta) -> wire::Meta {
    wire::Meta {
        as_of: meta.as_of.to_string(),
        partial: meta.partial,
        partial_reason: meta.partial_reason.map(|reason| match reason {
            query::PartialReason::StoreUnavailable => wire::PartialReason::StoreUnavailable,
            query::PartialReason::Deadline => wire::PartialReason::Deadline,
            query::PartialReason::Quarantined => wire::PartialReason::Quarantined,
        }),
        consistency: match meta.consistency {
            query::Consistency::Strong => wire::Consistency::Strong,
            query::Consistency::Lagging => wire::Consistency::Lagging,
        },
    }
}

pub(crate) fn span(span: &query::TraceSpan) -> wire::Span {
    let summary = &span.summary;
    let mut s = Saturation::default();
    wire::Span {
        span_id: summary.span_id.to_string(),
        parent_span_id: (summary.parent_span_id != SpanId::ZERO)
            .then(|| summary.parent_span_id.to_string()),
        name: summary.name.to_string(),
        observation_type: summary.observation_type.into(),
        start_time: summary.start_time.to_string(),
        end_time: summary.end_time.to_string(),
        duration_us: s.u64(micros_between(summary.start_time, summary.end_time)),
        status_code: summary.status_code.into(),
        level: summary.level.into(),
        model: summary.model.to_string(),
        total_cost_usd: usd_from_nanos(summary.total_cost_nusd.nanos()),
        input_tokens: s.u64(summary.input_tokens),
        output_tokens: s.u64(summary.output_tokens),
        total_tokens: s.u64(summary.total_tokens),
        child_count: span.child_count.map(|count| s.u64(count)),
        saturated: s.saturated(),
    }
}
