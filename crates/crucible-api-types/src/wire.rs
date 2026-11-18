//! The integer and cost rules of contract 1.3: 64-bit values travel as JSON numbers that a JavaScript `number`
//! holds exactly, and costs travel as USD.

use core::fmt;

use serde::de::{self, Deserializer, Unexpected};
use serde::{Deserialize, Serialize, Serializer};
use ts_rs::TS;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ObjectBuilder, Schema, Type};
use utoipa::{PartialSchema, ToSchema};

/// The largest integer a JavaScript `number` holds exactly: 2^53 - 1.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// An unsigned count, size, token total or duration on the wire: a JSON number at most 2^53 - 1. A larger
/// value saturates there, and the object that carries it says so with `saturated: true`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, TS)]
#[ts(type = "number")]
pub struct SafeU64(u64);

impl SafeU64 {
    /// The value clamped to 2^53 - 1, and whether it was clamped.
    pub const fn saturating(value: u64) -> (Self, bool) {
        if value > MAX_SAFE_INTEGER {
            (Self(MAX_SAFE_INTEGER), true)
        } else {
            (Self(value), false)
        }
    }

    /// The value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for SafeU64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl Serialize for SafeU64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for SafeU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(deserializer)?;
        if value > MAX_SAFE_INTEGER {
            return Err(de::Error::invalid_value(
                Unexpected::Unsigned(value),
                &"an integer at most 2^53 - 1",
            ));
        }
        Ok(Self(value))
    }
}

/// Written out because the schema derive cannot state a bound beyond `i32`.
impl PartialSchema for SafeU64 {
    fn schema() -> RefOr<Schema> {
        ObjectBuilder::new()
            .schema_type(Type::Integer)
            .description(Some("An unsigned integer at most 2^53 - 1."))
            .minimum(Some(0u64))
            .maximum(Some(MAX_SAFE_INTEGER))
            .into()
    }
}

impl ToSchema for SafeU64 {}

/// A signed count or size on the wire: a JSON number between -(2^53 - 1) and 2^53 - 1. A value outside
/// saturates at the nearer bound, and the object that carries it says so with `saturated: true`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, TS)]
#[ts(type = "number")]
pub struct SafeI64(i64);

impl SafeI64 {
    /// The value clamped to the safe range, and whether it was clamped.
    pub const fn saturating(value: i64) -> (Self, bool) {
        let max = MAX_SAFE_INTEGER as i64;
        if value > max {
            (Self(max), true)
        } else if value < -max {
            (Self(-max), true)
        } else {
            (Self(value), false)
        }
    }

    /// The value.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Debug for SafeI64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl Serialize for SafeI64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for SafeI64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = i64::deserialize(deserializer)?;
        if value.unsigned_abs() > MAX_SAFE_INTEGER {
            return Err(de::Error::invalid_value(
                Unexpected::Signed(value),
                &"an integer between -(2^53 - 1) and 2^53 - 1",
            ));
        }
        Ok(Self(value))
    }
}

/// Written out because the schema derive cannot state a bound beyond `i32`.
impl PartialSchema for SafeI64 {
    fn schema() -> RefOr<Schema> {
        let max = MAX_SAFE_INTEGER as i64;
        ObjectBuilder::new()
            .schema_type(Type::Integer)
            .description(Some("A signed integer between -(2^53 - 1) and 2^53 - 1."))
            .minimum(Some(-max))
            .maximum(Some(max))
            .into()
    }
}

impl ToSchema for SafeI64 {}

/// Collects whether any wire integer of one object saturated, for its `saturated` field.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Saturation {
    saturated: bool,
}

impl Saturation {
    /// `value` on the wire; remembers a clamp.
    pub fn u64(&mut self, value: u64) -> SafeU64 {
        let (safe, clamped) = SafeU64::saturating(value);
        self.saturated |= clamped;
        safe
    }

    /// `value` on the wire; remembers a clamp.
    pub fn i64(&mut self, value: i64) -> SafeI64 {
        let (safe, clamped) = SafeI64::saturating(value);
        self.saturated |= clamped;
        safe
    }

    /// Whether any value so far was clamped.
    pub const fn saturated(&self) -> bool {
        self.saturated
    }
}

const NANOS_PER_USD: u64 = 1_000_000_000;

/// A cost stored in nano-USD (contract 3.4) as USD on the wire (contract 1.3): `nanos / 1e9` rounded once to
/// the nearest IEEE 754 double, ties to even. Exact to one nano-USD below 2^53 nano-USD.
pub fn usd_from_nanos(nanos: i64) -> f64 {
    let magnitude = nanos.unsigned_abs();
    let usd = if magnitude <= MAX_SAFE_INTEGER {
        // Both operands are exact doubles, and IEEE 754 division rounds once.
        magnitude as f64 / NANOS_PER_USD as f64
    } else {
        // Scale the dividend up so that the quotient keeps far more than 53 significant bits, fold the
        // remainder into the lowest bit so that it still decides a tie, and let one conversion round.
        let shift = magnitude.leading_zeros() + 64;
        let scaled = u128::from(magnitude) << shift;
        let quotient = scaled / u128::from(NANOS_PER_USD);
        let sticky = u128::from(!scaled.is_multiple_of(u128::from(NANOS_PER_USD)));
        // `shift` is between 64 and 74, so 2^-shift is a normal double and the product is exact.
        let scale = f64::from_bits(u64::from(1023 - shift) << 52);
        (quotient | sticky) as f64 * scale
    };
    if nanos < 0 { -usd } else { usd }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The correctly rounded value, from the decimal text: Rust parses floats with correct rounding.
    fn decimal_usd(nanos: i64) -> f64 {
        let magnitude = nanos.unsigned_abs();
        let sign = if nanos < 0 { "-" } else { "" };
        let text = format!(
            "{sign}{}.{:09}",
            magnitude / NANOS_PER_USD,
            magnitude % NANOS_PER_USD
        );
        text.parse().unwrap()
    }

    #[test]
    fn costs_convert_at_the_edges() {
        assert_eq!(usd_from_nanos(0), 0.0);
        assert_eq!(usd_from_nanos(1), 1e-9);
        assert_eq!(usd_from_nanos(1_500_000_000), 1.5);
        assert_eq!(usd_from_nanos(-7), -7e-9);
        for nanos in [
            i64::MIN,
            i64::MIN + 1,
            i64::MAX,
            (1 << 53) - 1,
            1 << 53,
            (1 << 53) + 1,
            -(1 << 53),
        ] {
            assert_eq!(usd_from_nanos(nanos), decimal_usd(nanos), "{nanos}");
        }
    }

    #[test]
    fn integers_saturate_at_two_to_the_53_minus_one() {
        assert_eq!(
            SafeU64::saturating(MAX_SAFE_INTEGER),
            (SafeU64(MAX_SAFE_INTEGER), false)
        );
        assert_eq!(
            SafeU64::saturating(u64::MAX),
            (SafeU64(MAX_SAFE_INTEGER), true)
        );
        let max = MAX_SAFE_INTEGER as i64;
        assert_eq!(SafeI64::saturating(i64::MIN), (SafeI64(-max), true));
        assert_eq!(SafeI64::saturating(-max), (SafeI64(-max), false));
        let mut saturation = Saturation::default();
        assert_eq!(saturation.u64(3).get(), 3);
        assert!(!saturation.saturated());
        assert_eq!(saturation.i64(i64::MAX).get(), max);
        assert!(saturation.saturated());
        assert_eq!(saturation.u64(4).get(), 4);
        assert!(saturation.saturated(), "a clamp is remembered");
    }

    #[test]
    fn wire_integers_are_json_numbers_and_refuse_unsafe_values() {
        assert_eq!(serde_json::to_string(&SafeU64(42)).unwrap(), "42");
        assert_eq!(serde_json::to_string(&SafeI64(-42)).unwrap(), "-42");
        assert_eq!(
            serde_json::from_str::<SafeU64>("9007199254740991").unwrap(),
            SafeU64(MAX_SAFE_INTEGER)
        );
        assert!(serde_json::from_str::<SafeU64>("9007199254740992").is_err());
        assert!(serde_json::from_str::<SafeI64>("-9007199254740992").is_err());
        assert!(serde_json::from_str::<SafeU64>("\"1\"").is_err());
    }

    proptest! {
        #[test]
        fn every_cost_is_the_nearest_double(nanos in any::<i64>()) {
            prop_assert_eq!(usd_from_nanos(nanos), decimal_usd(nanos));
        }

        #[test]
        fn small_costs_are_one_division(nanos in -(1i64 << 53)..(1i64 << 53)) {
            prop_assert_eq!(usd_from_nanos(nanos), nanos as f64 / 1e9);
        }

        #[test]
        fn saturation_clamps_exactly_the_unsafe_values(value in any::<u64>(), signed in any::<i64>()) {
            let (safe, clamped) = SafeU64::saturating(value);
            prop_assert_eq!(safe.get(), value.min(MAX_SAFE_INTEGER));
            prop_assert_eq!(clamped, value > MAX_SAFE_INTEGER);
            let (safe, clamped) = SafeI64::saturating(signed);
            let max = MAX_SAFE_INTEGER as i64;
            prop_assert_eq!(safe.get(), signed.clamp(-max, max));
            prop_assert_eq!(clamped, signed.unsigned_abs() > MAX_SAFE_INTEGER);
            let json = serde_json::to_string(&safe).unwrap();
            prop_assert_eq!(serde_json::from_str::<SafeI64>(&json).unwrap(), safe);
        }
    }
}
