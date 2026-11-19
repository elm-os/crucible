//! Users (contract 8.7 and 3.10): the user list and one user, the `user_rollup` partials of the window's UTC
//! days merged per user; each user's distinct session count and the sessions of one user, computed at read
//! time from `trace_heads`. A user's traces are the trace list filtered by its `user_id`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crucible_plan::{
    MAX_LIST, Row, UserListRequest, UserSessionsRequest, user_detail, user_list,
    user_session_counts, user_sessions,
};
use crucible_records::{NanoUsd, TableId, TimeWindow, Timestamp};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::IdCursor;
use crate::decode::Fields;
use crate::heads::page_limit;
use crate::service::{QueryService, Read, lagging_if, on_window_days};

/// The tables user reads depend on, for the quarantine rule.
const USER_TABLES: [TableId; 2] = [TableId::UserRollup, TableId::TraceHeads];
/// The table the sessions of a user depend on.
const USER_SESSION_TABLES: [TableId; 1] = [TableId::TraceHeads];

/// One user, merged from its rollup partials.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserSummary {
    /// The user id.
    pub user_id: Arc<str>,
    /// The earliest span start.
    pub first_seen: Timestamp,
    /// The latest span end: the list's sort key.
    pub last_seen: Timestamp,
    /// Root spans: the user's traces.
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
    /// The distinct sessions the user's trace heads in the window name.
    pub session_count: u64,
}

impl UserSummary {
    /// A row of `user_list` or `user_detail`, before its session count is known.
    fn decode(row: &Row) -> Result<Self, QueryServiceError> {
        let f = Fields(row.values());
        Ok(Self {
            user_id: f.text(0)?,
            last_seen: f.time(1)?,
            first_seen: f.time(2)?,
            trace_count: f.uint(3)?,
            span_count: f.uint(4)?,
            error_count: f.uint(5)?,
            total_cost_nusd: NanoUsd::from_nanos(f.int(6)?),
            input_tokens: f.uint(7)?,
            output_tokens: f.uint(8)?,
            total_tokens: f.uint(9)?,
            session_count: 0,
        })
    }
}

/// A request for one page of the user list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserListQuery {
    /// The window; the rollup selects its whole UTC days, the session counts its exact span.
    pub window: TimeWindow,
    /// The page size; default 50, at most 200.
    pub limit: Option<u32>,
    /// The cursor of the previous page.
    pub cursor: Option<IdCursor>,
}

/// One page of the user list, last seen first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserListPage {
    /// The users.
    pub data: Vec<UserSummary>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<IdCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The response metadata.
    pub meta: Meta,
}

/// One user.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserDetail {
    /// The user; `None` only when the response is partial or lagging, so the user may exist unseen.
    pub user: Option<UserSummary>,
    /// The response metadata.
    pub meta: Meta,
}

/// One session of a user, from the user's trace heads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserSession {
    /// The session id.
    pub session_id: Arc<str>,
    /// The start of the session's earliest trace head in the window.
    pub first_trace_at: Timestamp,
    /// The start of its latest trace head: the list's sort key.
    pub last_trace_at: Timestamp,
    /// The distinct traces whose head names the user and the session.
    pub trace_count: u64,
}

/// A request for one page of the sessions of a user.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserSessionsQuery {
    /// The window over the trace heads' start times.
    pub window: TimeWindow,
    /// The user.
    pub user_id: String,
    /// The page size; default 50, at most 200.
    pub limit: Option<u32>,
    /// The cursor of the previous page.
    pub cursor: Option<IdCursor>,
}

/// One page of the sessions of a user, latest trace first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserSessionsPage {
    /// The sessions.
    pub data: Vec<UserSession>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<IdCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The response metadata.
    pub meta: Meta,
}

impl Read<'_> {
    /// The distinct session count of each user, for the users that have a session.
    async fn session_counts(
        &mut self,
        window: TimeWindow,
        user_ids: &[String],
    ) -> Result<BTreeMap<Arc<str>, u64>, QueryServiceError> {
        let mut counts = BTreeMap::new();
        for ids in user_ids.chunks(MAX_LIST) {
            let (scope, caller) = (self.ctx.scope, self.ctx.caller);
            let plan = user_session_counts(scope, window, &self.clamp, caller, ids);
            for row in self.rows(plan).await? {
                let f = Fields(row.values());
                counts.insert(f.text(0)?, f.uint(1)?);
            }
        }
        Ok(counts)
    }
}

impl QueryService {
    /// One page of the user list (contract 8.7): the rollup merged per user over the window's days, last seen
    /// first, keyset `(last_seen, user_id)`, with each user's distinct session count (contract 3.10).
    ///
    /// As for sessions, the rollup is read from the store alone, so the answer is `consistency: lagging` while
    /// the recent index holds spans of a user that the store may not hold yet, and pages are not snapshots.
    pub async fn user_list(
        &self,
        ctx: &ReadContext,
        query: UserListQuery,
    ) -> Result<UserListPage, QueryServiceError> {
        let mut read = self.read(ctx);
        let limit = page_limit(query.limit)?;
        let cursor = match &query.cursor {
            Some(cursor) => Some(cursor.check(ctx.scope, query.window)?),
            None => None,
        };
        let base = self.consistency();
        let (unconfirmed, state) = read.unconfirmed()?;
        let left_out = unconfirmed.iter().any(|span| {
            !span.summary.user_id.is_empty()
                && on_window_days(query.window, span.summary.start_time)
        });
        let request = UserListRequest { limit, cursor };
        let plan = user_list(ctx.scope, query.window, &read.clamp, ctx.caller, request);
        let rows = read.rows(plan).await?;
        let mut data = rows
            .iter()
            .map(UserSummary::decode)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = data.len() > limit as usize;
        data.truncate(limit as usize);
        let ids: Vec<String> = data.iter().map(|user| user.user_id.to_string()).collect();
        if !ids.is_empty() {
            let counts = read.session_counts(query.window, &ids).await?;
            for user in &mut data {
                user.session_count = counts.get(&user.user_id).copied().unwrap_or(0);
            }
        }
        let meta = read.finish(
            read.now,
            lagging_if(base, left_out),
            state,
            &USER_TABLES,
            false,
        )?;
        let project_id = ctx.scope.project_id();
        let next_cursor = data.last().filter(|_| has_more).map(|last| {
            let position = (last.last_seen, last.user_id.to_string());
            IdCursor::from_parts(project_id, query.window, position)
        });
        Ok(UserListPage {
            data,
            next_cursor,
            has_more,
            meta,
        })
    }

    /// One user (contract 8.7), merged over the window's days, with its distinct session count. Absent from a
    /// whole and strong answer, it is `not_found`; absent from any other, the answer says why it may be missing.
    pub async fn user(
        &self,
        ctx: &ReadContext,
        window: TimeWindow,
        user_id: &str,
    ) -> Result<UserDetail, QueryServiceError> {
        let mut read = self.read(ctx);
        let base = self.consistency();
        let (unconfirmed, state) = read.unconfirmed()?;
        let left_out = unconfirmed.iter().any(|span| {
            &*span.summary.user_id == user_id && on_window_days(window, span.summary.start_time)
        });
        let plan = user_detail(ctx.scope, window, &read.clamp, ctx.caller, user_id);
        let rows = read.rows(plan).await?;
        let mut user = rows.first().map(UserSummary::decode).transpose()?;
        if let Some(user) = &mut user {
            let counts = read.session_counts(window, &[user_id.to_owned()]).await?;
            user.session_count = counts.get(user_id).copied().unwrap_or(0);
        }
        let meta = read.finish(
            read.now,
            lagging_if(base, left_out),
            state,
            &USER_TABLES,
            false,
        )?;
        if user.is_none() && meta.is_whole() {
            return Err(QueryServiceError::NotFound);
        }
        Ok(UserDetail { user, meta })
    }

    /// One page of the sessions of a user (contract 3.10): the sessions its trace heads in the window name,
    /// latest trace first, keyset `(last_trace_at, session_id)`. Read from the store alone, so the answer is
    /// `consistency: lagging` while the recent index holds such heads that the store may not hold yet.
    pub async fn user_sessions(
        &self,
        ctx: &ReadContext,
        query: UserSessionsQuery,
    ) -> Result<UserSessionsPage, QueryServiceError> {
        let mut read = self.read(ctx);
        let limit = page_limit(query.limit)?;
        let cursor = match &query.cursor {
            Some(cursor) => Some(cursor.check(ctx.scope, query.window)?),
            None => None,
        };
        let base = self.consistency();
        let (unconfirmed, state) = read.unconfirmed()?;
        let left_out = unconfirmed.iter().any(|span| {
            let s = &span.summary;
            span.head.is_some()
                && *s.user_id == *query.user_id
                && !s.session_id.is_empty()
                && query.window.contains(s.start_time)
        });
        let request = UserSessionsRequest {
            user_id: query.user_id.clone(),
            limit,
            cursor,
        };
        let plan = user_sessions(ctx.scope, query.window, &read.clamp, ctx.caller, request);
        let rows = read.rows(plan).await?;
        let mut data = rows
            .iter()
            .map(|row| {
                let f = Fields(row.values());
                Ok(UserSession {
                    session_id: f.text(0)?,
                    last_trace_at: f.time(1)?,
                    first_trace_at: f.time(2)?,
                    trace_count: f.uint(3)?,
                })
            })
            .collect::<Result<Vec<_>, QueryServiceError>>()?;
        let has_more = data.len() > limit as usize;
        data.truncate(limit as usize);
        let meta = read.finish(
            read.now,
            lagging_if(base, left_out),
            state,
            &USER_SESSION_TABLES,
            false,
        )?;
        let project_id = ctx.scope.project_id();
        let next_cursor = data.last().filter(|_| has_more).map(|last| {
            let position = (last.last_trace_at, last.session_id.to_string());
            IdCursor::from_parts(project_id, query.window, position)
        });
        Ok(UserSessionsPage {
            data,
            next_cursor,
            has_more,
            meta,
        })
    }
}
