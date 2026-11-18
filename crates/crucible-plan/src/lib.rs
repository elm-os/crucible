//! The typed query-plan IR (contract section 8) and the builders for the first endpoints.
//!
//! Every telemetry read is a [`QueryPlan`]: a scope, a window, a class and a tree of nodes. The IR holds no SQL
//! text. Columns are `crucible_schema::ColumnRef`s, which only the schema's column enums produce and which never
//! name `project_id`; values are typed [`Literal`]s. That is what lets the renderer in `crucible-store-clickhouse`
//! add the project and window predicates itself and bind every value as a server-side parameter.
//!
//! This crate is pure: no `Env`, no IO.

mod build;
mod class;
mod error;
mod literal;
mod node;
mod plan;
mod pred;
mod registry;
#[cfg(feature = "test-support")]
pub mod test_support;
mod value;

pub use build::{
    DataBoundsPlans, SPAN_CHILDREN_DEFAULT_LIMIT, SPAN_CHILDREN_MAX_LIMIT, TRACE_INLINE_SPAN_LIMIT,
    TRACE_LIST_MAX_LIMIT, TRACE_LIST_OVER_READ, TraceBounds, TraceListRequest, data_bounds,
    effective_window, span_children, trace_bounds_window, trace_list, trace_spans,
    trace_time_bounds,
};
pub use class::{
    CLASS_FIXED_SETTINGS, Caller, ClassLimits, DEADLINE_SLACK_SECS, QueryClass, profile_caps,
};
pub use error::PlanError;
pub use literal::{Literal, pad_in_list, padded_in_len};
pub use node::{
    AggOutput, Aggregate, ArrivalBounds, DEFAULT_MAX_IDS, Direction, FieldRange, Fill, Having,
    Measure, Node, Order, PointLookup, Scan, SemiJoin, Step, TextSearch, TimeSeries, TokenMode,
};
pub use plan::{BatchProbe, DeletePlan, DeleteTarget, ProbeKind, QueryPlan};
pub use pred::{CmpOp, MAX_LIST, Pred};
pub use registry::{Rollup, Source};
pub use value::{MAX_ROWS_PER_BLOCK, OutputColumn, OutputSchema, Row, RowBlock, ShapeError, Value};
