//! Rows as typed cells, generic over a [`TableDef`]: sorting by the ORDER BY key, combining rollup rows, the marker
//! row and the Native block (contract sections 5.2 and 5.5).
//!
//! A table needs only a function from its record to a row of cells in `TableDef` column order; everything else here
//! reads the `TableDef`.

use std::cmp::Ordering;

use crucible_chfmt::{
    ArrayColumn, Block, Column, Compression, EncodeError, EncodedBlock, MAX_BLOCK_ROWS,
    NullableColumn, StringColumn, TupleColumn, UuidColumn,
};
use crucible_records::{CivilDate, Lsn, PartitionId, Timestamp, WalEpoch};
use crucible_schema::{AggFn, PROJECT_ID, TableDef, TableKind, Ty};

/// One cell. The variant is fixed by the column's [`Ty`]:
///
/// | `Ty` | Cell |
/// |---|---|
/// | `UInt8` | `U8` |
/// | `UInt16`, `Date` (days) | `U16` |
/// | `UInt32`, `DateTime` (seconds) | `U32` |
/// | `UInt64` | `U64` |
/// | `Enum8` (stored value) | `I8` |
/// | `Int32` | `I32` |
/// | `Int64`, `DateTime64` (microseconds) | `I64` |
/// | `Float32`, `Float64` | `F32`, `F64` |
/// | `UUID` (RFC 9562 byte order) | `Uuid` |
/// | `String`, `LowCardinality(String)`, `FixedString(n)` | `Bytes` |
/// | `Nullable(T)` | `Null` or the cell of `T` |
/// | `Array(T)` | `Array` |
/// | `Tuple(A, B)` | `Tuple` |
/// | `SimpleAggregateFunction(f, T)` | the cell of `T` |
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    /// See the table above.
    U8(u8),
    /// See the table above.
    U16(u16),
    /// See the table above.
    U32(u32),
    /// See the table above.
    U64(u64),
    /// See the table above.
    I8(i8),
    /// See the table above.
    I32(i32),
    /// See the table above.
    I64(i64),
    /// See the table above.
    F32(f32),
    /// See the table above.
    F64(f64),
    /// See the table above.
    Uuid([u8; 16]),
    /// See the table above.
    Bytes(Box<[u8]>),
    /// A null of a `Nullable` column.
    Null,
    /// See the table above.
    Array(Vec<Value>),
    /// See the table above.
    Tuple(Box<(Value, Value)>),
}

impl Value {
    /// A string cell.
    pub fn str(text: &str) -> Self {
        Self::Bytes(text.as_bytes().into())
    }

    /// A string or fixed-string cell.
    pub fn bytes(bytes: &[u8]) -> Self {
        Self::Bytes(bytes.into())
    }

    /// An `Array(String)` cell.
    pub fn strs<S: AsRef<str>>(items: &[S]) -> Self {
        Self::Array(items.iter().map(|s| Self::str(s.as_ref())).collect())
    }

    /// A `DateTime64` cell.
    pub const fn time(t: Timestamp) -> Self {
        Self::I64(t.micros())
    }

    /// A `UInt8` flag cell.
    pub const fn flag(b: bool) -> Self {
        Self::U8(b as u8)
    }

    /// The estimated bytes the cell holds in memory and in an uncompressed block: the width of a fixed-size
    /// value, the length of a string plus its length prefix, and the sum over the elements of an array or a
    /// tuple.
    pub fn size(&self) -> u64 {
        match self {
            Self::Null | Self::U8(_) | Self::I8(_) => 1,
            Self::U16(_) => 2,
            Self::U32(_) | Self::I32(_) | Self::F32(_) => 4,
            Self::U64(_) | Self::I64(_) | Self::F64(_) => 8,
            Self::Uuid(_) => 16,
            Self::Bytes(bytes) => bytes.len() as u64 + 8,
            Self::Array(items) => 8 + items.iter().map(Self::size).sum::<u64>(),
            Self::Tuple(pair) => pair.0.size() + pair.1.size(),
        }
    }

    const fn rank(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::U8(_) => 1,
            Self::U16(_) => 2,
            Self::U32(_) => 3,
            Self::U64(_) => 4,
            Self::I8(_) => 5,
            Self::I32(_) => 6,
            Self::I64(_) => 7,
            Self::F32(_) => 8,
            Self::F64(_) => 9,
            Self::Uuid(_) => 10,
            Self::Bytes(_) => 11,
            Self::Array(_) => 12,
            Self::Tuple(_) => 13,
        }
    }

    /// The order of two cells of one column: numbers by value, strings and ids by their bytes.
    pub fn order(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::U8(a), Self::U8(b)) => a.cmp(b),
            (Self::U16(a), Self::U16(b)) => a.cmp(b),
            (Self::U32(a), Self::U32(b)) => a.cmp(b),
            (Self::U64(a), Self::U64(b)) => a.cmp(b),
            (Self::I8(a), Self::I8(b)) => a.cmp(b),
            (Self::I32(a), Self::I32(b)) => a.cmp(b),
            (Self::I64(a), Self::I64(b)) => a.cmp(b),
            (Self::F32(a), Self::F32(b)) => a.total_cmp(b),
            (Self::F64(a), Self::F64(b)) => a.total_cmp(b),
            (Self::Uuid(a), Self::Uuid(b)) => a.cmp(b),
            (Self::Bytes(a), Self::Bytes(b)) => a.cmp(b),
            (Self::Array(a), Self::Array(b)) => a
                .iter()
                .zip(b)
                .map(|(x, y)| x.order(y))
                .find(|o| o.is_ne())
                .unwrap_or_else(|| a.len().cmp(&b.len())),
            (Self::Tuple(a), Self::Tuple(b)) => a.0.order(&b.0).then_with(|| a.1.order(&b.1)),
            (a, b) => a.rank().cmp(&b.rank()),
        }
    }

    /// The type default of a column: zero, empty, nil, null. An enum without a 0 value takes its first variant
    /// (contract 6.7).
    pub fn default_of(ty: &Ty) -> Self {
        match ty {
            Ty::Uuid => Self::Uuid([0; 16]),
            Ty::UInt8 => Self::U8(0),
            Ty::UInt16 | Ty::Date => Self::U16(0),
            Ty::UInt32 | Ty::DateTime => Self::U32(0),
            Ty::UInt64 => Self::U64(0),
            Ty::Int32 => Self::I32(0),
            Ty::Int64 | Ty::DateTime64 => Self::I64(0),
            Ty::Float32 => Self::F32(0.0),
            Ty::Float64 => Self::F64(0.0),
            Ty::String | Ty::LowCardinalityString => Self::Bytes(Box::default()),
            Ty::FixedString(n) => Self::Bytes(vec![0; usize::from(*n)].into()),
            Ty::Enum8(kind) => {
                let variants = kind.variants();
                let first = variants.first().map_or(0, |v| v.1);
                let value = if variants.iter().any(|v| v.1 == 0) {
                    0
                } else {
                    first
                };
                Self::I8(i8::try_from(value).unwrap_or(0))
            }
            Ty::Nullable(_) => Self::Null,
            Ty::Array(_) => Self::Array(Vec::new()),
            Ty::Tuple2(a, b) => Self::Tuple(Box::new((Self::default_of(a), Self::default_of(b)))),
            Ty::Saf(_, inner) => Self::default_of(inner),
        }
    }

    /// Merges `other` into this cell of a `SimpleAggregateFunction(f, T)` column. Sums saturate. A bounded set
    /// keeps its smallest distinct values, as `crucible-records` merges them.
    fn merge(&mut self, other: Self, f: AggFn) {
        match f {
            AggFn::Min => {
                if other.order(self).is_lt() {
                    *self = other;
                }
            }
            AggFn::Max => {
                if other.order(self).is_gt() {
                    *self = other;
                }
            }
            AggFn::Sum => match (&mut *self, other) {
                (Self::U64(a), Self::U64(b)) => *a = a.saturating_add(b),
                (Self::I64(a), Self::I64(b)) => *a = a.saturating_add(b),
                _ => {}
            },
            AggFn::GroupUniqArrayArray(cap) => {
                if let (Self::Array(a), Self::Array(b)) = (&mut *self, other) {
                    a.extend(b);
                    a.sort_unstable_by(Self::order);
                    a.dedup();
                    a.truncate(usize::from(cap));
                }
            }
            AggFn::SumMap => {
                let (Self::Tuple(mine), Self::Tuple(theirs)) = (&mut *self, other) else {
                    return;
                };
                let (Self::Array(keys), Self::Array(counts)) = (&mut mine.0, &mut mine.1) else {
                    return;
                };
                let (Self::Array(other_keys), Self::Array(other_counts)) = (theirs.0, theirs.1)
                else {
                    return;
                };
                let mut entries: Vec<(Self, Self)> = std::mem::take(keys)
                    .into_iter()
                    .zip(std::mem::take(counts))
                    .chain(other_keys.into_iter().zip(other_counts))
                    .collect();
                entries.sort_by(|a, b| a.0.order(&b.0));
                for (key, count) in entries {
                    match (keys.last(), counts.last_mut()) {
                        (Some(last), Some(total)) if *last == key => {
                            total.merge(count, AggFn::Sum);
                        }
                        _ => {
                            keys.push(key);
                            counts.push(count);
                        }
                    }
                }
            }
        }
    }
}

/// One row of a table: its cells in `TableDef` column order, and where the row came from, which breaks ties in
/// the sort (contract 5.2).
#[derive(Clone, PartialEq, Debug)]
pub struct Row {
    /// One cell per column of the table.
    pub cells: Vec<Value>,
    /// LSN of the frame the row came from.
    pub lsn: Lsn,
    /// Position of the row's span in its frame.
    pub ordinal: u32,
}

impl Row {
    /// The estimated bytes of the row, which the `shipper.encoded_bytes` budget charges.
    pub fn size(&self) -> u64 {
        self.cells.iter().map(Value::size).sum()
    }
}

/// Why a block could not be built. Each is a defect of the row derivation, never of the data.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum BuildError {
    /// A row has another number of cells than the table has columns.
    #[error("table {table}: a row has {cells} cells for {columns} columns")]
    Width {
        /// The table.
        table: &'static str,
        /// Cells in the row.
        cells: usize,
        /// Columns of the table.
        columns: usize,
    },
    /// A cell's variant does not fit its column.
    #[error("table {table}: a cell does not fit column `{column}`")]
    Cell {
        /// The table.
        table: &'static str,
        /// The column.
        column: &'static str,
    },
    /// The table names a column in ORDER BY or PARTITION BY that it does not have.
    #[error("table {table}: no column `{column}`")]
    NoColumn {
        /// The table.
        table: &'static str,
        /// The missing column.
        column: &'static str,
    },
    /// More rows than one block may hold.
    #[error("table {table}: {rows} rows exceed one block")]
    TooManyRows {
        /// The table.
        table: &'static str,
        /// Rows of the pair.
        rows: usize,
    },
    /// The encoder refused the block.
    #[error("encoding failed: {0}")]
    Encode(#[from] EncodeError),
}

fn column_index(def: &TableDef, name: &'static str) -> Result<usize, BuildError> {
    def.columns
        .iter()
        .position(|c| c.name == name)
        .ok_or(BuildError::NoColumn {
            table: def.name(),
            column: name,
        })
}

fn key_indexes(def: &TableDef) -> Result<Vec<usize>, BuildError> {
    def.order_by
        .iter()
        .map(|name| column_index(def, name))
        .collect()
}

fn key_order(keys: &[usize], a: &Row, b: &Row) -> Ordering {
    keys.iter()
        .map(|&i| match (a.cells.get(i), b.cells.get(i)) {
            (Some(x), Some(y)) => x.order(y),
            _ => Ordering::Equal,
        })
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Sorts rows by the table's ORDER BY key, ties by LSN then span ordinal. On an aggregating table, rows with equal
/// keys are then combined by each column's merge function (contract 5.2).
pub fn sort_and_combine(def: &TableDef, rows: &mut Vec<Row>) -> Result<(), BuildError> {
    let columns = def.columns.len();
    if let Some(row) = rows.iter().find(|r| r.cells.len() != columns) {
        return Err(BuildError::Width {
            table: def.name(),
            cells: row.cells.len(),
            columns,
        });
    }
    let keys = key_indexes(def)?;
    rows.sort_by(|a, b| {
        key_order(&keys, a, b)
            .then_with(|| a.lsn.cmp(&b.lsn))
            .then_with(|| a.ordinal.cmp(&b.ordinal))
    });
    if def.kind != TableKind::Aggregating {
        return Ok(());
    }
    let mut combined: Vec<Row> = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        match combined.last_mut() {
            Some(last) if key_order(&keys, last, &row).is_eq() => {
                for ((mine, theirs), column) in
                    last.cells.iter_mut().zip(row.cells).zip(def.columns)
                {
                    if let Some(f) = column.ty.agg_fn() {
                        mine.merge(theirs, f);
                    }
                }
            }
            _ => combined.push(row),
        }
    }
    *rows = combined;
    Ok(())
}

/// The first instant of a partition, as the cell of a time column of type `ty`.
fn partition_start(ty: &Ty, partition: PartitionId) -> Value {
    let date = partition
        .as_date()
        .or_else(|| {
            partition
                .as_month()
                .and_then(|(year, month)| CivilDate::new(year, month, 1))
        })
        .unwrap_or_else(|| CivilDate::from_day_number(0));
    let start = date.start();
    match ty.scalar() {
        Ty::Date => Value::U16(u16::try_from(date.day_number()).unwrap_or(0)),
        Ty::DateTime => Value::U32(start.start_minute()),
        _ => Value::time(start),
    }
}

/// The marker row of one block of an aggregating table (contract 5.5): the nil project, type defaults in every
/// other key and measure column, the first instant of the partition in the time column, and the marker columns.
pub fn marker_row(
    def: &TableDef,
    partition: PartitionId,
    epoch: WalEpoch,
    batch_seq: Lsn,
    marker_at: Timestamp,
) -> Row {
    let time_column = def.partition.column();
    let cells = def
        .columns
        .iter()
        .map(|column| match column.name {
            PROJECT_ID => Value::Uuid([0; 16]),
            "marker_epoch" => Value::Uuid(epoch.to_bytes()),
            "marker_batch_seq" => Value::U64(batch_seq.get()),
            "marker_at" => {
                let seconds = marker_at.micros().div_euclid(1_000_000);
                Value::U32(u32::try_from(seconds).unwrap_or(0))
            }
            name if Some(name) == time_column => partition_start(&column.ty, partition),
            _ => Value::default_of(&column.ty),
        })
        .collect();
    Row {
        cells,
        lsn: Lsn::ZERO,
        ordinal: 0,
    }
}

/// A column under construction, one variant per wire layout of contract 6.4.
#[derive(Debug)]
enum ColumnBuf {
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
    I8(Vec<i8>),
    I64(Vec<i64>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Uuid(UuidColumn),
    Str(StringColumn),
    Fixed8(Vec<[u8; 8]>),
    Fixed16(Vec<[u8; 16]>),
    Fixed32(Vec<[u8; 32]>),
    NullU16(NullableColumn<u16>),
    NullU32(NullableColumn<u32>),
    NullI32(NullableColumn<i32>),
    NullI64(NullableColumn<i64>),
    ArrStr(ArrayColumn<StringColumn>),
    ArrU8(ArrayColumn<Vec<u8>>),
    ArrU32(ArrayColumn<Vec<u32>>),
    ArrU64(ArrayColumn<Vec<u64>>),
    ArrI64(ArrayColumn<Vec<i64>>),
    Hist16(TupleColumn<ArrayColumn<Vec<u16>>, ArrayColumn<Vec<u64>>>),
    Hist8(TupleColumn<ArrayColumn<Vec<u8>>, ArrayColumn<Vec<u64>>>),
}

/// Collects the elements of an array cell when every one has the expected variant.
fn elements<'a, T>(items: &'a [Value], pick: impl Fn(&'a Value) -> Option<T>) -> Option<Vec<T>> {
    items.iter().map(pick).collect()
}

fn as_u8(v: &Value) -> Option<u8> {
    match v {
        Value::U8(x) => Some(*x),
        _ => None,
    }
}

fn as_u16(v: &Value) -> Option<u16> {
    match v {
        Value::U16(x) => Some(*x),
        _ => None,
    }
}

fn as_u32(v: &Value) -> Option<u32> {
    match v {
        Value::U32(x) => Some(*x),
        _ => None,
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::U64(x) => Some(*x),
        _ => None,
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::I64(x) => Some(*x),
        _ => None,
    }
}

fn as_bytes(v: &Value) -> Option<&[u8]> {
    match v {
        Value::Bytes(x) => Some(x),
        _ => None,
    }
}

fn as_array(v: &Value) -> Option<&[Value]> {
    match v {
        Value::Array(x) => Some(x),
        _ => None,
    }
}

fn push_fixed<const N: usize>(column: &mut Vec<[u8; N]>, value: &Value) -> Option<()> {
    column.push(as_bytes(value)?.try_into().ok()?);
    Some(())
}

fn push_nullable<T: crucible_chfmt::Number>(
    column: &mut NullableColumn<T>,
    value: &Value,
    pick: impl Fn(&Value) -> Option<T>,
) -> Option<()> {
    match value {
        Value::Null => column.push(None),
        other => column.push(Some(pick(other)?)),
    }
    Some(())
}

/// Pushes one histogram cell: a tuple of a key array and a count array of equal lengths.
fn push_hist<K: crucible_chfmt::Number>(
    column: &mut TupleColumn<ArrayColumn<Vec<K>>, ArrayColumn<Vec<u64>>>,
    value: &Value,
    pick: impl Fn(&Value) -> Option<K>,
) -> Option<()> {
    let Value::Tuple(pair) = value else {
        return None;
    };
    let keys = elements(as_array(&pair.0)?, pick)?;
    let counts = elements(as_array(&pair.1)?, as_u64)?;
    if keys.len() != counts.len() {
        return None;
    }
    column.first_mut().push(keys);
    column.second_mut().push(counts);
    Some(())
}

impl ColumnBuf {
    fn for_ty(ty: &Ty) -> Option<Self> {
        Some(match ty {
            Ty::Saf(_, inner) => return Self::for_ty(inner),
            Ty::Uuid => Self::Uuid(UuidColumn::default()),
            Ty::UInt8 => Self::U8(Vec::new()),
            Ty::UInt16 | Ty::Date => Self::U16(Vec::new()),
            Ty::UInt32 | Ty::DateTime => Self::U32(Vec::new()),
            Ty::UInt64 => Self::U64(Vec::new()),
            Ty::Int64 | Ty::DateTime64 => Self::I64(Vec::new()),
            Ty::Float32 => Self::F32(Vec::new()),
            Ty::Float64 => Self::F64(Vec::new()),
            Ty::String | Ty::LowCardinalityString => Self::Str(StringColumn::default()),
            Ty::FixedString(8) => Self::Fixed8(Vec::new()),
            Ty::FixedString(16) => Self::Fixed16(Vec::new()),
            Ty::FixedString(32) => Self::Fixed32(Vec::new()),
            Ty::Enum8(_) => Self::I8(Vec::new()),
            Ty::Nullable(inner) => match inner {
                Ty::UInt16 => Self::NullU16(NullableColumn::default()),
                Ty::UInt32 => Self::NullU32(NullableColumn::default()),
                Ty::Int32 => Self::NullI32(NullableColumn::default()),
                Ty::Int64 | Ty::DateTime64 => Self::NullI64(NullableColumn::default()),
                _ => return None,
            },
            Ty::Array(inner) => match inner {
                Ty::String | Ty::LowCardinalityString => {
                    Self::ArrStr(ArrayColumn::new(StringColumn::default()))
                }
                Ty::UInt8 => Self::ArrU8(ArrayColumn::new(Vec::new())),
                Ty::UInt32 => Self::ArrU32(ArrayColumn::new(Vec::new())),
                Ty::UInt64 => Self::ArrU64(ArrayColumn::new(Vec::new())),
                Ty::Int64 => Self::ArrI64(ArrayColumn::new(Vec::new())),
                _ => return None,
            },
            Ty::Tuple2(Ty::Array(Ty::UInt16), Ty::Array(Ty::UInt64)) => Self::Hist16(
                TupleColumn::new(ArrayColumn::new(Vec::new()), ArrayColumn::new(Vec::new())),
            ),
            Ty::Tuple2(Ty::Array(Ty::UInt8), Ty::Array(Ty::UInt64)) => Self::Hist8(
                TupleColumn::new(ArrayColumn::new(Vec::new()), ArrayColumn::new(Vec::new())),
            ),
            Ty::FixedString(_) | Ty::Int32 | Ty::Tuple2(..) => return None,
        })
    }

    fn push(&mut self, value: &Value) -> Option<()> {
        match (self, value) {
            (Self::U8(c), Value::U8(v)) => c.push(*v),
            (Self::U16(c), Value::U16(v)) => c.push(*v),
            (Self::U32(c), Value::U32(v)) => c.push(*v),
            (Self::U64(c), Value::U64(v)) => c.push(*v),
            (Self::I8(c), Value::I8(v)) => c.push(*v),
            (Self::I64(c), Value::I64(v)) => c.push(*v),
            (Self::F32(c), Value::F32(v)) => c.push(*v),
            (Self::F64(c), Value::F64(v)) => c.push(*v),
            (Self::Uuid(c), Value::Uuid(v)) => c.push(*v),
            (Self::Str(c), Value::Bytes(v)) => c.push(v),
            (Self::Fixed8(c), v) => return push_fixed(c, v),
            (Self::Fixed16(c), v) => return push_fixed(c, v),
            (Self::Fixed32(c), v) => return push_fixed(c, v),
            (Self::NullU16(c), v) => return push_nullable(c, v, as_u16),
            (Self::NullU32(c), v) => return push_nullable(c, v, as_u32),
            (Self::NullI32(c), v) => {
                return push_nullable(c, v, |v| match v {
                    Value::I32(x) => Some(*x),
                    _ => None,
                });
            }
            (Self::NullI64(c), v) => return push_nullable(c, v, as_i64),
            (Self::ArrStr(c), Value::Array(v)) => c.push(elements(v, as_bytes)?),
            (Self::ArrU8(c), Value::Array(v)) => c.push(elements(v, as_u8)?),
            (Self::ArrU32(c), Value::Array(v)) => c.push(elements(v, as_u32)?),
            (Self::ArrU64(c), Value::Array(v)) => c.push(elements(v, as_u64)?),
            (Self::ArrI64(c), Value::Array(v)) => c.push(elements(v, as_i64)?),
            (Self::Hist16(c), v) => return push_hist(c, v, as_u16),
            (Self::Hist8(c), v) => return push_hist(c, v, as_u8),
            _ => return None,
        }
        Some(())
    }

    fn column(&self) -> &dyn Column {
        match self {
            Self::U8(c) => c,
            Self::U16(c) => c,
            Self::U32(c) => c,
            Self::U64(c) => c,
            Self::I8(c) => c,
            Self::I64(c) => c,
            Self::F32(c) => c,
            Self::F64(c) => c,
            Self::Uuid(c) => c,
            Self::Str(c) => c,
            Self::Fixed8(c) => c,
            Self::Fixed16(c) => c,
            Self::Fixed32(c) => c,
            Self::NullU16(c) => c,
            Self::NullU32(c) => c,
            Self::NullI32(c) => c,
            Self::NullI64(c) => c,
            Self::ArrStr(c) => c,
            Self::ArrU8(c) => c,
            Self::ArrU32(c) => c,
            Self::ArrU64(c) => c,
            Self::ArrI64(c) => c,
            Self::Hist16(c) => c,
            Self::Hist8(c) => c,
        }
    }
}

/// Encodes rows, in the order given, as one compressed Native block with every column of the table in `TableDef`
/// order (contract 5.2).
pub fn encode_block(
    def: &TableDef,
    rows: &[Row],
    compression: Compression,
) -> Result<EncodedBlock, BuildError> {
    let table = def.name();
    if rows.len() > MAX_BLOCK_ROWS {
        return Err(BuildError::TooManyRows {
            table,
            rows: rows.len(),
        });
    }
    let mut built = Vec::with_capacity(def.columns.len());
    for (index, column) in def.columns.iter().enumerate() {
        let cell_error = BuildError::Cell {
            table,
            column: column.name,
        };
        let mut buf = ColumnBuf::for_ty(&column.ty).ok_or_else(|| cell_error.clone())?;
        for row in rows {
            let cell = row.cells.get(index).ok_or(BuildError::Width {
                table,
                cells: row.cells.len(),
                columns: def.columns.len(),
            })?;
            buf.push(cell).ok_or_else(|| cell_error.clone())?;
        }
        built.push((column.name, column.ty.to_string(), buf));
    }
    let mut block = Block::new(rows.len())?;
    for (name, ty, buf) in &built {
        block.push(name, ty, buf.column())?;
    }
    Ok(block.encode(compression)?)
}
