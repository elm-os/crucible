//! From the service's values to the wire DTOs (contract 1.3): times as RFC 3339 strings, ids as hex, costs in
//! USD and counts saturating at 2^53 - 1.

use crucible_api_types as wire;
use crucible_api_types::{Saturation, usd_from_nanos};
use crucible_query as query;
use crucible_records::{SpanId, Timestamp};

use crate::cursor::Cursors;

fn micros_between(start: Timestamp, end: Timestamp) -> u64 {
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

fn list_item(item: &query::TraceListItem) -> wire::TraceListItem {
    let (head, summary) = (&item.head, &item.summary);
    let mut s = Saturation::default();
    wire::TraceListItem {
        trace_id: head.trace_id.to_string(),
        name: head.name.to_string(),
        trace_name: summary.trace_name.to_string(),
        head_kind: head.head_kind.into(),
        start_time: head.start_time.to_string(),
        duration_us: s.u64(micros_between(summary.min_start, summary.max_end)),
        status_code: head.status_code.into(),
        level: head.level.into(),
        span_count: s.u64(summary.span_count),
        error_count: s.u64(summary.error_count),
        total_cost_usd: usd_from_nanos(summary.total_cost_nusd.nanos()),
        input_tokens: s.u64(summary.input_tokens),
        output_tokens: s.u64(summary.output_tokens),
        total_tokens: s.u64(summary.total_tokens),
        user_id: summary.user_id.to_string(),
        session_id: summary.session_id.to_string(),
        environment: head.environment.to_string(),
        release: head.release.to_string(),
        tags: head.tags.iter().map(ToString::to_string).collect(),
        saturated: s.saturated(),
    }
}

pub(crate) fn trace_list(
    page: &query::TraceListPage,
    cursors: &Cursors<'_>,
) -> wire::TraceListPage {
    wire::TraceListPage {
        data: page.data.iter().map(list_item).collect(),
        next_cursor: page.next_cursor.as_ref().map(|c| cursors.seal_page(c)),
        has_more: page.has_more,
        head_cursor: page.head_cursor.as_ref().map(|c| cursors.seal_head(c)),
        new_count: None,
        meta: meta(&page.meta),
    }
}

pub(crate) fn trace_tail(tail: &query::TraceTail, cursors: &Cursors<'_>) -> wire::TraceListPage {
    wire::TraceListPage {
        data: tail.data.iter().map(list_item).collect(),
        next_cursor: None,
        has_more: tail.has_more,
        head_cursor: Some(cursors.seal_head(&tail.head_cursor)),
        new_count: Some(tail.new_count),
        meta: meta(&tail.meta),
    }
}

fn span(span: &query::TraceSpan) -> wire::Span {
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

pub(crate) fn trace_detail(detail: &query::TraceDetail) -> wire::TraceDetail {
    let mut s = Saturation::default();
    wire::TraceDetail {
        trace_id: detail.trace_id.to_string(),
        span_count: s.u64(detail.span_count),
        paged: detail.paged,
        spans: detail.spans.iter().map(span).collect(),
        meta: meta(&detail.meta),
        saturated: s.saturated(),
    }
}

pub(crate) fn span_page(page: &query::SpanChildrenPage, cursors: &Cursors<'_>) -> wire::SpanPage {
    wire::SpanPage {
        data: page.data.iter().map(span).collect(),
        next_cursor: page.next_cursor.as_ref().map(|c| cursors.seal_child(c)),
        has_more: page.has_more,
        meta: meta(&page.meta),
    }
}

fn range(range: Option<query::MinuteRange>) -> wire::TimeRange {
    wire::TimeRange {
        first: range.map(|r| r.first.to_string()),
        last: range.map(|r| r.last.to_string()),
    }
}

pub(crate) fn data_bounds(bounds: &query::DataBounds, max_window_days: u32) -> wire::DataBounds {
    wire::DataBounds {
        spans: range(bounds.spans),
        mcp: wire::McpBounds {
            client: range(bounds.mcp_client),
            server: range(bounds.mcp_server),
        },
        retention_horizon: bounds.retention_horizon.map(|t| t.to_string()),
        max_window_days,
        server_time: bounds.meta.as_of.to_string(),
        meta: meta(&bounds.meta),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crucible_api_types::MAX_SAFE_INTEGER;
    use crucible_query::{SpanSource, TraceSpan};
    use crucible_recent::{SpanSummary, WalLocator};
    use crucible_records::{Level, NanoUsd, ObservationType, StatusCode, TraceId};

    use super::*;

    fn span(tokens: u64, start: i64, end: i64) -> TraceSpan {
        let text = || Arc::from("");
        TraceSpan {
            summary: SpanSummary {
                trace_id: TraceId::from_bytes([1; 16]),
                span_id: SpanId::from_bytes([2; 8]),
                parent_span_id: SpanId::ZERO,
                is_root: true,
                start_time: Timestamp::from_micros(start),
                end_time: Timestamp::from_micros(end),
                observation_type: ObservationType::Span,
                level: Level::Default,
                status_code: StatusCode::Unset,
                name: text(),
                trace_name: text(),
                user_id: text(),
                session_id: text(),
                model: text(),
                total_cost_nusd: NanoUsd::MAX,
                input_tokens: tokens,
                output_tokens: 0,
                total_tokens: tokens,
            },
            source: SpanSource::Recent(WalLocator {
                segment: 1,
                offset: 0,
                ordinal: 0,
            }),
            child_count: Some(tokens),
        }
    }

    #[test]
    fn counts_above_two_to_the_53_saturate_and_their_object_says_so() {
        let detail = query::TraceDetail {
            trace_id: TraceId::from_bytes([1; 16]),
            span_count: u64::MAX,
            paged: true,
            spans: vec![span(u64::MAX, 10, 20), span(3, 20, 10)],
            meta: query::Meta {
                as_of: Timestamp::EPOCH,
                partial: false,
                partial_reason: None,
                consistency: query::Consistency::Strong,
            },
        };
        let wire = trace_detail(&detail);
        assert!(wire.saturated);
        assert_eq!(wire.span_count.get(), MAX_SAFE_INTEGER);
        let (big, small) = (&wire.spans[0], &wire.spans[1]);
        assert!(big.saturated);
        assert_eq!(big.input_tokens.get(), MAX_SAFE_INTEGER);
        assert_eq!(big.child_count.map(|c| c.get()), Some(MAX_SAFE_INTEGER));
        assert_eq!(big.output_tokens.get(), 0);
        assert!(!small.saturated);
        // An end before the start is a zero duration, not a wrapped one.
        assert_eq!(small.duration_us.get(), 0);
        assert_eq!(big.total_cost_usd, usd_from_nanos(i64::MAX));
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["span_count"], MAX_SAFE_INTEGER);
        assert_eq!(json["saturated"], true);
        assert_eq!(json["spans"][0]["parent_span_id"], serde_json::Value::Null);
    }
}
