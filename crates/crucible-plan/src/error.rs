//! Why a plan could not be built.

use crucible_records::TableId;

/// A plan that the IR refuses to represent. Builders return this instead of producing a statement the renderer
/// could not render safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// A column of another table.
    #[error("column {column} does not belong to table {table:?}")]
    ForeignColumn {
        /// The table of the node.
        table: TableId,
        /// The offending column.
        column: &'static str,
    },
    /// A literal whose type does not match its column.
    #[error("literal does not fit column {0}")]
    LiteralType(&'static str),
    /// A predicate form that the column or table does not support.
    #[error("unsupported predicate on {0}")]
    UnsupportedPred(&'static str),
    /// An `In` or `HasAny` list that is empty or longer than the cap.
    #[error("list for {0} is empty or too long")]
    ListLength(&'static str),
    /// A `Scan`, `PointLookup`, `TextSearch` or `FieldRange` over an aggregating table: those are read only
    /// through `Aggregate` (contract section 8.3).
    #[error("table {0:?} is aggregating and must be read through an aggregate")]
    AggregatingTable(TableId),
    /// A measure that the column cannot be merged with.
    #[error("measure not valid for column {0}")]
    Measure(&'static str),
    /// A group dimension that is aggregated, or not part of the key of an aggregating table.
    #[error("column {0} cannot be a group dimension")]
    GroupDimension(&'static str),
    /// No output columns, no keys, no measures or no fields.
    #[error("the node selects nothing")]
    Empty,
    /// A limit of zero, or above the cap of the node.
    #[error("limit out of range")]
    Limit,
    /// A cursor without an order, or whose values do not match the order columns.
    #[error("cursor does not match the order")]
    Cursor,
    /// A key whose arity or types do not match the key columns.
    #[error("key does not match the key columns")]
    Key,
    /// Arrival bounds on a table without `received_at`.
    #[error("table {0:?} has no arrival time")]
    NoArrivalTime(TableId),
    /// A `having` or `order` reference to a measure or dimension that is not there.
    #[error("reference to an output that does not exist")]
    OutputIndex,
    /// The right side of a semi-join must produce exactly one id column of the left column's type.
    #[error("semi-join sides do not line up")]
    SemiJoin,
    /// A partition id of the wrong kind for the table.
    #[error("partition does not belong to table {0:?}")]
    Partition(TableId),
    /// Trace bounds whose end is before their start, or outside the representable range.
    #[error("trace bounds are not a time range")]
    Bounds,
    /// The retention cut-off cannot be computed: the clock is outside the valid time range.
    #[error("the retention cut-off is outside the valid time range")]
    Retention,
    /// The table is not one the operation applies to: for a read, a table without `project_id`.
    #[error("table {0:?} is not valid here")]
    Table(TableId),
}
