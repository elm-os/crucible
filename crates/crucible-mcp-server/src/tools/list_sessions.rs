//! `list_sessions`: one page of the session list (contract 8.7 and 12.5), last seen first.

use crucible_cursor::IdList;
use crucible_query::SessionListQuery;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{Meta, SessionRow, fit, meta};
use crate::tools::{Call, Check, Tool, check_window};

/// The most sessions one call returns.
const MAX_ROWS: u32 = 50;

pub(crate) struct ListSessions;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The start of the window, RFC 3339 (inclusive). Required: get_data_bounds gives the range with data.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// Sessions per page, from 1 to 50; 50 when absent.
    #[serde(default)]
    limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same `from` and `to`.
    #[serde(default)]
    cursor: Option<String>,
}

/// One page of sessions, last seen first.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The sessions whose spans fall on the window's UTC days, each aggregated over those days.
    data: Vec<SessionRow>,
    /// Pass it as `cursor`, with the same `from` and `to`, for the next page; null on the last page.
    next_cursor: Option<String>,
    /// Whether another page exists.
    has_more: bool,
    /// True when this page was cut to keep the result under 64 KiB; `next_cursor` continues after its last
    /// session.
    truncated: bool,
    /// How complete and how fresh the page is.
    meta: Meta,
}

impl Tool for ListSessions {
    const NAME: &'static str = "list_sessions";
    const DESCRIPTION: &'static str = "List the project's sessions whose spans fall on the UTC days of the window \
        [from, to), last seen first, each with its spans aggregated over those days: first and last seen, trace, \
        span and error counts, cost, tokens and up to 8 user ids. Pages are computed when they are read, so a \
        session seen again between two pages can move up. At most 50 sessions and 64 KiB per call; pass \
        `next_cursor` back as `cursor`, with the same window, for more. A session's traces are list_traces with \
        `filter.session_id`.";
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
        let list = IdList::Sessions;
        let cursor = args
            .cursor
            .map(|text| call.cursors.open_ids(&text, list, None, window))
            .transpose()
            .map_err(ToolError::cursor)?;
        let query = SessionListQuery {
            window,
            limit: Some(limit),
            cursor,
        };
        let page = call.service.session_list(&call.ctx, query).await;
        let page = page.map_err(|e| ToolError::query(&e, call.request_id))?;
        let rows: Vec<_> = page
            .data
            .iter()
            .map(|session| {
                (
                    (session.last_seen, session.session_id.clone()),
                    SessionRow::new(session),
                )
            })
            .collect();
        Ok(fit(
            rows,
            |(_, row)| row.lighten(),
            |prefix, cut| Output {
                data: prefix.iter().map(|(_, row)| row.clone()).collect(),
                next_cursor: match prefix.last() {
                    Some(((last_seen, id), _)) if cut => {
                        call.cursors
                            .seal_ids(list, None, window, (*last_seen, id.as_ref()))
                    }
                    _ => page.next_cursor.as_ref().and_then(|next| {
                        let (_, window, last) = next.parts();
                        call.cursors.seal_ids(list, None, window, last)
                    }),
                },
                has_more: cut || page.has_more,
                truncated: cut,
                meta: meta(&page.meta),
            },
        ))
    }
}
