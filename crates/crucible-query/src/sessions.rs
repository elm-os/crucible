//! Sessions (contract 8.7 and 3.10): the session list and one session, the `session_rollup` partials of the
//! window's UTC days merged per session. A session's traces are the trace list filtered by its `session_id`.

use std::sync::Arc;

use crucible_plan::{Row, SessionListRequest, session_detail, session_list};
use crucible_records::{NanoUsd, TableId, TimeWindow, Timestamp};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::IdCursor;
use crate::decode::Fields;
use crate::heads::page_limit;
use crate::service::{QueryService, lagging_if, on_window_days};

/// The table session reads depend on, for the quarantine rule.
const SESSION_TABLES: [TableId; 1] = [TableId::SessionRollup];

/// One session, merged from its rollup partials.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionSummary {
    /// The session id.
    pub session_id: Arc<str>,
    /// The earliest span start.
    pub first_seen: Timestamp,
    /// The latest span end: the list's sort key.
    pub last_seen: Timestamp,
    /// Root spans: the session's traces.
    pub trace_count: u64,
    /// Spans.
    pub span_count: u64,
    /// Spans with an error.
    pub error_count: u64,
    /// The total cost.
    pub total_cost_nusd: NanoUsd,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
    /// The users the session's spans name, ascending, at most 8 (the rollup keeps no more).
    pub user_ids: Vec<Arc<str>>,
}

impl SessionSummary {
    /// A row of `session_list` or `session_detail`.
    fn decode(row: &Row) -> Result<Self, QueryServiceError> {
        let f = Fields(row.values());
        Ok(Self {
            session_id: f.text(0)?,
            last_seen: f.time(1)?,
            first_seen: f.time(2)?,
            trace_count: f.uint(3)?,
            span_count: f.uint(4)?,
            error_count: f.uint(5)?,
            total_cost_nusd: NanoUsd::from_nanos(f.int(6)?),
            input_tokens: f.uint(7)?,
            output_tokens: f.uint(8)?,
            total_tokens: f.uint(9)?,
            user_ids: f.texts(10)?,
        })
    }
}

/// A request for one page of the session list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionListQuery {
    /// The window; the rollup selects its whole UTC days.
    pub window: TimeWindow,
    /// The page size; default 50, at most 200.
    pub limit: Option<u32>,
    /// The cursor of the previous page.
    pub cursor: Option<IdCursor>,
}

/// One page of the session list, last seen first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionListPage {
    /// The sessions.
    pub data: Vec<SessionSummary>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<IdCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The response metadata.
    pub meta: Meta,
}

/// One session.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionDetail {
    /// The session; `None` only when the response is partial or lagging, so the session may exist unseen.
    pub session: Option<SessionSummary>,
    /// The response metadata.
    pub meta: Meta,
}

impl QueryService {
    /// One page of the session list (contract 8.7): the rollup merged per session over the window's days, last
    /// seen first, keyset `(last_seen, session_id)`.
    ///
    /// The rollup is read from the store alone, so the answer is `consistency: lagging` while the recent index
    /// holds spans of a session on those days that the store may not hold yet. Pages merge partials at read
    /// time and are not snapshots: a session seen again between two pages moves up.
    pub async fn session_list(
        &self,
        ctx: &ReadContext,
        query: SessionListQuery,
    ) -> Result<SessionListPage, QueryServiceError> {
        let mut read = self.read(ctx, "session_list");
        let limit = page_limit(query.limit)?;
        let cursor = match &query.cursor {
            Some(cursor) => Some(cursor.check(ctx.scope, query.window)?),
            None => None,
        };
        let base = self.consistency();
        let (unconfirmed, state) = read.unconfirmed()?;
        let left_out = unconfirmed.iter().any(|span| {
            !span.summary.session_id.is_empty()
                && on_window_days(query.window, span.summary.start_time)
        });
        let request = SessionListRequest { limit, cursor };
        let plan = session_list(ctx.scope, query.window, &read.clamp, ctx.caller, request);
        let rows = read.rows(plan).await?;
        let mut data = rows
            .iter()
            .map(SessionSummary::decode)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = data.len() > limit as usize;
        data.truncate(limit as usize);
        let meta = read.finish(
            read.now,
            lagging_if(base, left_out),
            state,
            &SESSION_TABLES,
            false,
        )?;
        let project_id = ctx.scope.project_id();
        let next_cursor = data.last().filter(|_| has_more).map(|last| {
            let position = (last.last_seen, last.session_id.to_string());
            IdCursor::from_parts(project_id, query.window, position)
        });
        Ok(SessionListPage {
            data,
            next_cursor,
            has_more,
            meta,
        })
    }

    /// One session (contract 8.7), merged over the window's days. Absent from a whole and strong answer, it is
    /// `not_found`; absent from any other, the answer says why it may be missing (contract 8.9).
    pub async fn session(
        &self,
        ctx: &ReadContext,
        window: TimeWindow,
        session_id: &str,
    ) -> Result<SessionDetail, QueryServiceError> {
        let mut read = self.read(ctx, "session");
        let base = self.consistency();
        let (unconfirmed, state) = read.unconfirmed()?;
        let left_out = unconfirmed.iter().any(|span| {
            &*span.summary.session_id == session_id
                && on_window_days(window, span.summary.start_time)
        });
        let plan = session_detail(ctx.scope, window, &read.clamp, ctx.caller, session_id);
        let rows = read.rows(plan).await?;
        let session = rows.first().map(SessionSummary::decode).transpose()?;
        let meta = read.finish(
            read.now,
            lagging_if(base, left_out),
            state,
            &SESSION_TABLES,
            false,
        )?;
        if session.is_none() && meta.is_whole() {
            return Err(QueryServiceError::NotFound);
        }
        Ok(SessionDetail { session, meta })
    }
}
