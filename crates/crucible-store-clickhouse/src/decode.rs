//! The decoder of read results: `RowBinaryWithNamesAndTypes` to [`Value`] rows.
//!
//! The row format is chosen over `Native` because it is row-wise and self-delimiting per value. A row is decoded
//! as soon as its bytes arrive, so a result streams in bounded memory whatever the server's block size, and a
//! `LowCardinality` or `SimpleAggregateFunction` column needs no dictionary or wrapper handling: its row image is
//! that of its underlying type. The names-and-types header is checked against the plan's output schema before
//! the first row, so a statement whose shape drifted from `QueryPlan::output` fails loudly instead of decoding
//! garbage.

use bytes::{Buf, BytesMut};
use crucible_plan::{OutputSchema, Row, RowBlock, Value};
use crucible_records::{CivilDate, Timestamp, Uuid};
use crucible_schema::Ty;

/// A result that does not follow its plan's output schema.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error("the result has {actual} columns, the plan has {expected}")]
    Columns { expected: usize, actual: usize },
    #[error("result column {index} is {actual}, the plan expects {expected}")]
    Type {
        index: usize,
        expected: String,
        actual: String,
    },
    #[error("the result ends inside a row")]
    Truncated,
}

/// A read position in the buffered bytes. Every read returns `None` when the bytes run out, which means "wait
/// for more".
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let [byte] = self.array::<1>()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        // More than ten bytes is not a length: reading on can only fail the same way.
        Some(u64::MAX)
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.varint()?).unwrap_or(usize::MAX);
        self.take(len)
    }
}

/// `None`: more bytes are needed. `Some(Err)`: the bytes are not a value of the type.
fn value(r: &mut Reader<'_>, ty: &Ty) -> Option<Result<Value, DecodeError>> {
    Some(Ok(match ty {
        Ty::Saf(_, inner) => return value(r, inner),
        Ty::UInt8 => Value::UInt(u64::from(u8::from_le_bytes(r.array()?))),
        Ty::UInt16 => Value::UInt(u64::from(u16::from_le_bytes(r.array()?))),
        Ty::UInt32 => Value::UInt(u64::from(u32::from_le_bytes(r.array()?))),
        Ty::UInt64 => Value::UInt(u64::from_le_bytes(r.array()?)),
        Ty::Int32 => Value::Int(i64::from(i32::from_le_bytes(r.array()?))),
        Ty::Int64 => Value::Int(i64::from_le_bytes(r.array()?)),
        Ty::Float32 => Value::Float(f64::from(f32::from_le_bytes(r.array()?))),
        Ty::Float64 => Value::Float(f64::from_le_bytes(r.array()?)),
        Ty::String | Ty::LowCardinalityString => Value::string(r.string()?.to_vec()),
        Ty::FixedString(n) => Value::Bytes(r.take(usize::from(*n))?.to_vec()),
        Ty::Uuid => {
            // Two 64-bit halves, each little-endian.
            let mut bytes: [u8; 16] = r.array()?;
            bytes[..8].reverse();
            bytes[8..].reverse();
            Value::Uuid(Uuid::from_bytes(bytes))
        }
        Ty::Date => Value::Date(CivilDate::from_day_number(i64::from(u16::from_le_bytes(
            r.array()?,
        )))),
        Ty::DateTime => Value::Time(Timestamp::from_micros(
            i64::from(u32::from_le_bytes(r.array()?)) * 1_000_000,
        )),
        Ty::DateTime64 => Value::Time(Timestamp::from_micros(i64::from_le_bytes(r.array()?))),
        Ty::Enum8(_) => {
            let [byte] = r.array::<1>()?;
            Value::Enum(byte)
        }
        Ty::Nullable(inner) => match r.array::<1>()? {
            [0] => return value(r, inner),
            _ => Value::Null,
        },
        Ty::Array(element) => {
            let len = r.varint()?;
            // Each element takes at least one byte, so a length beyond the buffered bytes waits for more.
            let mut items = Vec::with_capacity(usize::try_from(len).ok()?.min(r.bytes.len()));
            for _ in 0..len {
                match value(r, element)? {
                    Ok(item) => items.push(item),
                    Err(error) => return Some(Err(error)),
                }
            }
            Value::Array(items)
        }
        Ty::Tuple2(a, b) => {
            let first = match value(r, a)? {
                Ok(first) => first,
                Err(error) => return Some(Err(error)),
            };
            match value(r, b)? {
                Ok(second) => Value::Tuple2(Box::new(first), Box::new(second)),
                Err(error) => return Some(Err(error)),
            }
        }
    }))
}

/// The type string the server prints for a result column, without the `LowCardinality` wrapper: the row image
/// is the same with or without it.
fn normalize(server: &str) -> String {
    let mut out = String::with_capacity(server.len());
    let mut rest = server;
    while let Some(at) = rest.find("LowCardinality(") {
        out.push_str(&rest[..at]);
        let inner = &rest[at + "LowCardinality(".len()..];
        match inner.find(')') {
            Some(end) => {
                out.push_str(&inner[..end]);
                rest = &inner[end + 1..];
            }
            None => {
                rest = inner;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decodes the byte chunks of one result, in arrival order.
#[derive(Debug)]
pub(crate) struct Decoder {
    types: Vec<Ty>,
    buffer: BytesMut,
    header_done: bool,
}

impl Decoder {
    pub(crate) fn new(schema: &OutputSchema) -> Self {
        Self {
            types: schema.columns().iter().map(|c| c.ty).collect(),
            buffer: BytesMut::new(),
            header_done: false,
        }
    }

    fn header(&self, r: &mut Reader<'_>) -> Option<Result<(), DecodeError>> {
        let count = usize::try_from(r.varint()?).unwrap_or(usize::MAX);
        if count != self.types.len() {
            return Some(Err(DecodeError::Columns {
                expected: self.types.len(),
                actual: count,
            }));
        }
        for _ in 0..count {
            r.string()?;
        }
        for (index, expected) in self.types.iter().enumerate() {
            let actual = normalize(&String::from_utf8_lossy(r.string()?));
            let expected = expected.to_string();
            if actual != expected {
                return Some(Err(DecodeError::Type {
                    index,
                    expected,
                    actual,
                }));
            }
        }
        Some(Ok(()))
    }

    fn row(&self, r: &mut Reader<'_>) -> Option<Result<Row, DecodeError>> {
        let mut values = Vec::with_capacity(self.types.len());
        for ty in &self.types {
            match value(r, ty)? {
                Ok(v) => values.push(v),
                Err(error) => return Some(Err(error)),
            }
        }
        Some(Ok(Row::new(values)))
    }

    /// Adds a chunk and returns the rows that are now complete, cut into blocks.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<RowBlock>, DecodeError> {
        self.buffer.extend_from_slice(chunk);
        let mut r = Reader {
            bytes: &self.buffer,
            at: 0,
        };
        let mut consumed = 0;
        let mut rows = Vec::new();
        if !self.header_done {
            match self.header(&mut r) {
                None => return Ok(Vec::new()),
                Some(result) => result?,
            }
            consumed = r.at;
        }
        while r.at < r.bytes.len() {
            match self.row(&mut r) {
                None => break,
                Some(row) => rows.push(row?),
            }
            consumed = r.at;
        }
        self.header_done = true;
        self.buffer.advance(consumed);
        Ok(RowBlock::split(rows))
    }

    /// Called at the end of the result: bytes left over mean the result was cut inside a row.
    pub(crate) fn finish(&self) -> Result<(), DecodeError> {
        // A result with no row at all still carries its header.
        if self.buffer.is_empty() && self.header_done {
            Ok(())
        } else {
            Err(DecodeError::Truncated)
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_auth::test_support::scope;
    use crucible_plan::{Aggregate, Caller, Measure, Node, Pred, QueryClass, QueryPlan, data_bounds};
    use crucible_records::{ProjectId, TableId, TimeWindow};
    use crucible_schema::{Column, SpanMetricsCol};
    use proptest::prelude::*;

    use super::*;

    fn window() -> TimeWindow {
        TimeWindow::new(Timestamp::from_micros(0), Timestamp::from_micros(1)).unwrap()
    }

    fn put_string(out: &mut Vec<u8>, s: &[u8]) {
        out.push(u8::try_from(s.len()).unwrap());
        out.extend_from_slice(s);
    }

    /// `min(minute)`, `max(minute)` of the span rollup: two `DateTime('UTC')` columns.
    fn bounds_result(rows: &[(u32, u32)]) -> (OutputSchema, Vec<u8>) {
        let plans = data_bounds(
            scope(ProjectId::from_bytes([1; 16])).unwrap(),
            window(),
            Caller::Session,
        )
        .unwrap();
        let mut bytes = vec![2];
        put_string(&mut bytes, b"m0");
        put_string(&mut bytes, b"m1");
        put_string(&mut bytes, b"DateTime('UTC')");
        put_string(&mut bytes, b"DateTime('UTC')");
        for (a, b) in rows {
            bytes.extend_from_slice(&a.to_le_bytes());
            bytes.extend_from_slice(&b.to_le_bytes());
        }
        (plans.spans.output(), bytes)
    }

    #[test]
    fn low_cardinality_wrappers_are_dropped_from_server_types() {
        assert_eq!(normalize("LowCardinality(String)"), "String");
        assert_eq!(
            normalize("Array(LowCardinality(String))"),
            "Array(String)"
        );
        assert_eq!(
            normalize("Tuple(Array(UInt16), Array(UInt64))"),
            "Tuple(Array(UInt16), Array(UInt64))"
        );
    }

    #[test]
    fn an_empty_result_is_a_header_and_no_block() {
        let (schema, bytes) = bounds_result(&[]);
        let mut decoder = Decoder::new(&schema);
        assert_eq!(decoder.finish(), Err(DecodeError::Truncated));
        assert!(decoder.push(&bytes).unwrap().is_empty());
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn a_header_that_departs_from_the_plan_is_refused() {
        let (schema, mut bytes) = bounds_result(&[]);
        let mut decoder = Decoder::new(&schema);
        // Two columns are expected; a result with three is refused.
        bytes[0] = 3;
        assert_eq!(
            decoder.push(&bytes),
            Err(DecodeError::Columns {
                expected: 2,
                actual: 3
            })
        );
        let mut bytes = vec![2];
        put_string(&mut bytes, b"m0");
        put_string(&mut bytes, b"m1");
        put_string(&mut bytes, b"DateTime('UTC')");
        put_string(&mut bytes, b"DateTime");
        let error = Decoder::new(&schema).push(&bytes).unwrap_err();
        assert!(matches!(error, DecodeError::Type { index: 1, .. }), "{error}");
    }

    #[test]
    fn every_value_kind_decodes_to_its_one_representation() {
        let cases: Vec<(Ty, Vec<u8>, Value)> = vec![
            (Ty::UInt16, vec![1, 2], Value::UInt(0x0201)),
            (Ty::Int32, (-2i32).to_le_bytes().to_vec(), Value::Int(-2)),
            (Ty::Float32, 0.5f32.to_le_bytes().to_vec(), Value::Float(0.5)),
            (Ty::String, vec![2, b'o', b'k'], Value::String("ok".into())),
            (Ty::String, vec![1, 0xff], Value::Bytes(vec![0xff])),
            (Ty::FixedString(2), vec![0, 9], Value::Bytes(vec![0, 9])),
            (
                Ty::Uuid,
                vec![8, 7, 6, 5, 4, 3, 2, 1, 16, 15, 14, 13, 12, 11, 10, 9],
                Value::Uuid(Uuid::from_bytes([
                    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
                ])),
            ),
            (
                Ty::Date,
                vec![1, 0],
                Value::Date(CivilDate::new(1970, 1, 2).unwrap()),
            ),
            (
                Ty::DateTime,
                2u32.to_le_bytes().to_vec(),
                Value::Time(Timestamp::from_micros(2_000_000)),
            ),
            (
                Ty::DateTime64,
                7i64.to_le_bytes().to_vec(),
                Value::Time(Timestamp::from_micros(7)),
            ),
            (Ty::Nullable(&Ty::UInt8), vec![1], Value::Null),
            (Ty::Nullable(&Ty::UInt8), vec![0, 5], Value::UInt(5)),
            (
                Ty::Tuple2(&Ty::Array(&Ty::UInt16), &Ty::Array(&Ty::UInt64)),
                [vec![1, 3, 0, 1], 9u64.to_le_bytes().to_vec()].concat(),
                Value::Tuple2(
                    Box::new(Value::Array(vec![Value::UInt(3)])),
                    Box::new(Value::Array(vec![Value::UInt(9)])),
                ),
            ),
        ];
        for (ty, bytes, expected) in cases {
            let mut r = Reader {
                bytes: &bytes,
                at: 0,
            };
            assert_eq!(value(&mut r, &ty), Some(Ok(expected.clone())), "{ty}");
            assert_eq!(r.at, bytes.len(), "{ty}");
            assert!(expected.fits(&ty), "{ty}");
            // Every strict prefix waits for more bytes.
            for cut in 0..bytes.len() {
                let mut r = Reader {
                    bytes: &bytes[..cut],
                    at: 0,
                };
                assert_eq!(value(&mut r, &ty), None, "{ty} cut at {cut}");
            }
        }
    }

    #[test]
    fn enum_and_low_cardinality_columns_decode_under_the_plan_schema() {
        let plan = QueryPlan::new(
            scope(ProjectId::from_bytes([1; 16])).unwrap(),
            window(),
            QueryClass::Analytics,
            Node::Aggregate(
                Aggregate::new(
                    TableId::SpanMetrics,
                    Pred::all(),
                    [SpanMetricsCol::Environment, SpanMetricsCol::Level],
                    vec![Measure::Max(SpanMetricsCol::Minute.column_ref())],
                )
                .unwrap(),
            ),
        );
        let schema = plan.output();
        let level = schema.columns()[1].ty.to_string();
        let mut bytes = vec![3];
        for name in [&b"environment"[..], b"level", b"m0"] {
            put_string(&mut bytes, name);
        }
        put_string(&mut bytes, b"LowCardinality(String)");
        bytes.push(u8::try_from(level.len()).unwrap());
        bytes.extend_from_slice(level.as_bytes());
        put_string(&mut bytes, b"DateTime('UTC')");
        put_string(&mut bytes, b"prod");
        let stored = match schema.columns()[1].ty {
            Ty::Enum8(kind) => kind.variants()[0].1,
            other => panic!("{other}"),
        };
        bytes.push(stored);
        bytes.extend_from_slice(&60u32.to_le_bytes());
        let mut decoder = Decoder::new(&schema);
        let blocks = decoder.push(&bytes).unwrap();
        assert_eq!(decoder.finish(), Ok(()));
        assert_eq!(blocks.len(), 1);
        schema.check_block(&blocks[0]).unwrap();
        assert_eq!(
            blocks[0].rows()[0].values(),
            [
                Value::String("prod".into()),
                Value::Enum(stored),
                Value::Time(Timestamp::from_micros(60_000_000))
            ]
        );
    }

    proptest! {
        /// However the bytes are cut into chunks, the rows are the same, and they follow the plan's schema.
        #[test]
        fn chunk_boundaries_do_not_change_the_rows(
            rows in prop::collection::vec((any::<u32>(), any::<u32>()), 0..40),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..12),
        ) {
            let (schema, bytes) = bounds_result(&rows);
            let mut at: Vec<usize> = cuts.iter().map(|c| c.index(bytes.len() + 1)).collect();
            at.extend([0, bytes.len()]);
            at.sort_unstable();
            let mut decoder = Decoder::new(&schema);
            let mut decoded = Vec::new();
            for pair in at.windows(2) {
                for block in decoder.push(&bytes[pair[0]..pair[1]]).unwrap() {
                    schema.check_block(&block).unwrap();
                    decoded.extend(block.into_rows());
                }
            }
            prop_assert_eq!(decoder.finish(), Ok(()));
            let expected: Vec<Row> = rows
                .iter()
                .map(|(a, b)| {
                    let time = |s: u32| Value::Time(Timestamp::from_micros(i64::from(s) * 1_000_000));
                    Row::new(vec![time(*a), time(*b)])
                })
                .collect();
            prop_assert_eq!(decoded, expected);
        }

        /// A result cut anywhere short of its end is reported, never returned as a shorter result.
        #[test]
        fn a_result_cut_inside_a_row_is_truncated(
            rows in prop::collection::vec((any::<u32>(), any::<u32>()), 1..10),
            cut in any::<prop::sample::Index>(),
        ) {
            let (schema, bytes) = bounds_result(&rows);
            let cut = cut.index(bytes.len());
            let mut decoder = Decoder::new(&schema);
            let complete = (bytes.len() - cut) % 8 == 0 && cut >= bytes.len() - rows.len() * 8;
            decoder.push(&bytes[..cut]).unwrap();
            prop_assert_eq!(decoder.finish().is_ok(), complete);
        }
    }
}
