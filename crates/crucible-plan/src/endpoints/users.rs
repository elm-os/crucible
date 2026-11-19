//! Users (contract 8.7 and 3.10): the user list and one user, from `user_rollup`; and the sessions of users,
//! which are computed at read time from `trace_heads`: the distinct session count of a page of users, and the
//! sessions of one user.

use crucible_records::{ProjectScope, TableId, TimeWindow, Timestamp};
use crucible_schema::{Column, TraceHeadsCol, UserRollupCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{AggOutput, Aggregate, Direction, Measure, Node};
use crate::plan::QueryPlan;
use crate::pred::{CmpOp, Pred};

/// The largest page of the user list.
pub const USER_LIST_MAX_LIMIT: u32 = 200;
/// The largest page of the sessions of one user.
pub const USER_SESSIONS_MAX_LIMIT: u32 = 200;

/// One page of the sessions of one user.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserSessionsRequest {
    /// The user.
    pub user_id: String,
    /// The page size, 1 to [`USER_SESSIONS_MAX_LIMIT`].
    pub limit: u32,
    /// The sort tuple `(latest head start, session_id)` of the last row of the previous page.
    pub cursor: Option<(Timestamp, String)>,
}

/// One page of the user list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserListRequest {
    /// The page size, 1 to [`USER_LIST_MAX_LIMIT`].
    pub limit: u32,
    /// The sort tuple `(last_seen, user_id)` of the last row of the previous page.
    pub cursor: Option<(Timestamp, String)>,
}

fn users(pred: Pred) -> Result<Aggregate, PlanError> {
    let column = |c: UserRollupCol| c.column_ref();
    Aggregate::new(
        TableId::UserRollup,
        pred,
        [UserRollupCol::UserId],
        vec![
            Measure::Max(column(UserRollupCol::LastSeen)),
            Measure::Min(column(UserRollupCol::FirstSeen)),
            Measure::Sum(column(UserRollupCol::TraceCount)),
            Measure::Sum(column(UserRollupCol::SpanCount)),
            Measure::Sum(column(UserRollupCol::ErrorCount)),
            Measure::Sum(column(UserRollupCol::TotalCostNusd)),
            Measure::Sum(column(UserRollupCol::InputTokens)),
            Measure::Sum(column(UserRollupCol::OutputTokens)),
            Measure::Sum(column(UserRollupCol::TotalTokens)),
        ],
    )
}

/// The user list: the users with a rollup row in the window's days, merged per user, last seen first, with
/// the keyset `(last_seen, user_id)`. The plan reads one row more than the page. The outputs are those of
/// [`user_detail`]. As for sessions, the window selects whole UTC days and pages are not snapshots.
pub fn user_list(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: UserListRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    if request.limit == 0 || request.limit > USER_LIST_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let mut aggregate = users(Pred::all())?
        .ordered(vec![
            (AggOutput::Measure(0), Direction::Desc),
            (AggOutput::Group(0), Direction::Desc),
        ])?
        .limited(request.limit + 1)?;
    if let Some((last_seen, user_id)) = request.cursor {
        aggregate = aggregate.after(vec![last_seen.into(), Literal::String(user_id)])?;
    }
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}

/// One user: its merged rollup row, or no row when the window's days hold none. The outputs are, in order:
/// `user_id`, `max(last_seen)`, `min(first_seen)`, `sum(trace_count)`, `sum(span_count)`, `sum(error_count)`,
/// `sum(total_cost_nusd)`, `sum(input_tokens)`, `sum(output_tokens)` and `sum(total_tokens)`.
pub fn user_detail(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    user_id: &str,
) -> Result<Option<QueryPlan>, PlanError> {
    let aggregate = users(Pred::eq(UserRollupCol::UserId, user_id))?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}

/// The sessions of one user (contract 3.10: a user's sessions are computed at read time): the non-empty
/// `session_id` values of the user's trace heads in the window, one row per session, latest head first, with
/// the keyset `(max(start_time), session_id)`. Heads carry the head span's values (contract 3.3), as the
/// session count of [`user_session_counts`] does. The plan reads one row more than the page. The outputs are,
/// in order: `session_id`, `max(start_time)`, `min(start_time)` and the distinct count of `trace_id`.
pub fn user_sessions(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: UserSessionsRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    if request.limit == 0 || request.limit > USER_SESSIONS_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let start = TraceHeadsCol::StartTime.column_ref();
    let mut aggregate = Aggregate::new(
        TableId::TraceHeads,
        Pred::And(vec![
            Pred::eq(TraceHeadsCol::UserId, request.user_id.as_str()),
            Pred::cmp(TraceHeadsCol::SessionId, CmpOp::Ne, ""),
        ]),
        [TraceHeadsCol::SessionId],
        vec![
            Measure::Max(start),
            Measure::Min(start),
            Measure::Uniq(TraceHeadsCol::TraceId.column_ref()),
        ],
    )?
    .ordered(vec![
        (AggOutput::Measure(0), Direction::Desc),
        (AggOutput::Group(0), Direction::Desc),
    ])?
    .limited(request.limit + 1)?;
    if let Some((latest, session_id)) = request.cursor {
        aggregate = aggregate.after(vec![latest.into(), Literal::String(session_id)])?;
    }
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}

/// The distinct session count of several users (contract 3.10): `user_id` and the number of distinct
/// non-empty `session_id` values among the trace heads of the window, one row per user that has one.
pub fn user_session_counts(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    user_ids: &[String],
) -> Result<Option<QueryPlan>, PlanError> {
    let ids = user_ids.iter().cloned().map(Literal::String).collect();
    let aggregate = Aggregate::new(
        TableId::TraceHeads,
        Pred::And(vec![
            Pred::is_in(TraceHeadsCol::UserId, ids),
            Pred::cmp(TraceHeadsCol::SessionId, CmpOp::Ne, ""),
        ]),
        [TraceHeadsCol::UserId],
        vec![Measure::Uniq(TraceHeadsCol::SessionId.column_ref())],
    )?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}
