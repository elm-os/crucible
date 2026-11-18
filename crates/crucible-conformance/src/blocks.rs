//! Native blocks for any Crucible table, built from the `TableDef`: every column takes its type's default
//! unless a test sets it. The conformance rows insert these through the production `insert_block`.

use anyhow::{Context, Result, bail};
use crucible_chfmt::{
    ArrayColumn, Block, Column, Compression, EncodedBlock, NullableColumn, StringColumn,
    TupleColumn, UuidColumn,
};
use crucible_plan::Value;
use crucible_records::{CivilDate, TableId, Timestamp, Uuid};
use crucible_schema::{Ty, table};

/// The default of a column type: zero, empty, the epoch, the first enum variant, or null.
pub fn default_value(ty: &Ty) -> Value {
    match ty {
        Ty::Uuid => Value::Uuid(Uuid::NIL),
        Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64 => Value::UInt(0),
        Ty::Int32 | Ty::Int64 => Value::Int(0),
        Ty::Float32 | Ty::Float64 => Value::Float(0.0),
        Ty::String | Ty::LowCardinalityString => Value::String(String::new()),
        Ty::FixedString(n) => Value::Bytes(vec![0; usize::from(*n)]),
        Ty::Date => Value::Date(CivilDate::from_day_number(0)),
        Ty::DateTime | Ty::DateTime64 => Value::Time(Timestamp::EPOCH),
        Ty::Enum8(kind) => Value::Enum(kind.variants().first().map_or(0, |(_, value)| *value)),
        Ty::Nullable(_) => Value::Null,
        Ty::Array(_) => Value::Array(Vec::new()),
        Ty::Tuple2(a, b) => Value::Tuple2(Box::new(default_value(a)), Box::new(default_value(b))),
        Ty::Saf(_, inner) => default_value(inner),
    }
}

fn uint(value: &Value) -> Result<u64> {
    match value {
        Value::UInt(v) => Ok(*v),
        other => bail!("expected an unsigned integer, got {other:?}"),
    }
}

fn int(value: &Value) -> Result<i64> {
    match value {
        Value::Int(v) => Ok(*v),
        other => bail!("expected a signed integer, got {other:?}"),
    }
}

fn time(value: &Value) -> Result<i64> {
    match value {
        Value::Time(t) => Ok(t.micros()),
        other => bail!("expected a time, got {other:?}"),
    }
}

fn bytes(value: &Value) -> Result<&[u8]> {
    match value {
        Value::String(s) => Ok(s.as_bytes()),
        Value::Bytes(b) => Ok(b),
        other => bail!("expected a string, got {other:?}"),
    }
}

fn items(value: &Value) -> Result<&[Value]> {
    match value {
        Value::Array(items) => Ok(items),
        other => bail!("expected an array, got {other:?}"),
    }
}

fn numbers<T, F>(values: &[Value], convert: F) -> Result<Vec<T>>
where
    F: Fn(&Value) -> Result<T>,
{
    values.iter().map(convert).collect()
}

fn narrow<T: TryFrom<u64>>(value: &Value) -> Result<T> {
    T::try_from(uint(value)?)
        .ok()
        .context("an unsigned value out of range")
}

fn fixed<const N: usize>(values: &[Value]) -> Result<Box<dyn Column>> {
    let rows: Vec<[u8; N]> = values
        .iter()
        .map(|v| <[u8; N]>::try_from(bytes(v)?).context("a fixed string of the wrong length"))
        .collect::<Result<_>>()?;
    Ok(Box::new(rows))
}

fn nullable<T, F>(values: &[Value], convert: F) -> Result<Box<dyn Column>>
where
    T: crucible_chfmt::Number + 'static,
    F: Fn(&Value) -> Result<T>,
{
    let mut column = NullableColumn::with_capacity(values.len());
    for value in values {
        column.push(match value {
            Value::Null => None,
            other => Some(convert(other)?),
        });
    }
    Ok(Box::new(column))
}

fn number_array<T, F>(values: &[Value], convert: F) -> Result<ArrayColumn<Vec<T>>>
where
    T: crucible_chfmt::Number + 'static,
    F: Fn(&Value) -> Result<T>,
{
    let mut column = ArrayColumn::new(Vec::new());
    for value in values {
        column.push(numbers(items(value)?, &convert)?);
    }
    Ok(column)
}

fn array(element: &Ty, values: &[Value]) -> Result<Box<dyn Column>> {
    Ok(match element {
        Ty::String | Ty::LowCardinalityString => {
            let mut column = ArrayColumn::new(StringColumn::default());
            for value in values {
                let strings: Vec<&[u8]> = items(value)?.iter().map(bytes).collect::<Result<_>>()?;
                column.push(strings);
            }
            Box::new(column)
        }
        Ty::UInt8 => Box::new(number_array(values, narrow::<u8>)?),
        Ty::UInt16 => Box::new(number_array(values, narrow::<u16>)?),
        Ty::UInt32 => Box::new(number_array(values, narrow::<u32>)?),
        Ty::UInt64 => Box::new(number_array(values, uint)?),
        Ty::Int64 => Box::new(number_array(values, int)?),
        other => bail!("no array builder for {other}"),
    })
}

fn halves(values: &[Value]) -> Result<(Vec<Value>, Vec<Value>)> {
    values
        .iter()
        .map(|value| match value {
            Value::Tuple2(a, b) => Ok(((**a).clone(), (**b).clone())),
            other => bail!("expected a tuple, got {other:?}"),
        })
        .collect::<Result<Vec<_>>>()
        .map(|pairs| pairs.into_iter().unzip())
}

/// The chfmt column of `values`, one per row, for a column of type `ty`.
fn column(ty: &Ty, values: &[Value]) -> Result<Box<dyn Column>> {
    Ok(match ty {
        Ty::Saf(_, inner) => return column(inner, values),
        Ty::Uuid => {
            let mut c = UuidColumn::with_capacity(values.len());
            for value in values {
                match value {
                    Value::Uuid(u) => c.push(u.to_bytes()),
                    other => bail!("expected a uuid, got {other:?}"),
                }
            }
            Box::new(c)
        }
        Ty::UInt8 => Box::new(numbers(values, narrow::<u8>)?),
        Ty::UInt16 => Box::new(numbers(values, narrow::<u16>)?),
        Ty::UInt32 => Box::new(numbers(values, narrow::<u32>)?),
        Ty::UInt64 => Box::new(numbers(values, uint)?),
        Ty::Int64 => Box::new(numbers(values, int)?),
        Ty::Float32 | Ty::Float64 => {
            let floats = numbers(values, |v| match v {
                Value::Float(f) => Ok(*f),
                other => bail!("expected a float, got {other:?}"),
            })?;
            if *ty == Ty::Float32 {
                Box::new(floats.into_iter().map(|f| f as f32).collect::<Vec<_>>())
            } else {
                Box::new(floats)
            }
        }
        Ty::String | Ty::LowCardinalityString => {
            let mut c = StringColumn::default();
            for value in values {
                c.push(bytes(value)?);
            }
            Box::new(c)
        }
        Ty::FixedString(8) => fixed::<8>(values)?,
        Ty::FixedString(16) => fixed::<16>(values)?,
        Ty::FixedString(32) => fixed::<32>(values)?,
        Ty::Date => Box::new(numbers(values, |v| match v {
            Value::Date(d) => u16::try_from(d.day_number()).context("a date out of range"),
            other => bail!("expected a date, got {other:?}"),
        })?),
        Ty::DateTime => Box::new(numbers(values, |v| {
            u32::try_from(time(v)? / 1_000_000).context("a time out of range")
        })?),
        Ty::DateTime64 => Box::new(numbers(values, time)?),
        Ty::Enum8(_) => Box::new(numbers(values, |v| match v {
            Value::Enum(e) => Ok(*e as i8),
            other => bail!("expected an enum value, got {other:?}"),
        })?),
        Ty::Nullable(Ty::DateTime64) => nullable(values, time)?,
        Ty::Nullable(Ty::UInt32) => nullable(values, narrow::<u32>)?,
        Ty::Nullable(Ty::UInt16) => nullable(values, narrow::<u16>)?,
        Ty::Nullable(Ty::Int32) => nullable(values, |v| {
            i32::try_from(int(v)?).context("a signed value out of range")
        })?,
        Ty::Array(element) => array(element, values)?,
        Ty::Tuple2(Ty::Array(Ty::UInt16), Ty::Array(Ty::UInt64)) => {
            let (keys, counts) = halves(values)?;
            Box::new(TupleColumn::new(
                number_array(&keys, narrow::<u16>)?,
                number_array(&counts, uint)?,
            ))
        }
        Ty::Tuple2(Ty::Array(Ty::UInt8), Ty::Array(Ty::UInt64)) => {
            let (keys, counts) = halves(values)?;
            Box::new(TupleColumn::new(
                number_array(&keys, narrow::<u8>)?,
                number_array(&counts, uint)?,
            ))
        }
        other => bail!("no column builder for {other}"),
    })
}

/// The rows of one block for one table.
#[derive(Clone, Debug)]
pub struct Rows {
    table: TableId,
    rows: usize,
    set: Vec<(&'static str, Vec<Value>)>,
}

impl Rows {
    /// `rows` rows of `table`, every column at its default.
    pub const fn new(table: TableId, rows: usize) -> Self {
        Self {
            table,
            rows,
            set: Vec::new(),
        }
    }

    /// Sets a column to one value per row.
    pub fn with(mut self, name: &'static str, values: Vec<Value>) -> Self {
        self.set.retain(|(set, _)| *set != name);
        self.set.push((name, values));
        self
    }

    /// Sets a column to the same value on every row.
    pub fn all(self, name: &'static str, value: Value) -> Self {
        let rows = self.rows;
        self.with(name, vec![value; rows])
    }

    /// Sets a column to `f(row index)`.
    pub fn each(self, name: &'static str, f: impl Fn(usize) -> Value) -> Self {
        let rows = self.rows;
        self.with(name, (0..rows).map(f).collect())
    }

    /// The number of rows.
    pub const fn len(&self) -> usize {
        self.rows
    }

    /// Whether there is no row.
    pub const fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The block, with every column of the `TableDef` in order and LZ4 frames.
    pub fn encode(&self) -> Result<EncodedBlock> {
        let def = table(self.table);
        for (name, values) in &self.set {
            if def.column(name).is_none() {
                bail!("{} has no column {name}", def.name());
            }
            if values.len() != self.rows {
                bail!("{name} has {} values for {} rows", values.len(), self.rows);
            }
        }
        let columns: Vec<Box<dyn Column>> = def
            .columns
            .iter()
            .map(|c| {
                let built = match self.set.iter().find(|(name, _)| *name == c.name) {
                    Some((_, values)) => column(&c.ty, values),
                    None => column(&c.ty, &vec![default_value(&c.ty); self.rows]),
                };
                built.with_context(|| format!("{}.{}", def.name(), c.name))
            })
            .collect::<Result<_>>()?;
        let types: Vec<String> = def.columns.iter().map(|c| c.ty.to_string()).collect();
        let mut block = Block::new(self.rows)?;
        for ((c, ty), data) in def.columns.iter().zip(&types).zip(&columns) {
            block.push(c.name, ty, data.as_ref())?;
        }
        Ok(block.encode(Compression::Lz4)?)
    }
}
