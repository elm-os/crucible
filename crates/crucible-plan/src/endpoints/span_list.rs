//! The span list (contract 8.7): a keyset scan of `span_core`, newest first, under the list filter. A metadata
//! condition is evaluated on `span_payload` and semi-joined in.

use crucible_records::{ProjectScope, SpanId, TableId, TimeWindow, Timestamp};
use crucible_schema::{Column, SpanCoreCol, SpanPayloadCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::filter::{Filter, FilterField, FilterTarget};
use crate::literal::Literal;
use crate::node::{ArrivalBounds, DEFAULT_MAX_IDS, Direction, Node, Order, Scan, SemiJoin};
use crate::plan::QueryPlan;

/// The largest page of the span list.
pub const SPAN_LIST_MAX_LIMIT: u32 = 200;

/// One page of the span list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanListRequest {
    /// The filter; [`Filter::all`] for none.
    pub filter: Filter,
    /// The page size, 1 to [`SPAN_LIST_MAX_LIMIT`].
    pub limit: u32,
    /// The sort tuple `(start_time, span_id)` of the last row of the previous page.
    pub cursor: Option<(Timestamp, SpanId)>,
    /// Fixed by the first page: only spans with `received_at <= as_of` are read, so pages do not shift.
    pub as_of: Timestamp,
}

/// The span list: every column of `span_core` for the spans that match the filter, ordered by
/// `(start_minute, start_time, span_id)` descending, which is `(start_time, span_id)` descending read along the
/// sort key. The scan reads one row more than the page, which tells the caller whether another page exists.
///
/// Conditions on `metadata` are evaluated on `span_payload`: they must be top-level conjuncts of the filter
/// (not under `or` or `not` with a condition on another field), and the plan is then a semi-join that is
/// refused as too broad when more than [`DEFAULT_MAX_IDS`] payload rows of the window match them.
pub fn span_list(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: SpanListRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    if request.limit == 0 || request.limit > SPAN_LIST_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let (core, payload) = request.filter.split(FilterTarget::SpanCore);
    let by = [
        SpanCoreCol::StartMinute,
        SpanCoreCol::StartTime,
        SpanCoreCol::SpanId,
    ];
    let mut scan = Scan::new(
        TableId::SpanCore,
        SpanCoreCol::ALL.iter().copied(),
        core.pred(FilterTarget::SpanCore)?,
    )?
    .ordered(Order::by(by, Direction::Desc))?
    .limited(request.limit + 1)?
    .arrived(ArrivalBounds {
        as_of: Some(request.as_of),
        since: None,
    })?;
    if let Some((start_time, span_id)) = request.cursor {
        scan = scan.after(vec![
            start_time.floor_minute().into(),
            start_time.into(),
            Literal::from(span_id),
        ])?;
    }
    let root = if payload.is_all() {
        Node::Scan(scan)
    } else {
        if payload.unsupported(FilterTarget::SpanPayload).is_some() {
            return Err(PlanError::UnsupportedFilter {
                field: FilterField::Metadata.as_str(),
                target: "a span list condition under `or` or `not` with another field",
            });
        }
        let ids = Scan::new(
            TableId::SpanPayload,
            [SpanPayloadCol::SpanId],
            payload.pred(FilterTarget::SpanPayload)?,
        )?;
        Node::SemiJoin(SemiJoin::new(
            scan,
            SpanCoreCol::SpanId,
            Node::Scan(ids),
            DEFAULT_MAX_IDS,
        )?)
    };
    clamp.plan(scope, window, caller.class(QueryClass::List), root)
}
