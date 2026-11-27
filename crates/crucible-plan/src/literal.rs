//! Typed literals. Every value in a plan is one of these; the renderer binds each as a server-side parameter of
//! its column's type (contract section 8.3), so no value is ever formatted into SQL text.

use crucible_records::{CivilDate, SpanId, Timestamp, TraceId, Uuid};
use crucible_schema::Ty;

/// A typed value.
#[derive(Clone, PartialEq, Debug)]
pub enum Literal {
    /// A `UUID`.
    Uuid(Uuid),
    /// Raw bytes for a `FixedString(n)` column; the length must be `n`.
    Bytes(Vec<u8>),
    /// A `String` or `LowCardinality(String)`.
    String(String),
    /// An unsigned integer; it must fit the column's width.
    UInt(u64),
    /// A signed integer; it must fit the column's width.
    Int(i64),
    /// A float.
    Float(f64),
    /// An instant, for `DateTime64(6, 'UTC')`, or for `DateTime('UTC')` when it is a whole second.
    Time(Timestamp),
    /// A `Date`.
    Date(CivilDate),
    /// The stored value of an `Enum8` column; it must be one of the enum's values.
    Enum(u8),
}

impl Literal {
    /// Whether the value can be bound as a parameter of this column type. `Nullable` and
    /// `SimpleAggregateFunction` wrappers are looked through; arrays never fit (see `Pred::HasAny`).
    pub fn fits(&self, ty: &Ty) -> bool {
        match (self, ty.scalar()) {
            (Self::Uuid(_), Ty::Uuid)
            | (Self::String(_), Ty::String | Ty::LowCardinalityString)
            | (Self::UInt(_), Ty::UInt64)
            | (Self::Int(_), Ty::Int64)
            | (Self::Float(_), Ty::Float32 | Ty::Float64)
            | (Self::Time(_), Ty::DateTime64)
            | (Self::Date(_), Ty::Date) => true,
            (Self::Bytes(b), Ty::FixedString(n)) => b.len() == usize::from(*n),
            (Self::UInt(v), Ty::UInt8) => u8::try_from(*v).is_ok(),
            (Self::UInt(v), Ty::UInt16) => u16::try_from(*v).is_ok(),
            (Self::UInt(v), Ty::UInt32) => u32::try_from(*v).is_ok(),
            (Self::Int(v), Ty::Int32) => i32::try_from(*v).is_ok(),
            (Self::Time(t), Ty::DateTime) => t.micros().rem_euclid(1_000_000) == 0,
            (Self::Enum(v), Ty::Enum8(kind)) => kind.variants().iter().any(|(_, x)| x == v),
            _ => false,
        }
    }
}

impl Literal {
    /// The literal that compares equal to a result value: what a read uses to restrict a later read to the
    /// keys an earlier one returned. `None` for a null, an array, a tuple, and for bytes that are a `String`
    /// with invalid UTF-8 when `ty` is a string type.
    pub fn of(value: &crate::Value, ty: &Ty) -> Option<Self> {
        let literal = match value {
            crate::Value::UInt(v) => Self::UInt(*v),
            crate::Value::Int(v) => Self::Int(*v),
            crate::Value::Float(v) => Self::Float(*v),
            crate::Value::String(v) => Self::String(v.clone()),
            crate::Value::Bytes(v) => Self::Bytes(v.clone()),
            crate::Value::Uuid(v) => Self::Uuid(*v),
            crate::Value::Date(v) => Self::Date(*v),
            crate::Value::Time(v) => Self::Time(*v),
            crate::Value::Enum(v) => Self::Enum(*v),
            crate::Value::Null | crate::Value::Array(_) | crate::Value::Tuple2(..) => return None,
        };
        literal.fits(ty).then_some(literal)
    }
}

impl From<Uuid> for Literal {
    fn from(v: Uuid) -> Self {
        Self::Uuid(v)
    }
}

impl From<TraceId> for Literal {
    fn from(v: TraceId) -> Self {
        Self::Bytes(v.to_bytes().to_vec())
    }
}

impl From<SpanId> for Literal {
    fn from(v: SpanId) -> Self {
        Self::Bytes(v.to_bytes().to_vec())
    }
}

impl From<&str> for Literal {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}

impl From<u64> for Literal {
    fn from(v: u64) -> Self {
        Self::UInt(v)
    }
}

impl From<u32> for Literal {
    fn from(v: u32) -> Self {
        Self::UInt(u64::from(v))
    }
}

impl From<Timestamp> for Literal {
    fn from(v: Timestamp) -> Self {
        Self::Time(v)
    }
}

impl From<CivilDate> for Literal {
    fn from(v: CivilDate) -> Self {
        Self::Date(v)
    }
}

/// The next power of two at or above `len` (at least 1). The renderer pads the token lists of a text search, one
/// parameter per token, to this length to bound the number of distinct statements (contract section 8.3).
pub const fn padded_in_len(len: usize) -> usize {
    if len <= 1 { 1 } else { len.next_power_of_two() }
}

/// Pads an `In` list to [`padded_in_len`] by repeating its last value, which does not change the set. An empty
/// list stays empty (plans never hold one).
pub fn pad_in_list(values: &[Literal]) -> Vec<Literal> {
    let mut out = values.to_vec();
    if let Some(last) = values.last() {
        out.resize(padded_in_len(values.len()), last.clone());
    }
    out
}
