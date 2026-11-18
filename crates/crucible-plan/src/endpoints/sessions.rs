//! Sessions (contract 8.7 and 3.10): the session list and one session, from `session_rollup`. A session's
//! traces are the trace list filtered by its `session_id`.

use crucible_records::{ProjectScope, TableId, TimeWindow, Timestamp};
use crucible_schema::{Column, SessionRollupCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{AggOutput, Aggregate, Direction, Measure, Node};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// The largest page of the session list.
pub const SESSION_LIST_MAX_LIMIT: u32 = 200;

/// One page of the session list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionListRequest {
    /// The page size, 1 to [`SESSION_LIST_MAX_LIMIT`].
    pub limit: u32,
    /// The sort tuple `(last_seen, session_id)` of the last row of the previous page.
    pub cursor: Option<(Timestamp, String)>,
}

/// The merged rollup rows of sessions. The outputs are, in order: `session_id`, `max(last_seen)`,
/// `min(first_seen)`, `sum(trace_count)`, `sum(span_count)`, `sum(error_count)`, `sum(total_cost_nusd)`,
/// `sum(input_tokens)`, `sum(output_tokens)`, `sum(total_tokens)` and the merged set of `user_ids`.
fn sessions(pred: Pred) -> Result<Aggregate, PlanError> {
    let column = |c: SessionRollupCol| c.column_ref();
    Aggregate::new(
        TableId::SessionRollup,
        pred,
        [SessionRollupCol::SessionId],
        vec![
            Measure::Max(column(SessionRollupCol::LastSeen)),
            Measure::Min(column(SessionRollupCol::FirstSeen)),
            Measure::Sum(column(SessionRollupCol::TraceCount)),
            Measure::Sum(column(SessionRollupCol::SpanCount)),
            Measure::Sum(column(SessionRollupCol::ErrorCount)),
            Measure::Sum(column(SessionRollupCol::TotalCostNusd)),
            Measure::Sum(column(SessionRollupCol::InputTokens)),
            Measure::Sum(column(SessionRollupCol::OutputTokens)),
            Measure::Sum(column(SessionRollupCol::TotalTokens)),
            Measure::SetMerge(column(SessionRollupCol::UserIds)),
        ],
    )
}

/// The session list: the sessions with a rollup row in the window's days, merged per session, last seen first,
/// with the keyset `(last_seen, session_id)`. The plan reads one row more than the page, which tells the caller
/// whether another page exists. The outputs are those of [`session_detail`].
///
/// The rollup has day granularity, so the window selects whole UTC days, and the rows are partials that merge
/// at read time: a session that is seen again between two pages moves up and can be missed by the later page.
pub fn session_list(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: SessionListRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    if request.limit == 0 || request.limit > SESSION_LIST_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let mut aggregate = sessions(Pred::all())?
        .ordered(vec![
            (AggOutput::Measure(0), Direction::Desc),
            (AggOutput::Group(0), Direction::Desc),
        ])?
        .limited(request.limit + 1)?;
    if let Some((last_seen, session_id)) = request.cursor {
        aggregate = aggregate.after(vec![last_seen.into(), Literal::String(session_id)])?;
    }
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}

/// One session: its merged rollup row, or no row when the window's days hold none. The outputs are, in order:
/// `session_id`, `max(last_seen)`, `min(first_seen)`, `sum(trace_count)`, `sum(span_count)`,
/// `sum(error_count)`, `sum(total_cost_nusd)`, `sum(input_tokens)`, `sum(output_tokens)`, `sum(total_tokens)`
/// and the merged set of `user_ids` (ascending, at most 8).
pub fn session_detail(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    session_id: &str,
) -> Result<Option<QueryPlan>, PlanError> {
    let aggregate = sessions(Pred::eq(SessionRollupCol::SessionId, session_id))?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}
