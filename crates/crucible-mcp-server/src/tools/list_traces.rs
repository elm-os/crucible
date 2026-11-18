//! `list_traces`: one page of the trace list (contract 8.7), newest first, with previews of each head span.

use crucible_plan::Pred;
use crucible_query::{HeadKey, PageCursor, TraceListQuery};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{Meta, TraceRow, fit, meta};
use crate::tools::{Call, Check, Tool, check_window};

/// The most traces one call returns.
const MAX_ROWS: u32 = 50;

pub(crate) struct ListTraces;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The start of the window, RFC 3339 (inclusive). Required: get_data_bounds gives the range with data.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// Traces per page, from 1 to 50; 50 when absent.
    #[serde(default)]
    limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same `from` and `to`.
    #[serde(default)]
    cursor: Option<String>,
}

/// One page of traces, newest first.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The traces whose head span started in the window.
    data: Vec<TraceRow>,
    /// Pass it as `cursor`, with the same `from` and `to`, for the next page; null on the last page.
    next_cursor: Option<String>,
    /// Whether another page exists.
    has_more: bool,
    /// True when this page was cut to keep the result under 64 KiB; `next_cursor` continues after its last trace.
    truncated: bool,
    /// How complete and how fresh the page is.
    meta: Meta,
}

impl Tool for ListTraces {
    const NAME: &'static str = "list_traces";
    const DESCRIPTION: &'static str = "List the project's traces whose head span (the root, or the earliest \
        span while the root has not arrived) started in the window [from, to), newest first: name, status, \
        duration, span and error counts, cost, tokens, user, session, environment, release and tags. At most 50 \
        traces and 64 KiB per call; pass `next_cursor` back as `cursor`, with the same window, for more. Read one \
        trace, with previews of its spans' inputs and outputs, with get_trace.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        let window = check.window(&args.from, &args.to);
        let limit = check.limit(args.limit, MAX_ROWS, MAX_ROWS);
        check.finish()?;
        let Some(window) = window else {
            return Err(ToolError::validation(Vec::new()));
        };
        check_window(window, call.settings.max_window_days)?;
        let cursor = args
            .cursor
            .map(|text| call.cursors.open_traces(&text, window))
            .transpose()
            .map_err(ToolError::cursor)?;
        let query = TraceListQuery {
            window,
            filter: Pred::all(),
            limit: Some(limit),
            cursor,
        };
        let page = call.service.trace_list(&call.ctx, query).await;
        let page = page.map_err(|e| ToolError::query(&e, call.request_id))?;
        let rows: Vec<(HeadKey, TraceRow)> = page
            .data
            .iter()
            .map(|item| (item.head.key(), TraceRow::new(item)))
            .collect();
        // A cut page resumes after its last trace, with the `as_of` that the first page fixed.
        let as_of = page.meta.as_of;
        let next = |cursor: &PageCursor| {
            let (_, window, as_of, last) = cursor.parts();
            call.cursors.seal_traces(window, as_of, last)
        };
        Ok(fit(
            rows,
            |(_, row)| row.lighten(),
            |prefix, cut| Output {
                data: prefix.iter().map(|(_, row)| row.clone()).collect(),
                next_cursor: match prefix.last() {
                    Some((last, _)) if cut => Some(call.cursors.seal_traces(window, as_of, *last)),
                    _ => page.next_cursor.as_ref().map(next),
                },
                has_more: cut || page.has_more,
                truncated: cut,
                meta: meta(&page.meta),
            },
        ))
    }
}
