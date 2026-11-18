//! Native blocks: named, typed columns encoded as one block and compressed into frames.

use std::fmt;

use bytes::Bytes;

use crate::column::Column;
use crate::frame::{Compression, Framer, Sink};
use crate::types;

/// Most rows one block may carry, marker row included (contract section 5.2).
pub const MAX_BLOCK_ROWS: usize = 262_144;

/// Why a block could not be built or encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// The block would exceed [`MAX_BLOCK_ROWS`].
    #[error("a block holds at most {} rows, got {rows}", MAX_BLOCK_ROWS)]
    TooManyRows {
        /// Requested row count.
        rows: usize,
    },
    /// The type string is malformed or not one of the types of contract section 6.4.
    #[error("unsupported column type `{ty}`")]
    UnsupportedType {
        /// The type string.
        ty: String,
    },
    /// The column holds a different number of rows than the block.
    #[error("column `{column}` has {actual} rows, the block has {expected}")]
    RowCount {
        /// Column name.
        column: String,
        /// Rows of the block.
        expected: usize,
        /// Rows of the column.
        actual: usize,
    },
    /// The column's builder does not produce the layout of its type, or its nested row counts disagree.
    #[error("column `{column}` does not match type `{ty}`")]
    TypeMismatch {
        /// Column name.
        column: String,
        /// The type string.
        ty: String,
    },
    /// The compressor failed.
    #[error("compression failed: {0}")]
    Compression(String),
}

/// One Native block under construction: references to built columns, each checked against its type string.
#[derive(Debug)]
pub struct Block<'a> {
    rows: usize,
    columns: Vec<BlockColumn<'a>>,
}

#[derive(Debug)]
struct BlockColumn<'a> {
    name: &'a str,
    header: String,
    data: &'a dyn Column,
}

impl<'a> Block<'a> {
    /// An empty block of `rows` rows.
    pub fn new(rows: usize) -> Result<Self, EncodeError> {
        if rows > MAX_BLOCK_ROWS {
            return Err(EncodeError::TooManyRows { rows });
        }
        Ok(Self {
            rows,
            columns: Vec::new(),
        })
    }

    /// Rows of the block.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Appends the next column. `ty` is the column's `TableDef` type string; the header carries it verbatim except
    /// that every `LowCardinality(String)` becomes `String`.
    pub fn push(
        &mut self,
        name: &'a str,
        ty: &str,
        data: &'a dyn Column,
    ) -> Result<(), EncodeError> {
        let parsed =
            types::parse(ty).ok_or_else(|| EncodeError::UnsupportedType { ty: ty.to_owned() })?;
        if data.rows() != self.rows {
            return Err(EncodeError::RowCount {
                column: name.to_owned(),
                expected: self.rows,
                actual: data.rows(),
            });
        }
        if !data.check(&parsed.wire, self.rows) {
            return Err(EncodeError::TypeMismatch {
                column: name.to_owned(),
                ty: ty.to_owned(),
            });
        }
        self.columns.push(BlockColumn {
            name,
            header: parsed.header,
            data,
        });
        Ok(())
    }

    /// The uncompressed Native bytes of the block.
    pub fn native(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    /// Encodes the block and compresses it into frames of [`crate::CHUNK_SIZE`] uncompressed bytes, without holding
    /// the whole uncompressed block in memory.
    pub fn encode(&self, compression: Compression) -> Result<EncodedBlock, EncodeError> {
        let mut framer = Framer::new(compression)?;
        self.write(&mut framer);
        let (frames, uncompressed_size) = framer.finish()?;
        Ok(EncodedBlock {
            compressed: Bytes::from(frames),
            rows: self.rows,
            uncompressed_size,
        })
    }

    fn write(&self, out: &mut dyn Sink) {
        out.put_varint(self.columns.len() as u64);
        out.put_varint(self.rows as u64);
        for column in &self.columns {
            out.put_string(column.name);
            out.put_string(&column.header);
            if self.rows > 0 {
                column.data.write(out);
            }
        }
    }
}

/// A compressed Native block, ready to be the body of an `INSERT ... FORMAT Native` sent with `decompress=1`.
///
/// Cloning is cheap and the bytes never change, so a retry sends identical bytes (contract section 5.2).
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedBlock {
    compressed: Bytes,
    rows: usize,
    uncompressed_size: usize,
}

impl EncodedBlock {
    /// The frames.
    pub fn compressed(&self) -> &Bytes {
        &self.compressed
    }

    /// Rows in the block.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Length of the uncompressed Native block.
    pub fn uncompressed_size(&self) -> usize {
        self.uncompressed_size
    }
}

impl fmt::Debug for EncodedBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedBlock")
            .field("compressed_len", &self.compressed.len())
            .field("rows", &self.rows)
            .field("uncompressed_size", &self.uncompressed_size)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArrayColumn, StringColumn, TupleColumn, decompress};

    #[test]
    fn block_limit_is_enforced() {
        assert!(Block::new(MAX_BLOCK_ROWS).is_ok());
        assert_eq!(
            Block::new(MAX_BLOCK_ROWS + 1).err(),
            Some(EncodeError::TooManyRows {
                rows: MAX_BLOCK_ROWS + 1
            })
        );
    }

    #[test]
    fn columns_are_checked_against_their_types() {
        let numbers = vec![1u64, 2];
        let short = vec![1u64];
        let mut ragged = TupleColumn::new(
            ArrayColumn::new(Vec::<u16>::new()),
            ArrayColumn::new(Vec::<u64>::new()),
        );
        ragged.first_mut().push([1u16]);
        ragged.first_mut().push([2u16]);
        ragged.second_mut().push([3u64]);
        let mut block = Block::new(2).unwrap();
        assert_eq!(
            block.push("a", "Map(String, UInt64)", &numbers),
            Err(EncodeError::UnsupportedType {
                ty: "Map(String, UInt64)".to_owned()
            })
        );
        assert_eq!(
            block.push("a", "UInt64", &short),
            Err(EncodeError::RowCount {
                column: "a".to_owned(),
                expected: 2,
                actual: 1
            })
        );
        assert_eq!(
            block.push("a", "Int64", &numbers),
            Err(EncodeError::TypeMismatch {
                column: "a".to_owned(),
                ty: "Int64".to_owned()
            })
        );
        assert_eq!(
            block.push("h", "Tuple(Array(UInt16), Array(UInt64))", &ragged),
            Err(EncodeError::TypeMismatch {
                column: "h".to_owned(),
                ty: "Tuple(Array(UInt16), Array(UInt64))".to_owned()
            })
        );
        assert_eq!(
            block.push("a", "SimpleAggregateFunction(sum, UInt64)", &numbers),
            Ok(())
        );
    }

    #[test]
    fn zero_row_block_has_a_header_and_no_data() {
        let names = StringColumn::default();
        let mut block = Block::new(0).unwrap();
        block.push("s", "LowCardinality(String)", &names).unwrap();
        assert_eq!(block.native(), b"\x01\x00\x01s\x06String");
    }

    #[test]
    fn encoded_block_decompresses_to_the_native_bytes() {
        let values: Vec<u64> = (0..MAX_BLOCK_ROWS as u64).collect();
        let mut block = Block::new(MAX_BLOCK_ROWS).unwrap();
        block.push("v", "UInt64", &values).unwrap();
        let native = block.native();
        for compression in [Compression::Lz4, Compression::Zstd] {
            let encoded = block.encode(compression).unwrap();
            assert_eq!(encoded.rows(), MAX_BLOCK_ROWS);
            assert_eq!(encoded.uncompressed_size(), native.len());
            assert_eq!(decompress(encoded.compressed()).unwrap(), native);
        }
    }
}
