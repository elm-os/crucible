//! `Pred`: the closed predicate AST (contract section 8.1).
//!
//! There is no project variant. Columns are [`ColumnRef`]s, which only the schema's per-table column enums can
//! produce and which never name `project_id`; the renderer adds `project_id = {scope}` to every table reference
//! itself. Values are typed [`Literal`]s.

use crucible_records::TableId;
use crucible_schema::{ColumnRef, IndexKind, Ty, table};

use crate::error::PlanError;
use crate::literal::Literal;

/// The longest `In` or `HasAny` list a plan may hold.
pub const MAX_LIST: usize = 10_000;

/// A comparison operator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A predicate over the rows of one table.
#[derive(Clone, PartialEq, Debug)]
pub enum Pred {
    /// Every operand holds; the empty conjunction is "all rows".
    And(Vec<Pred>),
    /// Any operand holds.
    Or(Vec<Pred>),
    /// The operand does not hold.
    Not(Box<Pred>),
    /// A column compared with a typed literal.
    Cmp(ColumnRef, CmpOp, Literal),
    /// A column equal to one of the values.
    In(ColumnRef, Vec<Literal>),
    /// A string column that starts with the prefix.
    StartsWith(ColumnRef, String),
    /// An array column that shares at least one element with the values.
    HasAny(ColumnRef, Vec<Literal>),
    /// A metadata entry `key = value`, matched on the table's `metadata_kv` column.
    MetadataEq {
        /// The metadata key.
        key: String,
        /// The metadata value.
        value: String,
    },
    /// A text match on the table's `name` column, served by its text index.
    TextMatch(String),
}

impl Pred {
    /// The predicate that keeps every row.
    pub const fn all() -> Self {
        Self::And(Vec::new())
    }

    /// `column <op> value`.
    pub fn cmp(column: impl Into<ColumnRef>, op: CmpOp, value: impl Into<Literal>) -> Self {
        Self::Cmp(column.into(), op, value.into())
    }

    /// `column = value`.
    pub fn eq(column: impl Into<ColumnRef>, value: impl Into<Literal>) -> Self {
        Self::cmp(column, CmpOp::Eq, value)
    }

    /// `column IN (values)`.
    pub fn is_in(column: impl Into<ColumnRef>, values: Vec<Literal>) -> Self {
        Self::In(column.into(), values)
    }

    /// Checks that the predicate is well-formed for `table_id`: every column belongs to the table and is not an
    /// aggregated measure, and every literal fits its column.
    pub(crate) fn check(&self, table_id: TableId) -> Result<(), PlanError> {
        let own = |column: &ColumnRef| {
            if column.table() != table_id {
                Err(PlanError::ForeignColumn {
                    table: table_id,
                    column: column.name(),
                })
            } else if column.ty().agg_fn().is_some() {
                Err(PlanError::UnsupportedPred(column.name()))
            } else {
                Ok(())
            }
        };
        let list = |column: &ColumnRef, element: &Ty, values: &[Literal]| {
            if values.is_empty() || values.len() > MAX_LIST {
                Err(PlanError::ListLength(column.name()))
            } else if values.iter().all(|v| v.fits(element)) {
                Ok(())
            } else {
                Err(PlanError::LiteralType(column.name()))
            }
        };
        match self {
            Self::And(operands) | Self::Or(operands) => {
                operands.iter().try_for_each(|p| p.check(table_id))
            }
            Self::Not(operand) => operand.check(table_id),
            Self::Cmp(column, _, value) => {
                own(column)?;
                if value.fits(column.ty()) {
                    Ok(())
                } else {
                    Err(PlanError::LiteralType(column.name()))
                }
            }
            Self::In(column, values) => {
                own(column)?;
                list(column, column.ty(), values)
            }
            Self::StartsWith(column, _) => {
                own(column)?;
                match column.ty().scalar() {
                    Ty::String | Ty::LowCardinalityString => Ok(()),
                    _ => Err(PlanError::UnsupportedPred(column.name())),
                }
            }
            Self::HasAny(column, values) => {
                own(column)?;
                match column.ty() {
                    Ty::Array(element) => list(column, element, values),
                    _ => Err(PlanError::UnsupportedPred(column.name())),
                }
            }
            Self::MetadataEq { .. } => match table(table_id).column("metadata_kv") {
                Some(_) => Ok(()),
                None => Err(PlanError::UnsupportedPred("metadata_kv")),
            },
            Self::TextMatch(_) => {
                let indexed = table(table_id)
                    .indexes
                    .iter()
                    .any(|i| i.column == "name" && matches!(i.kind, IndexKind::Text { .. }));
                if indexed {
                    Ok(())
                } else {
                    Err(PlanError::UnsupportedPred("name"))
                }
            }
        }
    }
}
