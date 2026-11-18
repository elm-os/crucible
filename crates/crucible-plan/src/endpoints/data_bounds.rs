//! Data bounds (contract 8.7 and 8.8): the first and last minute with data, from the minute rollups of the
//! registry's sources.

use crucible_records::{ProjectScope, TimeWindow};
use crucible_schema::{Column, McpToolRollupCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::node::{Aggregate, Measure, Node};
use crate::plan::QueryPlan;
use crate::pred::Pred;
use crate::registry::Source;

/// The two plans of the data-bounds endpoint. Each returns `min(minute)` and `max(minute)` of a rollup; a plan
/// is `None` when the rollup's whole window has expired.
#[derive(Clone, PartialEq, Debug)]
pub struct DataBoundsPlans {
    /// Over `span_metrics`: one row, or none when the window holds no data.
    pub spans: Option<QueryPlan>,
    /// Over `mcp_tool_rollup`, grouped by `side`: one row per side.
    pub mcp: Option<QueryPlan>,
}

/// Data bounds: the first and last minute with data, from the minute rollups of the registry's sources.
pub fn data_bounds(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
) -> Result<DataBoundsPlans, PlanError> {
    let class = caller.class(QueryClass::Analytics);
    let bounds = |source: Source, by_side: bool| {
        let rollup = source.rollup().ok_or(PlanError::Empty)?;
        let group = by_side.then(|| McpToolRollupCol::Side.column_ref());
        let aggregate = Aggregate::new(
            rollup.table,
            Pred::all(),
            group,
            vec![Measure::Min(rollup.minute), Measure::Max(rollup.minute)],
        )?;
        clamp.plan(scope, window, class, Node::Aggregate(aggregate))
    };
    Ok(DataBoundsPlans {
        spans: bounds(Source::Spans, false)?,
        mcp: bounds(Source::McpOps, true)?,
    })
}
