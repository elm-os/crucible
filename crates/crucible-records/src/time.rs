//! Time (contract section 1.2): `Timestamp` is `i64` microseconds since the Unix epoch, UTC; windows are
//! half-open. Calendar arithmetic is integer-only (days from civil and back), so it is exact and total.

use core::fmt;

const MICROS_PER_SECOND: i64 = 1_000_000;
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;

/// Microseconds since the Unix epoch, UTC.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The Unix epoch, the lower bound of the valid range.
    pub const EPOCH: Self = Self(0);

    /// Exclusive upper bound of the valid range: 2100-01-01T00:00:00Z.
    pub const MAX_EXCLUSIVE: Self = Self(4_102_444_800_000_000);

    /// Wraps a microsecond count.
    pub const fn from_micros(micros: i64) -> Self {
        Self(micros)
    }

    /// The microsecond count.
    pub const fn micros(self) -> i64 {
        self.0
    }

    /// Converts Unix nanoseconds, truncating toward zero. Every `u64` input fits.
    pub const fn from_unix_nanos(nanos: u64) -> Self {
        // u64::MAX / 1000 is below i64::MAX, so the cast cannot wrap.
        Self((nanos / 1_000) as i64)
    }

    /// Whether `0 <= t < 2100-01-01`.
    pub const fn is_valid(self) -> bool {
        self.0 >= 0 && self.0 < Self::MAX_EXCLUSIVE.0
    }

    /// Clamps into the valid range. The flag is `true` when the value changed (the span is then flagged
    /// `clock_skew`).
    pub const fn clamp_valid(self) -> (Self, bool) {
        if self.0 < 0 {
            (Self::EPOCH, true)
        } else if self.0 >= Self::MAX_EXCLUSIVE.0 {
            (Self(Self::MAX_EXCLUSIVE.0 - 1), true)
        } else {
            (self, false)
        }
    }

    /// The start of the minute containing this instant.
    pub const fn floor_minute(self) -> Self {
        Self(self.0 - self.0.rem_euclid(MICROS_PER_MINUTE))
    }

    /// The start of the UTC day containing this instant.
    pub const fn floor_day(self) -> Self {
        Self(self.0 - self.0.rem_euclid(MICROS_PER_DAY))
    }

    /// Days since 1970-01-01 (floor division, so negative instants map to negative days).
    pub const fn day_number(self) -> i64 {
        self.0.div_euclid(MICROS_PER_DAY)
    }

    /// The UTC calendar date of this instant.
    pub const fn date(self) -> CivilDate {
        CivilDate::from_day_number(self.day_number())
    }

    /// Adds microseconds, saturating at the `i64` bounds.
    pub const fn saturating_add_micros(self, micros: i64) -> Self {
        Self(self.0.saturating_add(micros))
    }

    /// Adds microseconds, or `None` on overflow.
    pub const fn checked_add_micros(self, micros: i64) -> Option<Self> {
        match self.0.checked_add(micros) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }
}

/// RFC 3339 UTC with exactly six fractional digits and a `Z` suffix, the wire form.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let date = self.date();
        let within_day = self.0.rem_euclid(MICROS_PER_DAY);
        let secs = within_day / MICROS_PER_SECOND;
        let micros = within_day % MICROS_PER_SECOND;
        write!(
            f,
            "{date}T{:02}:{:02}:{:02}.{micros:06}Z",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        )
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({self})")
    }
}

/// A proleptic Gregorian calendar date.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CivilDate {
    year: i32,
    month: u8,
    day: u8,
}

impl CivilDate {
    /// A validated date, or `None` when the day does not exist.
    pub const fn new(year: i32, month: u8, day: u8) -> Option<Self> {
        match (month, day) {
            (1..=12, 1..=31) => {}
            _ => return None,
        }
        let candidate = Self { year, month, day };
        let back = Self::from_day_number(candidate.day_number());
        if back.year == year && back.month == month && back.day == day {
            Some(candidate)
        } else {
            None
        }
    }

    /// The date `days` days after 1970-01-01. Total over the range of `i64` days that fit an `i32` year.
    pub const fn from_day_number(days: i64) -> Self {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097); // [0, 146096]
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11]
        let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
        let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
        let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
        Self {
            year: year as i32,
            month: month as u8,
            day: day as u8,
        }
    }

    /// Days since 1970-01-01.
    pub const fn day_number(self) -> i64 {
        let m = self.month as i64;
        let y = self.year as i64 - if m <= 2 { 1 } else { 0 };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400); // [0, 399]
        let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
        let doy = (153 * mp + 2) / 5 + self.day as i64 - 1; // [0, 365]
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
        era * 146_097 + doe - 719_468
    }

    /// The year.
    pub const fn year(self) -> i32 {
        self.year
    }

    /// The month, 1 to 12.
    pub const fn month(self) -> u8 {
        self.month
    }

    /// The day of the month, 1 to 31.
    pub const fn day(self) -> u8 {
        self.day
    }

    /// The Monday on or before this date (the `toMonday` partition rule).
    pub const fn monday_on_or_before(self) -> Self {
        let days = self.day_number();
        // 1970-01-01 was a Thursday: with Monday as 0 its index is 3.
        Self::from_day_number(days - (days + 3).rem_euclid(7))
    }

    /// The first instant of this date, UTC.
    pub const fn start(self) -> Timestamp {
        Timestamp(self.day_number().saturating_mul(MICROS_PER_DAY))
    }
}

/// `YYYY-MM-DD`.
impl fmt::Display for CivilDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// A window that is not `from < to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a time window needs from < to")]
pub struct InvalidWindow;

/// A half-open time window `[from, to)`; both ends are required and `from < to`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TimeWindow {
    from: Timestamp,
    to: Timestamp,
}

impl TimeWindow {
    /// A window, or an error unless `from < to`.
    pub const fn new(from: Timestamp, to: Timestamp) -> Result<Self, InvalidWindow> {
        if from.0 < to.0 {
            Ok(Self { from, to })
        } else {
            Err(InvalidWindow)
        }
    }

    /// Inclusive start.
    pub const fn from(&self) -> Timestamp {
        self.from
    }

    /// Exclusive end.
    pub const fn to(&self) -> Timestamp {
        self.to
    }

    /// Whether `t` lies in `[from, to)`.
    pub const fn contains(&self, t: Timestamp) -> bool {
        self.from.0 <= t.0 && t.0 < self.to.0
    }

    /// Length in microseconds; positive by construction.
    pub const fn len_micros(&self) -> i64 {
        self.to.0.saturating_sub(self.from.0)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn formats_rfc3339_with_six_digits() {
        assert_eq!(Timestamp::EPOCH.to_string(), "1970-01-01T00:00:00.000000Z");
        let t = Timestamp::from_micros(1_794_907_800_000_123);
        assert_eq!(t.to_string(), "2026-11-17T09:30:00.000123Z");
        assert_eq!(
            Timestamp::from_micros(-1).to_string(),
            "1969-12-31T23:59:59.999999Z"
        );
    }

    #[test]
    fn known_dates() {
        assert_eq!(
            CivilDate::from_day_number(0),
            CivilDate::new(1970, 1, 1).unwrap()
        );
        let d = CivilDate::new(2026, 11, 17).unwrap();
        assert_eq!(d.day_number(), 20_774);
        assert_eq!(
            d.monday_on_or_before(),
            CivilDate::new(2026, 11, 16).unwrap()
        );
        assert_eq!(CivilDate::new(2024, 2, 29).unwrap().day_number(), 19_782);
        assert!(CivilDate::new(2026, 2, 29).is_none());
        assert!(CivilDate::new(2026, 13, 1).is_none());
        assert_eq!(
            Timestamp::MAX_EXCLUSIVE.date(),
            CivilDate::new(2100, 1, 1).unwrap()
        );
    }

    #[test]
    fn clamp_and_nanos() {
        assert_eq!(
            Timestamp::from_micros(-5).clamp_valid(),
            (Timestamp::EPOCH, true)
        );
        assert!(Timestamp::MAX_EXCLUSIVE.clamp_valid().1);
        assert_eq!(Timestamp::from_unix_nanos(1_999), Timestamp::from_micros(1));
        assert_eq!(
            Timestamp::from_unix_nanos(u64::MAX).micros(),
            18_446_744_073_709_551
        );
    }

    #[test]
    fn windows_are_half_open() {
        let w = TimeWindow::new(Timestamp::from_micros(10), Timestamp::from_micros(20)).unwrap();
        assert!(w.contains(Timestamp::from_micros(10)));
        assert!(!w.contains(Timestamp::from_micros(20)));
        assert_eq!(
            TimeWindow::new(Timestamp::EPOCH, Timestamp::EPOCH),
            Err(InvalidWindow)
        );
    }

    proptest! {
        #[test]
        fn day_numbers_round_trip(days in -1_000_000i64..1_000_000) {
            let d = CivilDate::from_day_number(days);
            prop_assert_eq!(d.day_number(), days);
            prop_assert_eq!(CivilDate::new(d.year(), d.month(), d.day()), Some(d));
        }

        #[test]
        fn consecutive_days_are_consecutive_dates(days in -1_000_000i64..1_000_000) {
            let a = CivilDate::from_day_number(days);
            let b = CivilDate::from_day_number(days + 1);
            prop_assert!(a < b);
        }

        #[test]
        fn monday_is_within_six_days(days in 0i64..60_000) {
            let d = CivilDate::from_day_number(days);
            let m = d.monday_on_or_before();
            let delta = d.day_number() - m.day_number();
            prop_assert!((0..7).contains(&delta));
            prop_assert_eq!((m.day_number() + 3).rem_euclid(7), 0);
        }

        #[test]
        fn floors_are_consistent(us in 0i64..Timestamp::MAX_EXCLUSIVE.micros()) {
            let t = Timestamp::from_micros(us);
            prop_assert!(t.floor_minute() <= t && t.floor_day() <= t.floor_minute());
            prop_assert_eq!(t.floor_day(), t.date().start());
        }
    }
}
