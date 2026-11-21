//! `list_spans`: one page of the span list (contract 8.7), newest first, under the filter of the REST span list
//! (contract 12.5).

use crucible_query::{SpanListCursor, SpanListQuery};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::filters::{ListFilter, list_error};
use crate::shape::{Meta, SpanListRow, fit, meta};
use crate::tools::{Call, Check, Tool, check_window};
use crucible_api::FilterList;

/// The most spans one call returns.
const MAX_ROWS: u32 = 50;

pub(crate) struct ListSpans;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The start of the window, RFC 3339 (inclusive). Required: get_data_bounds gives the range with data.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// Keeps only the spans that match; each condition tests the span's own value.
    #[serde(default)]
    filter: Option<ListFilter>,
    /// Spans per page, from 1 to 50; 50 when absent.
    #[serde(default)]
    limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same `from`, `to` and `filter`.
    #[serde(default)]
    cursor: Option<String>,
}

/// One page of spans, newest first.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The spans that started in the window.
    data: Vec<SpanListRow>,
    /// Pass it as `cursor`, with the same `from`, `to` and `filter`, for the next page; null on the last page.
    next_cursor: Option<String>,
    /// Whether another page exists.
    has_more: bool,
    /// True when this page was cut to keep the result under 64 KiB; `next_cursor` continues after its last span.
    truncated: bool,
    /// How complete and how fresh the page is.
    meta: Meta,
}

impl Tool for ListSpans {
    const NAME: &'static str = "list_spans";
    const DESCRIPTION: &'static str = "List the project's spans that started in the window [from, to), newest \
        first, across traces: trace and span ids, name, observation type, status, level, duration, model, \
        environment, user, session, cost and tokens. `filter` narrows the list by environment, release, \
        version, user, session, level, observation type, tags, metadata or name; each condition tests the \
        span's own value. At most 50 spans and 64 KiB per call; pass `next_cursor` back as `cursor`, with the \
        same window and filter, for more. Read a span's trace with get_trace, and its full input or output with \
        get_span_payload.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        let window = check.window(&args.from, &args.to);
        let limit = check.limit(args.limit, MAX_ROWS, MAX_ROWS);
        let filter = ListFilter::parse(args.filter, &mut check, FilterList::Spans);
        check.finish()?;
        let Some(window) = window else {
            return Err(ToolError::validation(Vec::new()));
        };
        check_window(window, call.settings.max_window_days)?;
        let cursor = args
            .cursor
            .map(|text| call.cursors.open_spans(&text, window, &filter))
            .transpose()
            .map_err(ToolError::cursor)?;
        let query = SpanListQuery {
            window,
            filter: filter.clone(),
            limit: Some(limit),
            cursor,
        };
        let page = call.service.span_list(&call.ctx, query).await;
        let page = page.map_err(|e| list_error(&e, call.request_id))?;
        // A cut page resumes after its last span, with the `as_of` that the first page fixed.
        let as_of = page.meta.as_of;
        let next = |cursor: &SpanListCursor| {
            let (_, window, as_of, last) = cursor.parts();
            call.cursors.seal_spans(window, as_of, last, &filter)
        };
        Ok(fit(
            SpanListRow::rows(&page),
            |_| {},
            |prefix, cut| Output {
                data: prefix.iter().map(|(_, row)| row.clone()).collect(),
                next_cursor: match prefix.last() {
                    Some((last, _)) if cut => {
                        Some(call.cursors.seal_spans(window, as_of, *last, &filter))
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
