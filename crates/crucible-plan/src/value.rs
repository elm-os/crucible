//! Typed results: the values, rows and row blocks a store returns for a plan, and the output schema every row of
//! a plan follows (contract section 8.1).
//!
//! [`crate::QueryPlan::output`] is the single definition of a plan's result shape. The ClickHouse adapter and the
//! simulation model must both satisfy it, and [`OutputSchema::check_block`] is the one validator for both.
//!
//! # Result types
//!
//! An output column's type is the *result type* of what the plan reads, never a storage wrapper: a
//! `SimpleAggregateFunction(f, T)` column or measure is described as `T`, and `LowCardinality(String)` as
//! `String`. So no output type contains `SimpleAggregateFunction` or `LowCardinality`.
//!
//! # One representation per datum
//!
//! | Result type | [`Value`] |
//! |---|---|
//! | `UInt8`, `UInt16`, `UInt32`, `UInt64` | `UInt`, within the type's width |
//! | `Int32`, `Int64` | `Int`, within the type's width |
//! | `Float32`, `Float64` | `Float`; for `Float32` the widened single-precision value |
//! | `String` | `String` when the bytes are valid UTF-8, otherwise `Bytes` |
//! | `FixedString(n)` | `Bytes` of length `n` |
//! | `UUID` | `Uuid` |
//! | `Date` | `Date` |
//! | `DateTime('UTC')` | `Time` on a whole second |
//! | `DateTime64(6, 'UTC')` | `Time` |
//! | `Enum8(...)` | `Enum` holding the stored value |
//! | `Nullable(T)` | `Null`, or the value of `T` |
//! | `Array(T)` | `Array` of values of `T` |
//! | `Tuple(A, B)` | `Tuple2` |
//!
//! # Row blocks
//!
//! A result is a sequence of [`RowBlock`]s. Every block holds between 1 and [`MAX_ROWS_PER_BLOCK`] rows; the
//! result is the concatenation of the blocks in arrival order; an empty result is a sequence of no blocks. Block
//! boundaries carry no meaning: two stores may cut the same result differently, and callers must not depend on
//! where a block ends.

use crucible_records::{CivilDate, Timestamp, Uuid};
use crucible_schema::{ColumnRef, Ty};

use crate::node::{Aggregate, Measure, Node};

/// The most rows one [`RowBlock`] holds.
pub const MAX_ROWS_PER_BLOCK: usize = 65_536;

/// One value of a result row. The module documentation gives the one representation of each result type.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    /// The null of a `Nullable` type.
    Null,
    /// An unsigned integer.
    UInt(u64),
    /// A signed integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A `String` whose bytes are valid UTF-8.
    String(String),
    /// A `FixedString(n)`, or a `String` whose bytes are not valid UTF-8 (a byte range may cut a character).
    Bytes(Vec<u8>),
    /// A `UUID`.
    Uuid(Uuid),
    /// A `Date`.
    Date(CivilDate),
    /// A `DateTime('UTC')` or `DateTime64(6, 'UTC')` instant.
    Time(Timestamp),
    /// The stored value of an `Enum8`.
    Enum(u8),
    /// An `Array(T)`.
    Array(Vec<Value>),
    /// A `Tuple(A, B)`.
    Tuple2(Box<Value>, Box<Value>),
}

impl Value {
    /// The value of a `String` result from its bytes: `String` when they are valid UTF-8, `Bytes` otherwise.
    pub fn string(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::String(text),
            Err(invalid) => Self::Bytes(invalid.into_bytes()),
        }
    }

    /// Whether this is the representation of a value of type `ty`. Storage wrappers are looked through, so the
    /// check also holds against a column's `TableDef` type.
    pub fn fits(&self, ty: &Ty) -> bool {
        match (self, ty) {
            (_, Ty::Saf(_, inner)) => self.fits(inner),
            (Self::Null, Ty::Nullable(_)) => true,
            (_, Ty::Nullable(inner)) => self.fits(inner),
            (Self::UInt(_), Ty::UInt64)
            | (Self::Int(_), Ty::Int64)
            | (Self::Float(_), Ty::Float64) => true,
            (Self::UInt(v), Ty::UInt8) => u8::try_from(*v).is_ok(),
            (Self::UInt(v), Ty::UInt16) => u16::try_from(*v).is_ok(),
            (Self::UInt(v), Ty::UInt32) => u32::try_from(*v).is_ok(),
            (Self::Int(v), Ty::Int32) => i32::try_from(*v).is_ok(),
            // The narrowing cast is the test: a `Float32` result is the widening of a single-precision value.
            (Self::Float(v), Ty::Float32) => v.is_nan() || f64::from(*v as f32) == *v,
            (Self::String(_), Ty::String | Ty::LowCardinalityString) => true,
            (Self::Bytes(b), Ty::String | Ty::LowCardinalityString) => {
                core::str::from_utf8(b).is_err()
            }
            (Self::Bytes(b), Ty::FixedString(n)) => b.len() == usize::from(*n),
            (Self::Uuid(_), Ty::Uuid)
            | (Self::Date(_), Ty::Date)
            | (Self::Time(_), Ty::DateTime64) => true,
            (Self::Time(t), Ty::DateTime) => t.micros().rem_euclid(1_000_000) == 0,
            (Self::Enum(v), Ty::Enum8(kind)) => kind.variants().iter().any(|(_, x)| x == v),
            (Self::Array(items), Ty::Array(element)) => items.iter().all(|v| v.fits(element)),
            (Self::Tuple2(a, b), Ty::Tuple2(ta, tb)) => a.fits(ta) && b.fits(tb),
            _ => false,
        }
    }
}

/// One result row: one [`Value`] per output column, in output order.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Row(Vec<Value>);

impl Row {
    /// A row of these values.
    pub const fn new(values: Vec<Value>) -> Self {
        Self(values)
    }

    /// The values, in output order.
    pub fn values(&self) -> &[Value] {
        &self.0
    }

    /// The values, in output order.
    pub fn into_values(self) -> Vec<Value> {
        self.0
    }
}

impl From<Vec<Value>> for Row {
    fn from(values: Vec<Value>) -> Self {
        Self(values)
    }
}

/// Between 1 and [`MAX_ROWS_PER_BLOCK`] consecutive rows of a result.
#[derive(Clone, PartialEq, Debug)]
pub struct RowBlock(Vec<Row>);

impl RowBlock {
    /// A block of these rows; `None` when there are none or more than [`MAX_ROWS_PER_BLOCK`].
    pub fn new(rows: Vec<Row>) -> Option<Self> {
        (1..=MAX_ROWS_PER_BLOCK)
            .contains(&rows.len())
            .then_some(Self(rows))
    }

    /// Cuts a whole result into blocks, in order. An empty result gives no block.
    pub fn split(rows: Vec<Row>) -> Vec<Self> {
        let mut blocks = Vec::with_capacity(rows.len().div_ceil(MAX_ROWS_PER_BLOCK));
        let mut rows = rows.into_iter().peekable();
        while rows.peek().is_some() {
            blocks.push(Self(rows.by_ref().take(MAX_ROWS_PER_BLOCK).collect()));
        }
        blocks
    }

    /// The rows.
    pub fn rows(&self) -> &[Row] {
        &self.0
    }

    /// The rows.
    pub fn into_rows(self) -> Vec<Row> {
        self.0
    }
}

/// One output column of a plan.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct OutputColumn {
    /// A label. Columns are identified by position; two measures over one column may share a label.
    pub name: String,
    /// The result type (see the module documentation).
    pub ty: Ty,
}

/// How a row or block departs from an [`OutputSchema`].
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ShapeError {
    /// The row has the wrong number of values.
    #[error("a row has {actual} values, the plan has {expected} output columns")]
    Arity {
        /// The number of output columns.
        expected: usize,
        /// The number of values in the row.
        actual: usize,
    },
    /// A value is not the representation of its column's type.
    #[error("the value of output column {index} ({name}) does not fit its type")]
    Type {
        /// The position of the column.
        index: usize,
        /// The label of the column.
        name: String,
    },
}

/// The ordered output columns of a plan: the shape of every row any store returns for it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct OutputSchema {
    columns: Vec<OutputColumn>,
}

impl OutputSchema {
    /// The columns, in output order.
    pub fn columns(&self) -> &[OutputColumn] {
        &self.columns
    }

    /// Checks one row: one value per column, each the representation of its column's type.
    pub fn check_row(&self, row: &Row) -> Result<(), ShapeError> {
        let values = row.values();
        if values.len() != self.columns.len() {
            return Err(ShapeError::Arity {
                expected: self.columns.len(),
                actual: values.len(),
            });
        }
        match self
            .columns
            .iter()
            .zip(values)
            .position(|(column, value)| !value.fits(&column.ty))
        {
            Some(index) => Err(ShapeError::Type {
                index,
                name: self.columns[index].name.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Checks every row of a block.
    pub fn check_block(&self, block: &RowBlock) -> Result<(), ShapeError> {
        block.rows().iter().try_for_each(|row| self.check_row(row))
    }
}

/// The result type of a stored type: no `SimpleAggregateFunction` and no `LowCardinality`. `Ty` holds static
/// references, so the rewrite covers the nestings the schema uses; a unit test walks every column to prove it.
fn result_ty(ty: &Ty) -> Ty {
    match ty {
        Ty::Saf(_, inner) => result_ty(inner),
        Ty::LowCardinalityString => Ty::String,
        Ty::Array(Ty::LowCardinalityString) => Ty::Array(&Ty::String),
        other => *other,
    }
}

fn column(column: &ColumnRef) -> OutputColumn {
    OutputColumn {
        name: column.name().to_owned(),
        ty: result_ty(column.ty()),
    }
}

fn columns(refs: &[ColumnRef]) -> Vec<OutputColumn> {
    refs.iter().map(column).collect()
}

fn measure(measure: &Measure) -> OutputColumn {
    let named = |function: &str, c: &ColumnRef, ty: Ty| OutputColumn {
        name: format!("{function}_{}", c.name()),
        ty,
    };
    let count = |name: String| OutputColumn {
        name,
        ty: Ty::UInt64,
    };
    match measure {
        Measure::Count => count("count".to_owned()),
        Measure::Uniq(c) => count(format!("uniq_{}", c.name())),
        Measure::Sum(c) => named("sum", c, sum_ty(c.ty())),
        Measure::Min(c) => named("min", c, result_ty(c.ty())),
        Measure::Max(c) => named("max", c, result_ty(c.ty())),
        Measure::HistogramMerge(c) => named("histogram", c, result_ty(c.ty())),
        Measure::PickMerge(c) => named("pick", c, result_ty(c.ty())),
        Measure::LatestByVersion(c) => named("latest", c, result_ty(c.ty())),
    }
}

/// A sum widens to 64 bits and keeps the signedness and nullability of its column.
fn sum_ty(ty: &Ty) -> Ty {
    let nullable = matches!(result_ty(ty), Ty::Nullable(_));
    match (ty.scalar(), nullable) {
        (Ty::Int32 | Ty::Int64, false) => Ty::Int64,
        (Ty::Int32 | Ty::Int64, true) => Ty::Nullable(&Ty::Int64),
        (Ty::Float32 | Ty::Float64, false) => Ty::Float64,
        (Ty::Float32 | Ty::Float64, true) => Ty::Nullable(&Ty::Float64),
        (_, false) => Ty::UInt64,
        (_, true) => Ty::Nullable(&Ty::UInt64),
    }
}

fn aggregate(aggregate: &Aggregate) -> Vec<OutputColumn> {
    let mut out = columns(aggregate.group());
    out.extend(aggregate.measures().iter().map(measure));
    out
}

/// The output schema of a node. Total over every node kind.
pub(crate) fn output(node: &Node) -> OutputSchema {
    let columns = match node {
        Node::Scan(n) => columns(n.columns()),
        Node::PointLookup(n) => columns(n.columns()),
        Node::Aggregate(n) => aggregate(n),
        Node::SemiJoin(n) => columns(n.left().columns()),
        Node::TextSearch(n) => columns(&crucible_schema::table(n.table()).key_columns()),
        Node::TimeSeries(n) => {
            let mut out = vec![OutputColumn {
                name: "bucket".to_owned(),
                ty: Ty::DateTime,
            }];
            out.extend(aggregate(n.aggregate()));
            out
        }
        Node::FieldRange(n) => vec![column(&n.field())],
    };
    OutputSchema { columns }
}

#[cfg(test)]
mod tests {
    use crucible_schema::TABLES;

    use super::*;

    fn is_result_ty(ty: &Ty) -> bool {
        match ty {
            Ty::Saf(..) | Ty::LowCardinalityString => false,
            Ty::Nullable(inner) | Ty::Array(inner) => is_result_ty(inner),
            Ty::Tuple2(a, b) => is_result_ty(a) && is_result_ty(b),
            _ => true,
        }
    }

    #[test]
    fn result_types_of_every_schema_column_carry_no_storage_wrapper() {
        for def in &TABLES {
            for col in def.columns {
                let ty = result_ty(&col.ty);
                assert!(is_result_ty(&ty), "{}.{}: {ty}", def.name(), col.name);
                assert_eq!(ty.scalar(), &result_ty(col.ty.scalar()), "{}", col.name);
                assert!(is_result_ty(&sum_ty(&col.ty)));
            }
        }
    }

    #[test]
    fn sums_widen_and_keep_sign_and_nullability() {
        assert_eq!(sum_ty(&Ty::UInt8), Ty::UInt64);
        assert_eq!(
            sum_ty(&Ty::Saf(crucible_schema::AggFn::Sum, &Ty::Int64)),
            Ty::Int64
        );
        assert_eq!(sum_ty(&Ty::Float32), Ty::Float64);
        assert_eq!(
            sum_ty(&Ty::Nullable(&Ty::UInt32)),
            Ty::Nullable(&Ty::UInt64)
        );
        assert_eq!(sum_ty(&Ty::Nullable(&Ty::Int32)), Ty::Nullable(&Ty::Int64));
    }

    #[test]
    fn each_datum_has_one_representation() {
        assert_eq!(
            Value::string(b"ok".to_vec()),
            Value::String("ok".to_owned())
        );
        assert_eq!(
            Value::string(vec![0xff, 0x61]),
            Value::Bytes(vec![0xff, 0x61])
        );
        // Valid UTF-8 carried as bytes is not the representation of a `String`.
        assert!(!Value::Bytes(b"ok".to_vec()).fits(&Ty::String));
        assert!(Value::Bytes(vec![0xff]).fits(&Ty::LowCardinalityString));
        assert!(Value::Bytes(vec![0; 8]).fits(&Ty::FixedString(8)));
        assert!(!Value::Bytes(vec![0; 7]).fits(&Ty::FixedString(8)));
        assert!(Value::UInt(255).fits(&Ty::UInt8) && !Value::UInt(256).fits(&Ty::UInt8));
        assert!(!Value::Int(1).fits(&Ty::UInt64) && !Value::UInt(1).fits(&Ty::Int64));
        assert!(Value::Float(0.5).fits(&Ty::Float32) && !Value::Float(0.1).fits(&Ty::Float32));
        assert!(Value::Float(f64::NAN).fits(&Ty::Float32) && Value::Float(0.1).fits(&Ty::Float64));
        let second = Value::Time(Timestamp::from_micros(1_000_000));
        let micro = Value::Time(Timestamp::from_micros(1));
        assert!(second.fits(&Ty::DateTime) && !micro.fits(&Ty::DateTime));
        assert!(micro.fits(&Ty::DateTime64));
        assert!(Value::Null.fits(&Ty::Nullable(&Ty::Int32)) && !Value::Null.fits(&Ty::Int32));
        assert!(Value::Int(-1).fits(&Ty::Nullable(&Ty::Int32)));
        let status = Ty::Enum8(crucible_schema::EnumKind::StatusCode);
        assert!(Value::Enum(2).fits(&status) && !Value::Enum(3).fits(&status));
        let histogram = Value::Tuple2(
            Box::new(Value::Array(vec![Value::UInt(3)])),
            Box::new(Value::Array(vec![Value::UInt(9)])),
        );
        assert!(
            histogram.fits(
                &crucible_schema::table(crucible_records::TableId::SpanMetrics)
                    .columns
                    .iter()
                    .find(|c| c.ty.agg_fn() == Some(crucible_schema::AggFn::SumMap))
                    .unwrap()
                    .ty
            )
        );
        assert!(!Value::Array(vec![Value::Int(1)]).fits(&Ty::Array(&Ty::UInt64)));
    }

    #[test]
    fn blocks_hold_between_one_row_and_the_cap() {
        assert_eq!(RowBlock::new(Vec::new()), None);
        assert_eq!(
            RowBlock::new(vec![Row::default(); MAX_ROWS_PER_BLOCK + 1]),
            None
        );
        assert!(RowBlock::split(Vec::new()).is_empty());
        let rows: Vec<Row> = (0..MAX_ROWS_PER_BLOCK as u64 + 2)
            .map(|i| Row::new(vec![Value::UInt(i)]))
            .collect();
        let blocks = RowBlock::split(rows.clone());
        let sizes: Vec<usize> = blocks.iter().map(|b| b.rows().len()).collect();
        assert_eq!(sizes, [MAX_ROWS_PER_BLOCK, 2]);
        let joined: Vec<Row> = blocks.into_iter().flat_map(RowBlock::into_rows).collect();
        assert_eq!(joined, rows);
    }
}
