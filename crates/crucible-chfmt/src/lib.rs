//! ClickHouse Native block encoder and compressed framing.
//!
//! The shipper sends each (batch, table, partition) pair as one `INSERT INTO crucible.<table> (...) FORMAT Native`
//! whose body is a single Native block, compressed in ClickHouse's framed format and posted with `decompress=1`
//! (contract section 5.2). This crate builds that body and decodes framed `compress=1` responses. It is pure: no
//! `Env`, no IO.
//!
//! # Column types
//!
//! The encoder covers exactly the ClickHouse types that the schema of contract section 6.4 uses. The first column is
//! the type string as it appears in a `TableDef`, the second the layout of the values on the wire (all integers and
//! floats little-endian), the third the builder that holds them.
//!
//! | Type string (section 6.4) | Wire layout | Builder |
//! |---|---|---|
//! | `UInt8` | 1 byte | `Vec<u8>` |
//! | `UInt16` | 2 bytes | `Vec<u16>` |
//! | `UInt32` | 4 bytes | `Vec<u32>` |
//! | `UInt64` | 8 bytes | `Vec<u64>` |
//! | `Int64` | 8 bytes, two's complement | `Vec<i64>` |
//! | `Float32`, `Float64` | IEEE 754 binary32, binary64 | `Vec<f32>`, `Vec<f64>` |
//! | `UUID` | the high and low 64-bit halves, each little-endian | [`UuidColumn`] |
//! | `String` | LEB128 byte length, then the bytes | [`StringColumn`] |
//! | `FixedString(8)`, `FixedString(16)`, `FixedString(32)` | N bytes | `Vec<[u8; N]>` |
//! | `Date` | `u16` days since 1970-01-01 | `Vec<u16>` |
//! | `DateTime('UTC')` | `u32` seconds since the Unix epoch | `Vec<u32>` |
//! | `DateTime64(6, 'UTC')` | `i64` microseconds since the Unix epoch | `Vec<i64>` |
//! | `Enum8(...)` with the names and values of section 1.6 | `i8` value | `Vec<i8>` |
//! | `LowCardinality(String)` | sent as `String`, header and data | [`StringColumn`] |
//! | `Nullable(DateTime64(6, 'UTC'))`, `Nullable(UInt32)`, `Nullable(Int32)`, `Nullable(UInt16)` | null map (one byte per row, 1 means null), then every row's value, zero where null | [`NullableColumn`] |
//! | `Array(String)`, `Array(LowCardinality(String))` (sent as `Array(String)`), `Array(UInt8)`, `Array(UInt32)`, `Array(UInt64)`, `Array(Int64)` | cumulative end offset of each row (`u64`), then all elements as one column | [`ArrayColumn`] |
//! | `Tuple(Array(UInt16), Array(UInt64))`, `Tuple(Array(UInt8), Array(UInt64))` | the first element column, then the second | [`TupleColumn`] |
//! | `SimpleAggregateFunction(f, T)` | the layout of `T` | the builder of `T` |
//!
//! The `SimpleAggregateFunction` types of section 6.4 are `(max, DateTime('UTC'))`, `(min, DateTime64(6, 'UTC'))`,
//! `(max, DateTime64(6, 'UTC'))`, `(max, UInt8)`, `(sum, UInt64)`, `(sum, Int64)`, `(max, String)`,
//! `(groupUniqArrayArray(16), Array(String))`, `(groupUniqArrayArray(8), Array(String))`,
//! `(groupUniqArrayArray(4), Array(UInt8))`, and the histograms of section 6.5,
//! `(sumMap, Tuple(Array(UInt16), Array(UInt64)))` and `(sumMap, Tuple(Array(UInt8), Array(UInt64)))`.
//!
//! `Int32` occurs only inside `Nullable(Int32)`. Bare `DateTime` is also accepted, because ClickHouse writes a
//! top-level `DateTime('UTC')` column that way in its HTTP Native output. Types that section 6.4 does not use (for
//! example `Bool`, `Map`, `Int8`, `Decimal`, `DateTime64(3)`) are rejected.
//!
//! # Blocks
//!
//! A [`Block`] borrows one built column per table column, in `TableDef` order, and checks each against its type
//! string. The block header carries every column's name and its `TableDef` type string verbatim, except that each
//! `LowCardinality(String)` becomes `String` (section 5.2). Rows are encoded in the order the columns hold them;
//! sorting by the table's ORDER BY key is the caller's job. A block holds at most [`MAX_BLOCK_ROWS`] rows.
//!
//! # Compressed framing
//!
//! [`Block::encode`] splits the Native bytes into chunks of [`CHUNK_SIZE`] (the last one may be shorter) and
//! compresses each into one frame:
//!
//! | Bytes | Field |
//! |---|---|
//! | 16 | CityHash128 v1.0.2 of the rest of the frame: the hash's first 64-bit word, then its second, little-endian |
//! | 1 | method: `0x82` LZ4, `0x90` ZSTD |
//! | 4 | compressed size, including these 9 header bytes (`u32` little-endian) |
//! | 4 | uncompressed size of the chunk (`u32` little-endian) |
//! | n | the LZ4 block or ZSTD frame |
//!
//! [`decompress`] reverses this for both methods and verifies every checksum.

mod block;
mod column;
mod decode;
mod frame;
mod types;

#[cfg(test)]
mod testing;

pub use block::{Block, EncodeError, EncodedBlock, MAX_BLOCK_ROWS};
pub use column::{
    ArrayColumn, Column, NullableColumn, Number, StringColumn, TupleColumn, UuidColumn,
};
pub use decode::{BlockDecodeError, DecodedBlock, DecodedColumn, Values, decode_block};
pub use frame::{CHUNK_SIZE, Compression, DecodeError, decompress};
