//! Test-only Native decoder and the property tests that use it.

use proptest::collection::vec;
use proptest::prelude::*;

use crate::types::{WireType, parse};
use crate::{
    ArrayColumn, Block, Column, Compression, NullableColumn, Number, StringColumn, TupleColumn,
    UuidColumn, decompress,
};

/// Decoded values of one column.
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    UInt8(Vec<u8>),
    UInt16(Vec<u16>),
    UInt32(Vec<u32>),
    UInt64(Vec<u64>),
    Int8(Vec<i8>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    /// Bit patterns, so that NaN payloads compare.
    Float32(Vec<u32>),
    Float64(Vec<u64>),
    /// Standard (RFC 9562) byte order.
    Uuid(Vec<[u8; 16]>),
    String(Vec<Vec<u8>>),
    FixedString(usize, Vec<Vec<u8>>),
    Nullable(Vec<bool>, Box<Values>),
    Array(Vec<u64>, Box<Values>),
    Tuple(Vec<Values>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedColumn {
    pub name: String,
    pub ty: String,
    pub values: Values,
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> &'a [u8] {
        let taken = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        taken
    }

    fn varint(&mut self) -> u64 {
        let mut value = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.take(1)[0];
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return value;
            }
        }
        panic!("varint longer than 10 bytes");
    }

    fn string(&mut self) -> &'a [u8] {
        let len = self.varint() as usize;
        self.take(len)
    }

    fn le<const W: usize>(&mut self, rows: usize) -> Vec<[u8; W]> {
        self.take(rows * W).as_chunks::<W>().0.to_vec()
    }

    fn values(&mut self, ty: &WireType, rows: usize) -> Values {
        match ty {
            WireType::UInt8 => Values::UInt8(self.take(rows).to_vec()),
            WireType::UInt16 => {
                Values::UInt16(self.le(rows).into_iter().map(u16::from_le_bytes).collect())
            }
            WireType::UInt32 => {
                Values::UInt32(self.le(rows).into_iter().map(u32::from_le_bytes).collect())
            }
            WireType::UInt64 => {
                Values::UInt64(self.le(rows).into_iter().map(u64::from_le_bytes).collect())
            }
            WireType::Int8 => {
                Values::Int8(self.le(rows).into_iter().map(i8::from_le_bytes).collect())
            }
            WireType::Int32 => {
                Values::Int32(self.le(rows).into_iter().map(i32::from_le_bytes).collect())
            }
            WireType::Int64 => {
                Values::Int64(self.le(rows).into_iter().map(i64::from_le_bytes).collect())
            }
            WireType::Float32 => {
                Values::Float32(self.le(rows).into_iter().map(u32::from_le_bytes).collect())
            }
            WireType::Float64 => {
                Values::Float64(self.le(rows).into_iter().map(u64::from_le_bytes).collect())
            }
            WireType::Uuid => Values::Uuid(
                self.le::<16>(rows)
                    .into_iter()
                    .map(|wire| {
                        let mut uuid = wire;
                        uuid[..8].reverse();
                        uuid[8..].reverse();
                        uuid
                    })
                    .collect(),
            ),
            WireType::String => Values::String((0..rows).map(|_| self.string().to_vec()).collect()),
            WireType::FixedString(n) => Values::FixedString(
                *n,
                self.take(rows * n)
                    .chunks_exact(*n)
                    .map(<[u8]>::to_vec)
                    .collect(),
            ),
            WireType::Nullable(inner) => {
                let nulls = self.take(rows).iter().map(|&b| b != 0).collect();
                Values::Nullable(nulls, Box::new(self.values(inner, rows)))
            }
            WireType::Array(inner) => {
                let offsets: Vec<u64> = self.le(rows).into_iter().map(u64::from_le_bytes).collect();
                let elements = offsets.last().copied().unwrap_or(0) as usize;
                Values::Array(offsets, Box::new(self.values(inner, elements)))
            }
            WireType::Tuple(elements) => Values::Tuple(
                elements
                    .iter()
                    .map(|element| self.values(element, rows))
                    .collect(),
            ),
        }
    }
}

/// Decodes one Native block from the front of `bytes`: its rows, its columns and the bytes it took.
pub fn decode_block(bytes: &[u8]) -> (usize, Vec<DecodedColumn>, usize) {
    let mut reader = Reader { bytes, pos: 0 };
    let columns = reader.varint() as usize;
    let rows = reader.varint() as usize;
    let decoded = (0..columns)
        .map(|_| {
            let name = String::from_utf8(reader.string().to_vec()).unwrap();
            let ty = String::from_utf8(reader.string().to_vec()).unwrap();
            let wire = parse(&ty)
                .unwrap_or_else(|| panic!("unsupported header type `{ty}`"))
                .wire;
            DecodedColumn {
                name,
                ty,
                values: reader.values(&wire, rows),
            }
        })
        .collect();
    (rows, decoded, reader.pos)
}

/// A builder holding decoded values, for re-encoding them.
#[derive(Debug)]
pub enum Built {
    UInt8(Vec<u8>),
    UInt16(Vec<u16>),
    UInt32(Vec<u32>),
    UInt64(Vec<u64>),
    Int8(Vec<i8>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Float32(Vec<f32>),
    Float64(Vec<f64>),
    Uuid(UuidColumn),
    String(StringColumn),
    Fixed8(Vec<[u8; 8]>),
    Fixed16(Vec<[u8; 16]>),
    Fixed32(Vec<[u8; 32]>),
    NullableUInt16(NullableColumn<u16>),
    NullableUInt32(NullableColumn<u32>),
    NullableInt32(NullableColumn<i32>),
    NullableInt64(NullableColumn<i64>),
    ArrayString(ArrayColumn<StringColumn>),
    ArrayUInt8(ArrayColumn<Vec<u8>>),
    ArrayUInt16(ArrayColumn<Vec<u16>>),
    ArrayUInt32(ArrayColumn<Vec<u32>>),
    ArrayUInt64(ArrayColumn<Vec<u64>>),
    ArrayInt64(ArrayColumn<Vec<i64>>),
    Histogram16(TupleColumn<ArrayColumn<Vec<u16>>, ArrayColumn<Vec<u64>>>),
    Histogram8(TupleColumn<ArrayColumn<Vec<u8>>, ArrayColumn<Vec<u64>>>),
}

impl Built {
    pub fn column(&self) -> &dyn Column {
        match self {
            Built::UInt8(c) => c,
            Built::UInt16(c) => c,
            Built::UInt32(c) => c,
            Built::UInt64(c) => c,
            Built::Int8(c) => c,
            Built::Int32(c) => c,
            Built::Int64(c) => c,
            Built::Float32(c) => c,
            Built::Float64(c) => c,
            Built::Uuid(c) => c,
            Built::String(c) => c,
            Built::Fixed8(c) => c,
            Built::Fixed16(c) => c,
            Built::Fixed32(c) => c,
            Built::NullableUInt16(c) => c,
            Built::NullableUInt32(c) => c,
            Built::NullableInt32(c) => c,
            Built::NullableInt64(c) => c,
            Built::ArrayString(c) => c,
            Built::ArrayUInt8(c) => c,
            Built::ArrayUInt16(c) => c,
            Built::ArrayUInt32(c) => c,
            Built::ArrayUInt64(c) => c,
            Built::ArrayInt64(c) => c,
            Built::Histogram16(c) => c,
            Built::Histogram8(c) => c,
        }
    }
}

fn nullable<T: Number>(nulls: &[bool], values: &[T]) -> NullableColumn<T> {
    let mut column = NullableColumn::default();
    for (&null, &value) in nulls.iter().zip(values) {
        column.push((!null).then_some(value));
    }
    column
}

fn array<C, T>(offsets: &[u64], flat: &[T], values: C) -> ArrayColumn<C>
where
    C: Column + for<'x> Extend<&'x T>,
{
    let mut column = ArrayColumn::new(values);
    let mut start = 0;
    for &end in offsets {
        column.push(&flat[start..end as usize]);
        start = end as usize;
    }
    column
}

fn fixed<const N: usize>(values: &[Vec<u8>]) -> Vec<[u8; N]> {
    values
        .iter()
        .map(|v| v.as_slice().try_into().unwrap())
        .collect()
}

/// Puts decoded values into the builder for their layout.
pub fn build(values: &Values) -> Built {
    match values {
        Values::UInt8(v) => Built::UInt8(v.clone()),
        Values::UInt16(v) => Built::UInt16(v.clone()),
        Values::UInt32(v) => Built::UInt32(v.clone()),
        Values::UInt64(v) => Built::UInt64(v.clone()),
        Values::Int8(v) => Built::Int8(v.clone()),
        Values::Int32(v) => Built::Int32(v.clone()),
        Values::Int64(v) => Built::Int64(v.clone()),
        Values::Float32(v) => Built::Float32(v.iter().map(|&bits| f32::from_bits(bits)).collect()),
        Values::Float64(v) => Built::Float64(v.iter().map(|&bits| f64::from_bits(bits)).collect()),
        Values::Uuid(v) => {
            let mut column = UuidColumn::default();
            v.iter().for_each(|&uuid| column.push(uuid));
            Built::Uuid(column)
        }
        Values::String(v) => {
            let mut column = StringColumn::default();
            column.extend(v);
            Built::String(column)
        }
        Values::FixedString(8, v) => Built::Fixed8(fixed(v)),
        Values::FixedString(16, v) => Built::Fixed16(fixed(v)),
        Values::FixedString(32, v) => Built::Fixed32(fixed(v)),
        Values::Nullable(nulls, inner) => match &**inner {
            Values::UInt16(v) => Built::NullableUInt16(nullable(nulls, v)),
            Values::UInt32(v) => Built::NullableUInt32(nullable(nulls, v)),
            Values::Int32(v) => Built::NullableInt32(nullable(nulls, v)),
            Values::Int64(v) => Built::NullableInt64(nullable(nulls, v)),
            other => panic!("no builder for Nullable of {other:?}"),
        },
        Values::Array(offsets, inner) => match &**inner {
            Values::String(v) => Built::ArrayString(array(offsets, v, StringColumn::default())),
            Values::UInt8(v) => Built::ArrayUInt8(array(offsets, v, Vec::new())),
            Values::UInt16(v) => Built::ArrayUInt16(array(offsets, v, Vec::new())),
            Values::UInt32(v) => Built::ArrayUInt32(array(offsets, v, Vec::new())),
            Values::UInt64(v) => Built::ArrayUInt64(array(offsets, v, Vec::new())),
            Values::Int64(v) => Built::ArrayInt64(array(offsets, v, Vec::new())),
            other => panic!("no builder for Array of {other:?}"),
        },
        Values::Tuple(elements) => match elements.as_slice() {
            [Values::Array(ko, keys), Values::Array(co, counts)] => match (&**keys, &**counts) {
                (Values::UInt16(k), Values::UInt64(c)) => Built::Histogram16(TupleColumn::new(
                    array(ko, k, Vec::new()),
                    array(co, c, Vec::new()),
                )),
                (Values::UInt8(k), Values::UInt64(c)) => Built::Histogram8(TupleColumn::new(
                    array(ko, k, Vec::new()),
                    array(co, c, Vec::new()),
                )),
                other => panic!("no builder for Tuple of {other:?}"),
            },
            other => panic!("no builder for Tuple of {other:?}"),
        },
        other => panic!("no builder for {other:?}"),
    }
}

/// Re-encodes a decoded block with the builders and the header types it carried.
pub fn reencode(rows: usize, columns: &[DecodedColumn]) -> Vec<u8> {
    let built: Vec<Built> = columns.iter().map(|c| build(&c.values)).collect();
    let mut block = Block::new(rows).unwrap();
    for (column, data) in columns.iter().zip(&built) {
        block.push(&column.name, &column.ty, data.column()).unwrap();
    }
    block.native()
}

/// (type string, expected header, values) for one random column of `rows` rows.
type Case = (&'static str, &'static str, Values);

fn pick(
    spellings: &'static [(&'static str, &'static str)],
) -> impl Strategy<Value = (&'static str, &'static str)> {
    proptest::sample::select(spellings)
}

fn offsets_of<T>(rows: &[Vec<T>]) -> Vec<u64> {
    rows.iter()
        .scan(0u64, |end, row| {
            *end += row.len() as u64;
            Some(*end)
        })
        .collect()
}

fn array_values<T: Strategy>(
    element: T,
    rows: usize,
    wrap: fn(Vec<T::Value>) -> Values,
) -> impl Strategy<Value = Values>
where
    T::Value: Clone,
{
    vec(vec(element, 0..6), rows).prop_map(move |rows| {
        let offsets = offsets_of(&rows);
        Values::Array(
            offsets,
            Box::new(wrap(rows.into_iter().flatten().collect())),
        )
    })
}

fn nullable_values<T: Strategy>(
    value: T,
    rows: usize,
    wrap: fn(Vec<T::Value>) -> Values,
) -> impl Strategy<Value = Values>
where
    T::Value: Default + Clone,
{
    vec(proptest::option::of(value), rows).prop_map(move |values| {
        let nulls = values.iter().map(Option::is_none).collect();
        let values = values.into_iter().map(Option::unwrap_or_default).collect();
        Values::Nullable(nulls, Box::new(wrap(values)))
    })
}

fn histogram_values<K: Strategy>(
    key: K,
    rows: usize,
    wrap: fn(Vec<K::Value>) -> Values,
) -> impl Strategy<Value = Values>
where
    K::Value: Clone,
{
    vec((vec(key, 0..6), vec(any::<u64>(), 0..6)), rows).prop_map(move |rows| {
        let (keys, counts): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        Values::Tuple(vec![
            Values::Array(
                offsets_of(&keys),
                Box::new(wrap(keys.into_iter().flatten().collect())),
            ),
            Values::Array(
                offsets_of(&counts),
                Box::new(Values::UInt64(counts.into_iter().flatten().collect())),
            ),
        ])
    })
}

/// Short and two-byte-length strings.
fn string_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![vec(any::<u8>(), 0..8), vec(any::<u8>(), 120..140)]
}

fn column(rows: usize) -> BoxedStrategy<Case> {
    fn case<S: Strategy<Value = Values> + 'static>(
        spellings: &'static [(&'static str, &'static str)],
        values: S,
    ) -> BoxedStrategy<Case> {
        (pick(spellings), values)
            .prop_map(|((ty, header), values)| (ty, header, values))
            .boxed()
    }
    let n = rows;
    prop_oneof![
        case(
            &[
                ("UInt8", "UInt8"),
                (
                    "SimpleAggregateFunction(max, UInt8)",
                    "SimpleAggregateFunction(max, UInt8)"
                )
            ],
            vec(any::<u8>(), n).prop_map(Values::UInt8)
        ),
        case(
            &[("UInt16", "UInt16"), ("Date", "Date")],
            vec(any::<u16>(), n).prop_map(Values::UInt16)
        ),
        case(
            &[
                ("UInt32", "UInt32"),
                ("DateTime('UTC')", "DateTime('UTC')"),
                ("DateTime", "DateTime"),
                (
                    "SimpleAggregateFunction(max, DateTime('UTC'))",
                    "SimpleAggregateFunction(max, DateTime('UTC'))"
                )
            ],
            vec(any::<u32>(), n).prop_map(Values::UInt32)
        ),
        case(
            &[
                ("UInt64", "UInt64"),
                (
                    "SimpleAggregateFunction(sum, UInt64)",
                    "SimpleAggregateFunction(sum, UInt64)"
                )
            ],
            vec(any::<u64>(), n).prop_map(Values::UInt64)
        ),
        case(
            &[(
                "Enum8('a' = -128, 'b' = 0, 'c' = 127)",
                "Enum8('a' = -128, 'b' = 0, 'c' = 127)"
            )],
            vec(any::<i8>(), n).prop_map(Values::Int8)
        ),
        case(
            &[("Int32", "Int32")],
            vec(any::<i32>(), n).prop_map(Values::Int32)
        ),
        case(
            &[
                ("Int64", "Int64"),
                ("DateTime64(6, 'UTC')", "DateTime64(6, 'UTC')"),
                (
                    "SimpleAggregateFunction(sum, Int64)",
                    "SimpleAggregateFunction(sum, Int64)"
                ),
                (
                    "SimpleAggregateFunction(min, DateTime64(6, 'UTC'))",
                    "SimpleAggregateFunction(min, DateTime64(6, 'UTC'))"
                )
            ],
            vec(any::<i64>(), n).prop_map(Values::Int64)
        ),
        case(
            &[("Float32", "Float32")],
            vec(any::<u32>(), n).prop_map(Values::Float32)
        ),
        case(
            &[("Float64", "Float64")],
            vec(any::<u64>(), n).prop_map(Values::Float64)
        ),
        case(
            &[("UUID", "UUID")],
            vec(any::<[u8; 16]>(), n).prop_map(Values::Uuid)
        ),
        case(
            &[
                ("String", "String"),
                ("LowCardinality(String)", "String"),
                (
                    "SimpleAggregateFunction(max, String)",
                    "SimpleAggregateFunction(max, String)"
                )
            ],
            vec(string_bytes(), n).prop_map(Values::String)
        ),
        case(
            &[("FixedString(8)", "FixedString(8)")],
            vec(vec(any::<u8>(), 8), n).prop_map(|v| Values::FixedString(8, v))
        ),
        case(
            &[("FixedString(16)", "FixedString(16)")],
            vec(vec(any::<u8>(), 16), n).prop_map(|v| Values::FixedString(16, v))
        ),
        case(
            &[("FixedString(32)", "FixedString(32)")],
            vec(vec(any::<u8>(), 32), n).prop_map(|v| Values::FixedString(32, v))
        ),
        case(
            &[("Nullable(UInt16)", "Nullable(UInt16)")],
            nullable_values(any::<u16>(), n, Values::UInt16)
        ),
        case(
            &[("Nullable(UInt32)", "Nullable(UInt32)")],
            nullable_values(any::<u32>(), n, Values::UInt32)
        ),
        case(
            &[("Nullable(Int32)", "Nullable(Int32)")],
            nullable_values(any::<i32>(), n, Values::Int32)
        ),
        case(
            &[(
                "Nullable(DateTime64(6, 'UTC'))",
                "Nullable(DateTime64(6, 'UTC'))"
            )],
            nullable_values(any::<i64>(), n, Values::Int64)
        ),
        case(
            &[
                ("Array(String)", "Array(String)"),
                ("Array(LowCardinality(String))", "Array(String)"),
                (
                    "SimpleAggregateFunction(groupUniqArrayArray(16), Array(String))",
                    "SimpleAggregateFunction(groupUniqArrayArray(16), Array(String))"
                )
            ],
            array_values(string_bytes(), n, Values::String)
        ),
        case(
            &[
                ("Array(UInt8)", "Array(UInt8)"),
                (
                    "SimpleAggregateFunction(groupUniqArrayArray(4), Array(UInt8))",
                    "SimpleAggregateFunction(groupUniqArrayArray(4), Array(UInt8))"
                )
            ],
            array_values(any::<u8>(), n, Values::UInt8)
        ),
        case(
            &[("Array(UInt32)", "Array(UInt32)")],
            array_values(any::<u32>(), n, Values::UInt32)
        ),
        case(
            &[("Array(UInt64)", "Array(UInt64)")],
            array_values(any::<u64>(), n, Values::UInt64)
        ),
        case(
            &[("Array(Int64)", "Array(Int64)")],
            array_values(any::<i64>(), n, Values::Int64)
        ),
        case(
            &[
                (
                    "Tuple(Array(UInt16), Array(UInt64))",
                    "Tuple(Array(UInt16), Array(UInt64))"
                ),
                (
                    "SimpleAggregateFunction(sumMap, Tuple(Array(UInt16), Array(UInt64)))",
                    "SimpleAggregateFunction(sumMap, Tuple(Array(UInt16), Array(UInt64)))"
                )
            ],
            histogram_values(any::<u16>(), n, Values::UInt16)
        ),
        case(
            &[
                (
                    "Tuple(Array(UInt8), Array(UInt64))",
                    "Tuple(Array(UInt8), Array(UInt64))"
                ),
                (
                    "SimpleAggregateFunction(sumMap, Tuple(Array(UInt8), Array(UInt64)))",
                    "SimpleAggregateFunction(sumMap, Tuple(Array(UInt8), Array(UInt64)))"
                )
            ],
            histogram_values(any::<u8>(), n, Values::UInt8)
        ),
    ]
    .boxed()
}

/// A block of one to four random columns; row counts above 127 take a two-byte length on the wire.
fn block() -> impl Strategy<Value = (usize, Vec<Case>)> {
    prop_oneof![3 => 0..40usize, 1 => 120..300usize]
        .prop_flat_map(|rows| (Just(rows), vec(column(rows), 1..5)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_columns_round_trip_through_the_encoder((rows, cases) in block()) {
        let built: Vec<Built> = cases.iter().map(|(_, _, values)| build(values)).collect();
        let names: Vec<String> = (0..cases.len()).map(|i| format!("c{i}")).collect();
        let mut block = Block::new(rows).unwrap();
        for ((name, (ty, _, _)), data) in names.iter().zip(&cases).zip(&built) {
            block.push(name, ty, data.column()).unwrap();
        }
        let native = block.native();
        let (decoded_rows, decoded, used) = decode_block(&native);
        prop_assert_eq!(decoded_rows, rows);
        prop_assert_eq!(used, native.len());
        prop_assert_eq!(decoded.len(), cases.len());
        for ((column, name), (_, header, values)) in decoded.iter().zip(&names).zip(&cases) {
            prop_assert_eq!(&column.name, name);
            prop_assert_eq!(&column.ty, header);
            prop_assert_eq!(&column.values, values);
        }
        prop_assert_eq!(reencode(rows, &decoded), native.clone());
        for compression in [Compression::Lz4, Compression::Zstd] {
            let encoded = block.encode(compression).unwrap();
            prop_assert_eq!(encoded.uncompressed_size(), native.len());
            prop_assert_eq!(decompress(encoded.compressed()).unwrap(), native.clone());
        }
    }
}

macro_rules! fixtures {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_bytes!(concat!("../tests/fixtures/", $name, ".native")) as &[u8])),*]
    };
}

/// Every Native block ClickHouse produced for `tests/fixtures/*.sql`.
const FIXTURES: &[(&str, &[u8])] = fixtures![
    "array_int64",
    "array_string",
    "array_uint32",
    "array_uint64",
    "array_uint8",
    "date",
    "datetime",
    "datetime64",
    "enum8",
    "fixed_string_16",
    "fixed_string_32",
    "fixed_string_8",
    "float32",
    "float64",
    "int64",
    "nullable_datetime64",
    "nullable_int32",
    "nullable_uint16",
    "nullable_uint32",
    "saf_group_uniq_array_string_16",
    "saf_group_uniq_array_string_8",
    "saf_group_uniq_array_uint8_4",
    "saf_max_datetime",
    "saf_max_datetime64",
    "saf_max_string",
    "saf_max_uint8",
    "saf_min_datetime64",
    "saf_sum_int64",
    "saf_sum_map_u16",
    "saf_sum_map_u8",
    "saf_sum_uint64",
    "string",
    "tuple_u16_u64",
    "tuple_u8_u64",
    "uint16",
    "uint32",
    "uint64",
    "uint64_many",
    "uint8",
    "uuid",
];

#[test]
fn clickhouse_fixtures_decode_and_reencode_byte_for_byte() {
    for (name, bytes) in FIXTURES {
        let (rows, columns, used) = decode_block(bytes);
        assert_eq!(used, bytes.len(), "{name}");
        assert_eq!(columns.len(), 1, "{name}");
        assert_eq!(&columns[0].name, name);
        assert_eq!(reencode(rows, &columns), *bytes, "{name}");
    }
}

#[test]
fn decoder_reads_clickhouse_values() {
    let decoded = |name: &str| {
        let (_, bytes) = FIXTURES.iter().find(|(n, _)| *n == name).unwrap();
        decode_block(bytes).1.remove(0).values
    };
    let Values::Uuid(uuids) = decoded("uuid") else {
        panic!("uuid")
    };
    assert_eq!(
        uuids[1],
        [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff
        ]
    );
    let Values::Int64(micros) = decoded("datetime64") else {
        panic!("datetime64")
    };
    assert_eq!(micros[0], -2_208_988_800_000_000);
    assert_eq!(micros[4], 1_794_907_800_123_456);
    let Values::Float32(bits) = decoded("float32") else {
        panic!("float32")
    };
    assert_eq!(bits[1], (-0.0f32).to_bits());
    assert_eq!(bits[7], f32::NAN.to_bits());
    let Values::Nullable(nulls, inner) = decoded("nullable_int32") else {
        panic!("nullable_int32")
    };
    assert_eq!(nulls, [false, true, false, false, true, false, false, true]);
    assert_eq!(
        *inner,
        Values::Int32(vec![i32::MIN, 0, -1, 0, 0, 1, i32::MAX, 0])
    );
    let Values::Array(offsets, inner) = decoded("array_int64") else {
        panic!("array_int64")
    };
    assert_eq!(offsets, [0, 1, 3, 5, 5, 6, 9, 10]);
    assert_eq!(
        inner.as_ref(),
        &Values::Int64(vec![
            0,
            i64::MIN,
            i64::MAX,
            -1,
            1,
            42,
            -42,
            0,
            42,
            1_000_000_000_000
        ])
    );
    let Values::String(strings) = decoded("string") else {
        panic!("string")
    };
    assert_eq!(strings[2], b"\x00\xff\r\n");
    assert_eq!(strings[7].len(), 16384);
}
