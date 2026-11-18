//! From the service's values to the wire DTOs (contract 1.3): times as RFC 3339 strings, ids as hex, costs in
//! USD and counts saturating at 2^53 - 1.

use crucible_api_types as wire;
use crucible_api_types::{Saturation, usd_from_nanos};
use crucible_query as query;
use crucible_records::{SpanId, Timestamp};

use crate::cursor;

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

pub(crate) fn trace_list(page: &query::TraceListPage) -> wire::TraceListPage {
    wire::TraceListPage {
        data: page.data.iter().map(list_item).collect(),
        next_cursor: page.next_cursor.as_ref().map(cursor::encode_page),
        has_more: page.has_more,
        head_cursor: page.head_cursor.as_ref().map(cursor::encode_head),
        new_count: None,
        meta: meta(&page.meta),
    }
}

pub(crate) fn trace_tail(tail: &query::TraceTail) -> wire::TraceListPage {
    wire::TraceListPage {
        data: tail.data.iter().map(list_item).collect(),
        next_cursor: None,
        has_more: tail.has_more,
        head_cursor: Some(cursor::encode_head(&tail.head_cursor)),
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

pub(crate) fn span_page(page: &query::SpanChildrenPage) -> wire::SpanPage {
    wire::SpanPage {
        data: page.data.iter().map(span).collect(),
        next_cursor: page.next_cursor.as_ref().map(cursor::encode_child),
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
