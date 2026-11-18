//! `get_span_children`: one page of the direct children of a span (contract 8.8), how an agent reads a large
//! trace below the levels that `get_trace` returns.

use crucible_query::{ChildCursor, SpanChildrenQuery};
use crucible_records::{SpanId, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{Meta, SpanRow, fit, meta};
use crate::tools::{Call, Check, Tool};

/// The most children one call returns.
const MAX_ROWS: u32 = 200;
/// Children per call when `limit` is absent.
const DEFAULT_ROWS: u32 = 50;

pub(crate) struct GetSpanChildren;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The trace id, 32 hex characters.
    trace_id: String,
    /// The span whose direct children to list, 16 hex characters.
    parent_span_id: String,
    /// Children per page, from 1 to 200; 50 when absent.
    #[serde(default)]
    limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same `trace_id` and `parent_span_id`.
    #[serde(default)]
    cursor: Option<String>,
}

/// One page of children, in start order.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The children, each with `child_count`.
    data: Vec<SpanRow>,
    /// Pass it as `cursor`, with the same span, for the next page; null on the last page.
    next_cursor: Option<String>,
    /// Whether another page exists.
    has_more: bool,
    /// True when this page was cut to keep the result under 64 KiB; `next_cursor` continues after its last span.
    truncated: bool,
    /// How complete and how fresh the page is.
    meta: Meta,
}

impl Tool for GetSpanChildren {
    const NAME: &'static str = "get_span_children";
    const DESCRIPTION: &'static str = "List the direct children of one span of a trace, in start order: each \
        child's name, type, timing, status, model, tokens, cost, its own child count, and the first 1,024 bytes \
        of its input and output. At most 200 children and 64 KiB per call; pass `next_cursor` back as `cursor` \
        for more. Use it to walk a trace that get_trace returned in paged form.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        let trace_id = check.trace_id("trace_id", &args.trace_id);
        let parent = check.span_id("parent_span_id", &args.parent_span_id);
        let limit = check.limit(args.limit, DEFAULT_ROWS, MAX_ROWS);
        check.finish()?;
        let (Some(trace_id), Some(parent)) = (trace_id, parent) else {
            return Err(ToolError::validation(Vec::new()));
        };
        let cursor = args
            .cursor
            .map(|text| call.cursors.open_children(&text, trace_id, parent))
            .transpose()
            .map_err(ToolError::cursor)?;
        let query = SpanChildrenQuery {
            trace_id,
            parent,
            cursor,
            limit: Some(limit),
        };
        let page = call.service.span_children(&call.ctx, query).await;
        let page = page.map_err(|e| ToolError::query(&e, call.request_id))?;
        let rows: Vec<((Timestamp, SpanId), SpanRow)> = page
            .data
            .iter()
            .map(|span| {
                let key = (span.summary.start_time, span.summary.span_id);
                (key, SpanRow::new(span))
            })
            .collect();
        let next = |cursor: &ChildCursor| {
            let (_, trace_id, parent, last) = cursor.parts();
            call.cursors.seal_children(trace_id, parent, last)
        };
        Ok(fit(
            rows,
            |(_, row)| row.lighten(),
            |prefix, cut| Output {
                data: prefix.iter().map(|(_, row)| row.clone()).collect(),
                next_cursor: match prefix.last() {
                    Some((last, _)) if cut => {
                        Some(call.cursors.seal_children(trace_id, parent, *last))
                    }
                    _ => page.next_cursor.as_ref().map(next),
                },
                has_more: cut || page.has_more,
                truncated: cut,
                meta: meta(&page.meta),
            },
        ))
    }
}
