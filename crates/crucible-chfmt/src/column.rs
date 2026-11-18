//! Typed column builders that hold values in, or close to, their Native wire layout.
//!
//! Every builder appends without allocating per value: numbers and fixed-size values go into one `Vec`, strings into
//! one byte buffer that already holds the wire image. Preallocate with `with_capacity` and reuse a builder across
//! blocks with [`Column::clear`].

use std::fmt;

use crate::frame::Sink;
use crate::types::WireType;

pub(crate) mod sealed {
    use crate::frame::Sink;
    use crate::types::WireType;

    pub trait Sealed {
        /// Whether the values fit `ty` and number exactly `rows`, recursively.
        fn check(&self, ty: &WireType, rows: usize) -> bool;
        /// Writes the column's data section.
        fn write(&self, out: &mut dyn Sink);
    }

    pub trait Number {
        const WIRE: WireType;
        const WIDTH: usize;
        /// Writes the little-endian bytes of the value into `dst`, which is exactly `WIDTH` long.
        fn put_le(self, dst: &mut [u8]);
    }
}

/// One column of a Native block. The implementations are the builders listed in the crate documentation.
pub trait Column: sealed::Sealed + fmt::Debug {
    /// Number of rows.
    fn rows(&self) -> usize;

    /// Removes every row and keeps the allocated capacity.
    fn clear(&mut self);
}

/// A fixed-width number stored in a `Vec<T>` column: `u8`, `u16`, `u32`, `u64`, `i8`, `i32`, `i64`, `f32`, `f64`.
pub trait Number: sealed::Number + Copy + Default + fmt::Debug {}

macro_rules! number {
    ($($t:ty => $wire:ident),* $(,)?) => {$(
        impl sealed::Number for $t {
            const WIRE: WireType = WireType::$wire;
            const WIDTH: usize = std::mem::size_of::<$t>();
            fn put_le(self, dst: &mut [u8]) {
                dst.copy_from_slice(&self.to_le_bytes());
            }
        }
        impl Number for $t {}
    )*};
}

number!(
    u8 => UInt8, u16 => UInt16, u32 => UInt32, u64 => UInt64,
    i8 => Int8, i32 => Int32, i64 => Int64,
    f32 => Float32, f64 => Float64,
);

/// Writes numbers little-endian through a stack buffer, so the sink sees a few large writes.
fn write_numbers<T: Number>(values: &[T], out: &mut dyn Sink) {
    const BUF: usize = 4096;
    let mut buf = [0u8; BUF];
    for chunk in values.chunks(BUF / T::WIDTH) {
        let len = chunk.len() * T::WIDTH;
        for (dst, value) in buf[..len].chunks_exact_mut(T::WIDTH).zip(chunk) {
            value.put_le(dst);
        }
        out.put(&buf[..len]);
    }
}

impl<T: Number> sealed::Sealed for Vec<T> {
    fn check(&self, ty: &WireType, rows: usize) -> bool {
        *ty == T::WIRE && self.len() == rows
    }

    fn write(&self, out: &mut dyn Sink) {
        write_numbers(self, out);
    }
}

impl<T: Number> Column for Vec<T> {
    fn rows(&self) -> usize {
        self.len()
    }

    fn clear(&mut self) {
        Vec::clear(self);
    }
}

/// `FixedString(N)`: each row is `N` raw bytes.
impl<const N: usize> sealed::Sealed for Vec<[u8; N]> {
    fn check(&self, ty: &WireType, rows: usize) -> bool {
        *ty == WireType::FixedString(N) && self.len() == rows
    }

    fn write(&self, out: &mut dyn Sink) {
        out.put(self.as_flattened());
    }
}

impl<const N: usize> Column for Vec<[u8; N]> {
    fn rows(&self) -> usize {
        self.len()
    }

    fn clear(&mut self) {
        Vec::clear(self);
    }
}

/// `UUID` column.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UuidColumn {
    /// Wire images: the high and low 64-bit halves of each UUID, each little-endian.
    wire: Vec<[u8; 16]>,
}

impl UuidColumn {
    /// An empty column with room for `rows` values.
    pub fn with_capacity(rows: usize) -> Self {
        Self {
            wire: Vec::with_capacity(rows),
        }
    }

    /// Appends a UUID given in its standard (RFC 9562) byte order.
    pub fn push(&mut self, uuid: [u8; 16]) {
        let value = u128::from_be_bytes(uuid);
        let mut wire = [0u8; 16];
        wire[..8].copy_from_slice(&((value >> 64) as u64).to_le_bytes());
        wire[8..].copy_from_slice(&(value as u64).to_le_bytes());
        self.wire.push(wire);
    }
}

impl sealed::Sealed for UuidColumn {
    fn check(&self, ty: &WireType, rows: usize) -> bool {
        *ty == WireType::Uuid && self.wire.len() == rows
    }

    fn write(&self, out: &mut dyn Sink) {
        out.put(self.wire.as_flattened());
    }
}

impl Column for UuidColumn {
    fn rows(&self) -> usize {
        self.wire.len()
    }

    fn clear(&mut self) {
        self.wire.clear();
    }
}

/// `String` column, also used for `LowCardinality(String)`. Values are arbitrary bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StringColumn {
    /// Wire image: each value's LEB128 length followed by its bytes.
    wire: Vec<u8>,
    rows: usize,
}

impl StringColumn {
    /// An empty column with room for `rows` values of `bytes` bytes in total.
    pub fn with_capacity(rows: usize, bytes: usize) -> Self {
        Self {
            wire: Vec::with_capacity(bytes + rows),
            rows: 0,
        }
    }

    /// Appends one value.
    pub fn push(&mut self, value: impl AsRef<[u8]>) {
        let value = value.as_ref();
        self.wire.put_varint(value.len() as u64);
        self.wire.extend_from_slice(value);
        self.rows += 1;
    }
}

impl<V: AsRef<[u8]>> Extend<V> for StringColumn {
    fn extend<I: IntoIterator<Item = V>>(&mut self, values: I) {
        for value in values {
            self.push(value);
        }
    }
}

impl sealed::Sealed for StringColumn {
    fn check(&self, ty: &WireType, rows: usize) -> bool {
        *ty == WireType::String && self.rows == rows
    }

    fn write(&self, out: &mut dyn Sink) {
        out.put(&self.wire);
    }
}

impl Column for StringColumn {
    fn rows(&self) -> usize {
        self.rows
    }

    fn clear(&mut self) {
        self.wire.clear();
        self.rows = 0;
    }
}

/// `Nullable(T)` column of numbers, for example `Nullable(DateTime64(6, 'UTC'))` as `NullableColumn<i64>`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NullableColumn<T> {
    /// One byte per row: 1 for null, 0 otherwise.
    nulls: Vec<u8>,
    /// One value per row, `T::default()` where null.
    values: Vec<T>,
}

impl<T: Number> NullableColumn<T> {
    /// An empty column with room for `rows` values.
    pub fn with_capacity(rows: usize) -> Self {
        Self {
            nulls: Vec::with_capacity(rows),
            values: Vec::with_capacity(rows),
        }
    }

    /// Appends a value or a null.
    pub fn push(&mut self, value: Option<T>) {
        self.nulls.push(u8::from(value.is_none()));
        self.values.push(value.unwrap_or_default());
    }
}

impl<T: Number> sealed::Sealed for NullableColumn<T> {
    fn check(&self, ty: &WireType, rows: usize) -> bool {
        matches!(ty, WireType::Nullable(inner) if **inner == T::WIRE) && self.nulls.len() == rows
    }

    fn write(&self, out: &mut dyn Sink) {
        out.put(&self.nulls);
        write_numbers(&self.values, out);
    }
}

impl<T: Number> Column for NullableColumn<T> {
    fn rows(&self) -> usize {
        self.nulls.len()
    }

    fn clear(&mut self) {
        self.nulls.clear();
        self.values.clear();
    }
}

/// `Array(T)` column whose elements go into one element column `C`, for example `ArrayColumn<StringColumn>` for
/// `Array(String)` or `ArrayColumn<Vec<u64>>` for `Array(UInt64)`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArrayColumn<C> {
    /// Cumulative element count at the end of each row.
    offsets: Vec<u64>,
    values: C,
}

impl<C: Column> ArrayColumn<C> {
    /// An empty column that stores its elements in `values`, whose rows are removed and whose capacity is kept.
    pub fn new(values: C) -> Self {
        Self::with_capacity(0, values)
    }

    /// As [`ArrayColumn::new`], with room for `rows` rows.
    pub fn with_capacity(rows: usize, mut values: C) -> Self {
        values.clear();
        Self {
            offsets: Vec::with_capacity(rows),
            values,
        }
    }

    /// Appends one row holding `elements`.
    pub fn push<I>(&mut self, elements: I)
    where
        I: IntoIterator,
        C: Extend<I::Item>,
    {
        self.values.extend(elements);
        self.offsets.push(self.values.rows() as u64);
    }
}

impl<C: Column> sealed::Sealed for ArrayColumn<C> {
    fn check(&self, ty: &WireType, rows: usize) -> bool {
        let WireType::Array(inner) = ty else {
            return false;
        };
        let elements = self.offsets.last().map_or(0, |&end| end as usize);
        self.offsets.len() == rows && self.values.check(inner, elements)
    }

    fn write(&self, out: &mut dyn Sink) {
        write_numbers(&self.offsets, out);
        self.values.write(out);
    }
}

impl<C: Column> Column for ArrayColumn<C> {
    fn rows(&self) -> usize {
        self.offsets.len()
    }

    fn clear(&mut self) {
        self.offsets.clear();
        self.values.clear();
    }
}

/// `Tuple(A, B)` column, for example
/// `TupleColumn<ArrayColumn<Vec<u16>>, ArrayColumn<Vec<u64>>>` for a histogram of section 6.5. Both elements must
/// hold the same number of rows when the column is added to a block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TupleColumn<A, B> {
    first: A,
    second: B,
}

impl<A: Column, B: Column> TupleColumn<A, B> {
    /// A column made of the two element columns.
    pub fn new(first: A, second: B) -> Self {
        Self { first, second }
    }

    /// The first element column.
    pub fn first_mut(&mut self) -> &mut A {
        &mut self.first
    }

    /// The second element column.
    pub fn second_mut(&mut self) -> &mut B {
        &mut self.second
    }
}

impl<A: Column, B: Column> sealed::Sealed for TupleColumn<A, B> {
    fn check(&self, ty: &WireType, rows: usize) -> bool {
        let WireType::Tuple(elements) = ty else {
            return false;
        };
        let [first, second] = elements.as_slice() else {
            return false;
        };
        self.first.check(first, rows) && self.second.check(second, rows)
    }

    fn write(&self, out: &mut dyn Sink) {
        self.first.write(out);
        self.second.write(out);
    }
}

impl<A: Column, B: Column> Column for TupleColumn<A, B> {
    fn rows(&self) -> usize {
        self.first.rows()
    }

    fn clear(&mut self) {
        self.first.clear();
        self.second.clear();
    }
}
