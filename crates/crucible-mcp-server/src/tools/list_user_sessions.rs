//! `list_user_sessions`: one page of the sessions of a user (contract 8.7 and 12.5), latest trace first.

use crucible_cursor::IdList;
use crucible_query::UserSessionsQuery;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{Meta, UserSessionRow, fit, meta};
use crate::tools::{Call, Check, Tool, check_window};

/// The most sessions one call returns.
const MAX_ROWS: u32 = 50;
/// The longest user id, in bytes (contract 3.1).
const MAX_USER_ID: usize = 512;

pub(crate) struct ListUserSessions;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The user id, as the spans name it: 1 to 512 bytes.
    user_id: String,
    /// The start of the window, RFC 3339 (inclusive). Required: get_data_bounds gives the range with data.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// Sessions per page, from 1 to 50; 50 when absent.
    #[serde(default)]
    limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same `user_id`, `from` and `to`.
    #[serde(default)]
    cursor: Option<String>,
}

/// One page of the user's sessions, latest trace first.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The sessions that the user's traces in the window name.
    data: Vec<UserSessionRow>,
    /// Pass it as `cursor`, with the same `user_id`, `from` and `to`, for the next page; null on the last page.
    next_cursor: Option<String>,
    /// Whether another page exists.
    has_more: bool,
    /// True when this page was cut to keep the result under 64 KiB; `next_cursor` continues after its last
    /// session.
    truncated: bool,
    /// How complete and how fresh the page is.
    meta: Meta,
}

impl Tool for ListUserSessions {
    const NAME: &'static str = "list_user_sessions";
    const DESCRIPTION: &'static str = "List the sessions that one user's traces in the window [from, to) name, \
        latest trace first, from the head span of each trace: the first and last trace start and the user's \
        trace count per session. At most 50 sessions and 64 KiB per call; pass `next_cursor` back as `cursor`, \
        with the same user and window, for more. Find users with list_users; read a session's traces with \
        list_traces and `filter.session_id`.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        if args.user_id.is_empty() || args.user_id.len() > MAX_USER_ID {
            check.error(
                "user_id",
                "invalid_id",
                &format!("expected a user id of 1 to {MAX_USER_ID} bytes"),
            );
        }
        let window = check.window(&args.from, &args.to);
        let limit = check.limit(args.limit, MAX_ROWS, MAX_ROWS);
        check.finish()?;
        let Some(window) = window else {
            return Err(ToolError::validation(Vec::new()));
        };
        check_window(window, call.settings.max_window_days)?;
        let (list, user) = (IdList::UserSessions, Some(args.user_id.as_str()));
        let cursor = args
            .cursor
            .as_deref()
            .map(|text| call.cursors.open_ids(text, list, user, window))
            .transpose()
            .map_err(ToolError::cursor)?;
        let query = UserSessionsQuery {
            window,
            user_id: args.user_id.clone(),
            limit: Some(limit),
            cursor,
        };
        let page = call.service.user_sessions(&call.ctx, query).await;
        let page = page.map_err(|e| ToolError::query(&e, call.request_id))?;
        let rows: Vec<_> = page
            .data
            .iter()
            .map(|session| {
                let key = (session.last_trace_at, session.session_id.clone());
                (key, UserSessionRow::new(session))
            })
            .collect();
        Ok(fit(
            rows,
            |_| {},
            |prefix, cut| Output {
                data: prefix.iter().map(|(_, row)| row.clone()).collect(),
                next_cursor: match prefix.last() {
                    Some(((last_trace_at, id), _)) if cut => {
                        call.cursors
                            .seal_ids(list, user, window, (*last_trace_at, id.as_ref()))
                    }
                    _ => page.next_cursor.as_ref().and_then(|next| {
                        let (_, window, last) = next.parts();
                        call.cursors.seal_ids(list, user, window, last)
                    }),
                },
                has_more: cut || page.has_more,
                truncated: cut,
                meta: meta(&page.meta),
            },
        ))
    }
}
