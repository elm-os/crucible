//! The filter-values lookup (contract 8.7 and 8.8): the most frequent values of a filter field of a metrics
//! source in a window, with how often each occurs.

use crucible_records::{ProjectScope, TimeWindow};
use crucible_schema::{Column, SpanMetricsCol, Ty};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::filter::{FilterField, FilterTarget};
use crate::node::{AggOutput, Aggregate, Direction, Measure, Node};
use crate::plan::QueryPlan;
use crate::pred::Pred;
use crate::registry::{Source, SourceSchema};

/// The most values one lookup returns.
pub const FILTER_VALUES_MAX_LIMIT: u32 = 100;

/// One filter-values lookup.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilterValuesRequest {
    /// The metrics source whose filter field is looked up.
    pub source: Source,
    /// The field: one of the source's filter fields in the metrics schema.
    pub field: FilterField,
    /// Only values that start with this text. The plan applies it to string fields; for the fields with a
    /// closed set of names (`level`, `observation_type`) the caller applies it to the names.
    pub prefix: Option<String>,
    /// The most values to return, 1 to [`FILTER_VALUES_MAX_LIMIT`]. The plan reads one more, which tells the
    /// caller that the list is truncated.
    pub limit: u32,
}

/// The values of a filter field with their counts, most frequent first and ties in ascending value order, at
/// most `limit + 1` rows. Every filter field of the `spans` source is a dimension of `span_metrics`, so the
/// lookup reads the rollup: the outputs are the field's column, then `sum(count)`, the spans that carry the
/// value. A field the source does not list is refused with [`PlanError::UnsupportedFilter`].
pub fn filter_values(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: &FilterValuesRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    if request.limit == 0 || request.limit > FILTER_VALUES_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let listed = SourceSchema::of(request.source)
        .is_some_and(|schema| schema.filter_fields.contains(&request.field));
    let column = FilterTarget::SpanMetrics.column(request.field);
    let (true, Some(column)) = (listed, column) else {
        return Err(PlanError::UnsupportedFilter {
            field: request.field.as_str(),
            target: request.source.as_str(),
        });
    };
    let pred = match &request.prefix {
        Some(prefix) if matches!(column.ty().scalar(), Ty::String | Ty::LowCardinalityString) => {
            Pred::StartsWith(column, prefix.clone())
        }
        _ => Pred::all(),
    };
    let aggregate = Aggregate::new(
        SpanMetricsCol::TABLE,
        pred,
        [column],
        vec![Measure::Sum(SpanMetricsCol::Count.column_ref())],
    )?
    .ordered(vec![
        (AggOutput::Measure(0), Direction::Desc),
        (AggOutput::Group(0), Direction::Asc),
    ])?
    .limited(request.limit + 1)?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Aggregate(aggregate),
    )
}
