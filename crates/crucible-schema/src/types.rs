//! ClickHouse column types and codecs as data (contract section 6.1).

use core::fmt;

macro_rules! enum_kinds {
    ($($kind:ident,)+) => {
        /// A frozen enum of contract section 1.6 that is stored as an `Enum8` column.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum EnumKind {
            $(
                #[doc = concat!("`crucible_records::", stringify!($kind), "`.")]
                $kind,
            )+
        }

        impl EnumKind {
            /// Every stored enum.
            pub const ALL: &'static [Self] = &[$(Self::$kind,)+];

            /// The wire names and stored values, in value order.
            pub fn variants(self) -> Vec<(&'static str, u8)> {
                match self {
                    $(Self::$kind => crucible_records::$kind::ALL
                        .iter()
                        .map(|v| (v.as_str(), v.value()))
                        .collect(),)+
                }
            }
        }
    };
}

enum_kinds! {
    SpanKind, StatusCode, ObservationType, Level, IoShape, Truncation, CostSource, TracePublic, McpSide,
    SourceFormat, CaptureSource, Era, Transport, McpTargetKind, Direction, ResultType, CancelSource, McpStatus,
    ErrorClass, ErrorSubclass, GroupKeyKind, IdentitySource, FactTargetKind, AuthorKind, HeadKind,
    ScoreDataType, ScoreSource, TagOp,
}

/// The merge function of a `SimpleAggregateFunction` column.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AggFn {
    /// `min`.
    Min,
    /// `max`.
    Max,
    /// `sum`.
    Sum,
    /// `sumMap` over a (keys, counts) tuple: the histogram merge.
    SumMap,
    /// `groupUniqArrayArray(n)`: a bounded set union.
    GroupUniqArrayArray(u8),
}

impl fmt::Display for AggFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Min => f.write_str("min"),
            Self::Max => f.write_str("max"),
            Self::Sum => f.write_str("sum"),
            Self::SumMap => f.write_str("sumMap"),
            Self::GroupUniqArrayArray(n) => write!(f, "groupUniqArrayArray({n})"),
        }
    }
}

/// A ClickHouse column type. `Display` prints the exact type string used in DDL.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Ty {
    /// `UUID`.
    Uuid,
    /// `UInt8`.
    UInt8,
    /// `UInt16`.
    UInt16,
    /// `UInt32`.
    UInt32,
    /// `UInt64`.
    UInt64,
    /// `Int32`.
    Int32,
    /// `Int64`.
    Int64,
    /// `Float32`.
    Float32,
    /// `Float64`.
    Float64,
    /// `String`.
    String,
    /// `FixedString(n)`.
    FixedString(u8),
    /// `Date`.
    Date,
    /// `DateTime('UTC')`.
    DateTime,
    /// `DateTime64(6, 'UTC')`.
    DateTime64,
    /// `Enum8(...)` with the names and values of a frozen enum.
    Enum8(EnumKind),
    /// `LowCardinality(String)`.
    LowCardinalityString,
    /// `Nullable(T)`.
    Nullable(&'static Ty),
    /// `Array(T)`.
    Array(&'static Ty),
    /// `Tuple(A, B)`.
    Tuple2(&'static Ty, &'static Ty),
    /// `SimpleAggregateFunction(f, T)`.
    Saf(AggFn, &'static Ty),
}

impl Ty {
    /// The type string of a Native block header (contract section 5.2): the `TableDef` type verbatim, except
    /// that every `LowCardinality(String)` is sent as `String`. `SimpleAggregateFunction` wrappers are kept.
    pub fn wire(&self) -> WireTy<'_> {
        WireTy(self)
    }

    /// The type under any `SimpleAggregateFunction` and `Nullable` wrapper.
    pub const fn scalar(&self) -> &Ty {
        match self {
            Self::Saf(_, inner) | Self::Nullable(inner) => inner.scalar(),
            other => other,
        }
    }

    /// The merge function, for a `SimpleAggregateFunction` column.
    pub const fn agg_fn(&self) -> Option<AggFn> {
        match self {
            Self::Saf(f, _) => Some(*f),
            _ => None,
        }
    }

    fn write(&self, f: &mut fmt::Formatter<'_>, wire: bool) -> fmt::Result {
        match self {
            Self::Uuid => f.write_str("UUID"),
            Self::UInt8 => f.write_str("UInt8"),
            Self::UInt16 => f.write_str("UInt16"),
            Self::UInt32 => f.write_str("UInt32"),
            Self::UInt64 => f.write_str("UInt64"),
            Self::Int32 => f.write_str("Int32"),
            Self::Int64 => f.write_str("Int64"),
            Self::Float32 => f.write_str("Float32"),
            Self::Float64 => f.write_str("Float64"),
            Self::String => f.write_str("String"),
            Self::FixedString(n) => write!(f, "FixedString({n})"),
            Self::Date => f.write_str("Date"),
            Self::DateTime => f.write_str("DateTime('UTC')"),
            Self::DateTime64 => f.write_str("DateTime64(6, 'UTC')"),
            Self::Enum8(kind) => {
                f.write_str("Enum8(")?;
                for (i, (name, value)) in kind.variants().iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "'{name}' = {value}")?;
                }
                f.write_str(")")
            }
            Self::LowCardinalityString if wire => f.write_str("String"),
            Self::LowCardinalityString => f.write_str("LowCardinality(String)"),
            Self::Nullable(inner) => {
                f.write_str("Nullable(")?;
                inner.write(f, wire)?;
                f.write_str(")")
            }
            Self::Array(inner) => {
                f.write_str("Array(")?;
                inner.write(f, wire)?;
                f.write_str(")")
            }
            Self::Tuple2(a, b) => {
                f.write_str("Tuple(")?;
                a.write(f, wire)?;
                f.write_str(", ")?;
                b.write(f, wire)?;
                f.write_str(")")
            }
            Self::Saf(func, inner) => {
                write!(f, "SimpleAggregateFunction({func}, ")?;
                inner.write(f, wire)?;
                f.write_str(")")
            }
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, false)
    }
}

/// The Native-block form of a [`Ty`]; `Display` prints the block-header type string.
#[derive(Clone, Copy, Debug)]
pub struct WireTy<'a>(&'a Ty);

impl fmt::Display for WireTy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.write(f, true)
    }
}

/// A column compression codec (contract section 6.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Codec {
    /// `ZSTD(1)`, the default.
    Zstd1,
    /// `ZSTD(3)`, for payload strings.
    Zstd3,
    /// `Delta(8), ZSTD(1)`, for `DateTime64` times.
    DeltaZstd1,
    /// `T64, ZSTD(1)`, for counters.
    T64Zstd1,
}

impl Codec {
    /// The convention of contract section 6.1 for a column of this type: times get `Delta`, `UInt32`, `UInt64`
    /// and `Int64` counters get `T64`, everything else plain `ZSTD(1)`. Hash-valued integers and payload strings
    /// name their codec in the column list instead.
    pub const fn conventional(ty: &Ty) -> Self {
        match ty.scalar() {
            Ty::DateTime64 => Self::DeltaZstd1,
            Ty::UInt32 | Ty::UInt64 | Ty::Int64 => Self::T64Zstd1,
            _ => Self::Zstd1,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Zstd1 => "ZSTD(1)",
            Self::Zstd3 => "ZSTD(3)",
            Self::DeltaZstd1 => "Delta(8), ZSTD(1)",
            Self::T64Zstd1 => "T64, ZSTD(1)",
        })
    }
}
