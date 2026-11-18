//! Native block decoder: the inverse of [`crate::Block`], for the store model of the simulation and for tests.
//!
//! It accepts exactly the types the encoder writes and never panics on malformed input.

use crate::types::{WireType, parse};

/// Why Native bytes could not be decoded as a block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockDecodeError {
    /// The input ends inside the block.
    #[error("input ends inside the block")]
    Truncated,
    /// A variable-length integer is longer than 10 bytes.
    #[error("variable-length integer longer than 10 bytes")]
    Varint,
    /// A column name or type string is not UTF-8.
    #[error("column name or type is not UTF-8")]
    Utf8,
    /// The header names a type the encoder never writes.
    #[error("unsupported header type `{0}`")]
    UnsupportedType(String),
}

/// Decoded values of one column.
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    /// `UInt8`.
    UInt8(Vec<u8>),
    /// `UInt16` and `Date`.
    UInt16(Vec<u16>),
    /// `UInt32` and `DateTime`.
    UInt32(Vec<u32>),
    /// `UInt64`.
    UInt64(Vec<u64>),
    /// `Enum8`.
    Int8(Vec<i8>),
    /// `Int32`.
    Int32(Vec<i32>),
    /// `Int64` and `DateTime64`.
    Int64(Vec<i64>),
    /// `Float32` as bit patterns, so that NaN payloads compare.
    Float32(Vec<u32>),
    /// `Float64` as bit patterns.
    Float64(Vec<u64>),
    /// `UUID` in standard (RFC 9562) byte order.
    Uuid(Vec<[u8; 16]>),
    /// `String`.
    String(Vec<Vec<u8>>),
    /// `FixedString(n)`: the width and the values.
    FixedString(usize, Vec<Vec<u8>>),
    /// `Nullable(T)`: the null flag of each row and every row's value.
    Nullable(Vec<bool>, Box<Values>),
    /// `Array(T)`: the cumulative end offset of each row and all elements.
    Array(Vec<u64>, Box<Values>),
    /// `Tuple(...)`: one column per element.
    Tuple(Vec<Values>),
}

/// One decoded column.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedColumn {
    /// The column name.
    pub name: String,
    /// The type string of the block header.
    pub ty: String,
    /// The values.
    pub values: Values,
}

/// One decoded Native block.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedBlock {
    /// Rows in the block.
    pub rows: usize,
    /// The columns, in header order.
    pub columns: Vec<DecodedColumn>,
}

type Result<T> = std::result::Result<T, BlockDecodeError>;

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(BlockDecodeError::Truncated)?;
        let taken = self
            .bytes
            .get(self.pos..end)
            .ok_or(BlockDecodeError::Truncated)?;
        self.pos = end;
        Ok(taken)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)?[0];
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err(BlockDecodeError::Varint)
    }

    fn len(&mut self) -> Result<usize> {
        usize::try_from(self.varint()?).map_err(|_| BlockDecodeError::Truncated)
    }

    fn string(&mut self) -> Result<&'a [u8]> {
        let len = self.len()?;
        self.take(len)
    }

    fn text(&mut self) -> Result<String> {
        String::from_utf8(self.string()?.to_vec()).map_err(|_| BlockDecodeError::Utf8)
    }

    fn le<const W: usize>(&mut self, rows: usize) -> Result<Vec<[u8; W]>> {
        let len = rows.checked_mul(W).ok_or(BlockDecodeError::Truncated)?;
        Ok(self.take(len)?.as_chunks::<W>().0.to_vec())
    }

    fn numbers<const W: usize, T>(
        &mut self,
        rows: usize,
        from: fn([u8; W]) -> T,
    ) -> Result<Vec<T>> {
        Ok(self.le(rows)?.into_iter().map(from).collect())
    }

    fn values(&mut self, ty: &WireType, rows: usize) -> Result<Values> {
        Ok(match ty {
            WireType::UInt8 => Values::UInt8(self.take(rows)?.to_vec()),
            WireType::UInt16 => Values::UInt16(self.numbers(rows, u16::from_le_bytes)?),
            WireType::UInt32 => Values::UInt32(self.numbers(rows, u32::from_le_bytes)?),
            WireType::UInt64 => Values::UInt64(self.numbers(rows, u64::from_le_bytes)?),
            WireType::Int8 => Values::Int8(self.numbers(rows, i8::from_le_bytes)?),
            WireType::Int32 => Values::Int32(self.numbers(rows, i32::from_le_bytes)?),
            WireType::Int64 => Values::Int64(self.numbers(rows, i64::from_le_bytes)?),
            WireType::Float32 => Values::Float32(self.numbers(rows, u32::from_le_bytes)?),
            WireType::Float64 => Values::Float64(self.numbers(rows, u64::from_le_bytes)?),
            WireType::Uuid => Values::Uuid(
                self.le::<16>(rows)?
                    .into_iter()
                    .map(|wire| {
                        let mut uuid = wire;
                        uuid[..8].reverse();
                        uuid[8..].reverse();
                        uuid
                    })
                    .collect(),
            ),
            WireType::String => {
                let mut strings = Vec::new();
                for _ in 0..rows {
                    strings.push(self.string()?.to_vec());
                }
                Values::String(strings)
            }
            WireType::FixedString(n) => {
                let len = rows.checked_mul(*n).ok_or(BlockDecodeError::Truncated)?;
                let bytes = self.take(len)?;
                let values = if *n == 0 {
                    vec![Vec::new(); rows]
                } else {
                    bytes.chunks_exact(*n).map(<[u8]>::to_vec).collect()
                };
                Values::FixedString(*n, values)
            }
            WireType::Nullable(inner) => {
                let nulls = self.take(rows)?.iter().map(|&b| b != 0).collect();
                Values::Nullable(nulls, Box::new(self.values(inner, rows)?))
            }
            WireType::Array(inner) => {
                let offsets = self.numbers(rows, u64::from_le_bytes)?;
                let elements = usize::try_from(offsets.last().copied().unwrap_or(0))
                    .map_err(|_| BlockDecodeError::Truncated)?;
                Values::Array(offsets, Box::new(self.values(inner, elements)?))
            }
            WireType::Tuple(elements) => {
                let mut columns = Vec::with_capacity(elements.len());
                for element in elements {
                    columns.push(self.values(element, rows)?);
                }
                Values::Tuple(columns)
            }
        })
    }
}

/// Decodes one Native block from the front of `bytes`: the block and the bytes it took.
pub fn decode_block(bytes: &[u8]) -> Result<(DecodedBlock, usize)> {
    let mut reader = Reader { bytes, pos: 0 };
    let count = reader.len()?;
    let rows = reader.len()?;
    let mut columns = Vec::new();
    for _ in 0..count {
        let name = reader.text()?;
        let ty = reader.text()?;
        let wire = parse(&ty)
            .ok_or_else(|| BlockDecodeError::UnsupportedType(ty.clone()))?
            .wire;
        // A block of zero rows is a header only.
        let values = reader.values(&wire, rows)?;
        columns.push(DecodedColumn { name, ty, values });
    }
    Ok((DecodedBlock { rows, columns }, reader.pos))
}
