//! Cost (contract 3.4): fixed-point `i64` nano-USD, the stored `Int64` form. Sums are exact integer sums.

/// A cost in nano-USD (1 USD = 1,000,000,000 nano-USD).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct NanoUsd(i64);

impl NanoUsd {
    /// No cost.
    pub const ZERO: Self = Self(0);

    /// The largest representable cost; conversions and saturating sums stop here.
    pub const MAX: Self = Self(i64::MAX);

    /// Nano-USD in one USD.
    pub const PER_USD: i64 = 1_000_000_000;

    /// Wraps a nano-USD count.
    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// The nano-USD count.
    pub const fn nanos(self) -> i64 {
        self.0
    }

    /// `self + rhs`, or `None` when it does not fit.
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// `self + rhs`, saturating at the `i64` bounds.
    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    /// The exact sum, or `None` when it does not fit. Partial sums may leave the `i64` range; only the result has
    /// to fit, so the answer does not depend on the order of the terms.
    pub fn checked_sum<I: IntoIterator<Item = Self>>(costs: I) -> Option<Self> {
        i64::try_from(exact_sum(costs)).ok().map(Self)
    }

    /// The exact sum clamped to the `i64` bounds. It does not depend on the order of the terms.
    pub fn saturating_sum<I: IntoIterator<Item = Self>>(costs: I) -> Self {
        let sum = exact_sum(costs);
        Self(i64::try_from(sum).unwrap_or(if sum < 0 { i64::MIN } else { i64::MAX }))
    }

    /// Converts a provided USD amount given as a binary floating-point number: the exact value of `usd` times
    /// 10^9, rounded half to even, saturating at [`NanoUsd::MAX`]. Negative amounts are clamped to zero, and so
    /// is NaN, which carries no amount.
    pub fn from_usd_f64(usd: f64) -> Self {
        if usd.is_nan() || usd <= 0.0 {
            return Self::ZERO;
        }
        let bits = usd.to_bits();
        let biased = ((bits >> 52) & 0x7ff) as i32;
        let fraction = bits & ((1 << 52) - 1);
        // usd = mantissa * 2^exponent exactly.
        let (mantissa, exponent) = if biased == 0 {
            (fraction, -1074)
        } else {
            (fraction | (1 << 52), biased - 1075)
        };
        if exponent >= 0 {
            // At least 2^52 USD (or infinite): far above the largest cost.
            return Self::MAX;
        }
        // mantissa < 2^53 and 10^9 < 2^30, so the product is below 2^83.
        let scaled = u128::from(mantissa) * u128::from(Self::PER_USD.unsigned_abs());
        let shift = exponent.unsigned_abs();
        if shift >= 128 {
            // scaled < 2^83, so the value is below one half.
            return Self::ZERO;
        }
        let quotient = scaled >> shift;
        let remainder = scaled & ((1u128 << shift) - 1);
        let half = 1u128 << (shift - 1);
        let round_up = remainder > half || (remainder == half && quotient & 1 == 1);
        Self::saturate(quotient + u128::from(round_up))
    }

    /// Converts a provided USD amount given as decimal text, `[+-]digits[.digits][(e|E)[+-]digits]`: the exact
    /// value times 10^9, rounded half to even, saturating at [`NanoUsd::MAX`]. Negative amounts are clamped to
    /// zero. Any other text is `None`.
    pub fn from_usd_decimal(text: &str) -> Option<Self> {
        let decimal = Decimal::parse(text.as_bytes())?;
        Some(if decimal.negative {
            Self::ZERO
        } else {
            decimal.to_nanos()
        })
    }

    fn saturate(value: u128) -> Self {
        Self(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

/// The exact sum of `costs`. An i128 holds the sum of any 2^64 terms; the add saturates beyond that.
fn exact_sum<I: IntoIterator<Item = NanoUsd>>(costs: I) -> i128 {
    costs
        .into_iter()
        .fold(0i128, |acc, c| acc.saturating_add(i128::from(c.0)))
}

/// Decimal text split into its digits and the position of the nano-USD point.
struct Decimal<'a> {
    negative: bool,
    /// Integer digits, ASCII.
    int: &'a [u8],
    /// Fraction digits, ASCII; may be empty.
    frac: &'a [u8],
    /// How many of the digits (integer then fraction) lie before the nano-USD point: the integer digit count
    /// plus the exponent plus 9. It may be negative or exceed the digit count.
    point: i64,
}

impl<'a> Decimal<'a> {
    fn parse(text: &'a [u8]) -> Option<Self> {
        let (negative, rest) = match text.split_first() {
            Some((b'-', rest)) => (true, rest),
            Some((b'+', rest)) => (false, rest),
            _ => (false, text),
        };
        let (int, rest) = rest.split_at(leading_digits(rest));
        if int.is_empty() {
            return None;
        }
        let (frac, rest) = match rest.split_first() {
            Some((b'.', after)) => {
                let (frac, rest) = after.split_at(leading_digits(after));
                if frac.is_empty() {
                    return None;
                }
                (frac, rest)
            }
            _ => (&rest[..0], rest),
        };
        let exponent = match rest.split_first() {
            None => 0,
            Some((b'e' | b'E', after)) => parse_exponent(after)?,
            Some(_) => return None,
        };
        let int_len = i64::try_from(int.len()).ok()?;
        Some(Self {
            negative,
            int,
            frac,
            point: int_len.saturating_add(exponent).saturating_add(9),
        })
    }

    /// The `i`-th digit, integer digits first; zero past the end.
    fn digit(&self, i: usize) -> u8 {
        let byte = match i.checked_sub(self.int.len()) {
            None => self.int.get(i),
            Some(j) => self.frac.get(j),
        };
        byte.map_or(0, |b| b - b'0')
    }

    fn to_nanos(&self) -> NanoUsd {
        // Below 0.1 nano-USD: rounds to zero.
        let Ok(point) = usize::try_from(self.point) else {
            return NanoUsd::ZERO;
        };
        let len = self.int.len() + self.frac.len();
        let mut value: u128 = 0;
        for i in 0..point {
            if i >= len && value == 0 {
                // Only zeros remain.
                break;
            }
            value = value * 10 + u128::from(self.digit(i));
            if value > i64::MAX as u128 {
                return NanoUsd::MAX;
            }
        }
        let first = self.digit(point);
        let rest_nonzero = (point.saturating_add(1)..len).any(|i| self.digit(i) != 0);
        let round_up = first > 5 || (first == 5 && (rest_nonzero || value & 1 == 1));
        NanoUsd::saturate(value + u128::from(round_up))
    }
}

fn leading_digits(text: &[u8]) -> usize {
    text.iter().take_while(|b| b.is_ascii_digit()).count()
}

/// `[+-]digits`, clamped far outside any useful range so the point arithmetic cannot overflow.
fn parse_exponent(text: &[u8]) -> Option<i64> {
    let (negative, digits) = match text.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, text),
    };
    if digits.is_empty() || leading_digits(digits) != digits.len() {
        return None;
    }
    let magnitude = digits.iter().fold(0i64, |acc, &b| {
        acc.saturating_mul(10)
            .saturating_add(i64::from(b - b'0'))
            .min(1 << 40)
    });
    Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn n(nanos: i64) -> NanoUsd {
        NanoUsd::from_nanos(nanos)
    }

    #[test]
    fn sums_are_exact_and_order_independent() {
        let terms = [NanoUsd::MAX, n(1), n(-1)];
        assert_eq!(NanoUsd::checked_sum(terms), Some(NanoUsd::MAX));
        assert_eq!(NanoUsd::checked_sum([NanoUsd::MAX, n(1)]), None);
        assert_eq!(NanoUsd::saturating_sum([NanoUsd::MAX, n(1)]), NanoUsd::MAX);
        assert_eq!(NanoUsd::saturating_sum([n(i64::MIN), n(-1)]), n(i64::MIN));
        assert_eq!(NanoUsd::checked_sum([]), Some(NanoUsd::ZERO));
        assert_eq!(n(2).checked_add(n(3)), Some(n(5)));
        assert_eq!(NanoUsd::MAX.checked_add(n(1)), None);
        assert_eq!(NanoUsd::MAX.saturating_add(n(1)), NanoUsd::MAX);
    }

    #[test]
    fn f64_conversion_rounds_half_to_even() {
        assert_eq!(NanoUsd::from_usd_f64(1.0), n(1_000_000_000));
        assert_eq!(NanoUsd::from_usd_f64(0.1), n(100_000_000));
        // 1/1024 USD is exactly 976,562.5 nano-USD: a tie, which goes to the even neighbour.
        assert_eq!(NanoUsd::from_usd_f64(1.0 / 1024.0), n(976_562));
        assert_eq!(NanoUsd::from_usd_f64(3.0 / 1024.0), n(2_929_688));
        assert_eq!(NanoUsd::from_usd_f64(-3.5), NanoUsd::ZERO);
        assert_eq!(NanoUsd::from_usd_f64(-0.0), NanoUsd::ZERO);
        assert_eq!(NanoUsd::from_usd_f64(f64::NAN), NanoUsd::ZERO);
        assert_eq!(NanoUsd::from_usd_f64(f64::INFINITY), NanoUsd::MAX);
        assert_eq!(NanoUsd::from_usd_f64(f64::MAX), NanoUsd::MAX);
        assert_eq!(NanoUsd::from_usd_f64(1e10), NanoUsd::MAX);
        assert_eq!(NanoUsd::from_usd_f64(f64::MIN_POSITIVE), NanoUsd::ZERO);
        assert_eq!(NanoUsd::from_usd_f64(5e-324), NanoUsd::ZERO);
    }

    #[test]
    fn decimal_conversion_is_exact() {
        let d = |s: &str| NanoUsd::from_usd_decimal(s);
        assert_eq!(d("1"), Some(n(1_000_000_000)));
        assert_eq!(d("0.000000001"), Some(n(1)));
        assert_eq!(d("0.0000000005"), Some(n(0)));
        assert_eq!(d("0.0000000015"), Some(n(2)));
        assert_eq!(d("0.00000000050001"), Some(n(1)));
        assert_eq!(d("0.0009765625"), Some(n(976_562)));
        assert_eq!(d("1.5e-9"), Some(n(2)));
        assert_eq!(d("25E-10"), Some(n(2)));
        assert_eq!(d("4e-11"), Some(n(0)));
        assert_eq!(d("+2.5"), Some(n(2_500_000_000)));
        assert_eq!(d("-2.5"), Some(n(0)));
        assert_eq!(d("0e999999999999999"), Some(n(0)));
        assert_eq!(d("1e999999999999999"), Some(NanoUsd::MAX));
        assert_eq!(d("1e-999999999999999"), Some(n(0)));
        assert_eq!(d("9223372036.854775807"), Some(NanoUsd::MAX));
        assert_eq!(d("9223372036.854775806"), Some(n(i64::MAX - 1)));
        assert_eq!(d("9223372036.854775808"), Some(NanoUsd::MAX));
        for bad in [
            "", "-", ".5", "1.", "1e", "1e+", "1.5.2", " 1", "1 ", "0x10", "1,5", "--1",
        ] {
            assert_eq!(d(bad), None, "{bad:?}");
        }
    }

    proptest! {
        #[test]
        fn checked_sum_is_the_exact_sum(terms in proptest::collection::vec(any::<i64>(), 0..8)) {
            let exact: i128 = terms.iter().map(|&t| i128::from(t)).sum();
            let costs = terms.iter().map(|&t| n(t));
            prop_assert_eq!(NanoUsd::checked_sum(costs.clone()), i64::try_from(exact).ok().map(n));
            let clamped = exact.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
            prop_assert_eq!(NanoUsd::saturating_sum(costs), n(clamped as i64));
        }

        #[test]
        fn f64_conversion_is_exact_for_representable_amounts(nanos in 0i64..(1 << 52)) {
            // nanos / 1e9 rounds once; below 2^52 the error is under half a nano-USD.
            prop_assert_eq!(NanoUsd::from_usd_f64(nanos as f64 / 1e9), n(nanos));
        }

        #[test]
        fn decimal_text_rounds_its_extra_digits_half_to_even(
            nanos in 0i64..i64::MAX,
            extra in "[0-9]{0,6}",
        ) {
            let text = format!("{}.{:09}{extra}", nanos / 1_000_000_000, nanos % 1_000_000_000);
            let first = extra.bytes().next().map_or(0, |b| b - b'0');
            let rest_nonzero = extra.bytes().skip(1).any(|b| b != b'0');
            let up = first > 5 || (first == 5 && (rest_nonzero || nanos & 1 == 1));
            let expected = if up { n(nanos).saturating_add(n(1)) } else { n(nanos) };
            prop_assert_eq!(NanoUsd::from_usd_decimal(&text), Some(expected));
            prop_assert_eq!(NanoUsd::from_usd_decimal(&format!("{nanos}e-9")), Some(n(nanos)));
        }
    }
}
