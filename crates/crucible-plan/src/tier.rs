//! How a read over a table's hourly tier divides its window (`crucible_schema::hourly_tier`, contract 3.10).
//!
//! A minute rollup's window covers the minutes `[floor_minute(from), floor_minute(to - 1 µs)]`: the renderer bounds
//! a minute column so (contract 8.3), and the model does the same. Of those minutes, every whole hour is read from
//! the tier, one row per key and hour, and the minutes before the first whole hour and after the last from the
//! minute table. Each minute is read from exactly one of the two, so an aggregate over both merges what an
//! aggregate over the minute table alone merges. The renderer and the store model take the split from here.

use crucible_records::{TimeWindow, Timestamp};

const MINUTE: i64 = 60_000_000;
const HOUR: i64 = 3_600_000_000;

/// The whole hours of a window: `[first, end)`, both the start of an hour.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HourSplit {
    hours: TimeWindow,
}

impl HourSplit {
    /// The whole hours among the minutes of `window`, or `None` when it holds none.
    pub fn of(window: TimeWindow) -> Option<Self> {
        let first_minute = window.from().floor_minute();
        let last_minute = window.to().saturating_add_micros(-1).floor_minute();
        let first = first_minute.saturating_add_micros(HOUR - 1).floor_hour();
        let end = last_minute.saturating_add_micros(MINUTE).floor_hour();
        TimeWindow::new(first, end).ok().map(|hours| Self { hours })
    }

    /// The start of the first whole hour.
    pub const fn first(&self) -> Timestamp {
        self.hours.from()
    }

    /// The end of the last whole hour: the first instant after it.
    pub const fn end(&self) -> Timestamp {
        self.hours.to()
    }

    /// The whole hours as a window, `[first, end)`: what the tier is read over.
    pub const fn hours(&self) -> TimeWindow {
        self.hours
    }

    /// Whether the minute that starts at `minute` is read from the tier.
    pub fn covers(&self, minute: Timestamp) -> bool {
        self.hours.contains(minute)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn window(from: i64, to: i64) -> TimeWindow {
        TimeWindow::new(Timestamp::from_micros(from), Timestamp::from_micros(to)).unwrap()
    }

    #[test]
    fn whole_hours_inside_a_day() {
        let day = 1_794_873_600_000_000; // 2026-11-17T00:00:00Z
        let split = HourSplit::of(window(day + 23 * MINUTE + 5, day + 86_400_000_000 + 23 * MINUTE)).unwrap();
        assert_eq!(split.first(), Timestamp::from_micros(day + HOUR));
        assert_eq!(split.end(), Timestamp::from_micros(day + 86_400_000_000));
        // An hour-aligned window is all whole hours.
        let aligned = HourSplit::of(window(day, day + 3 * HOUR)).unwrap();
        assert_eq!((aligned.first(), aligned.end()), (Timestamp::from_micros(day), Timestamp::from_micros(day + 3 * HOUR)));
        // A window that ends one microsecond into a minute reads that minute, so the hour it completes is whole.
        let into = HourSplit::of(window(day, day + HOUR - MINUTE + 1)).unwrap();
        assert_eq!(into.end(), Timestamp::from_micros(day + HOUR));
        assert_eq!(HourSplit::of(window(day, day + HOUR - MINUTE)), None);
        assert_eq!(HourSplit::of(window(day + MINUTE, day + 2 * HOUR - MINUTE)), None);
    }

    proptest! {
        /// Every minute of the window is read once: from the tier when its hour is whole inside the window's
        /// minutes, else from the minute table; and the tier is read only over minutes of the window.
        #[test]
        fn each_minute_of_the_window_is_read_once(from in 0i64..400 * 86_400_000_000, len in 1i64..30 * 86_400_000_000) {
            let w = window(from, from + len);
            let first_minute = w.from().floor_minute().micros();
            let last_minute = w.to().saturating_add_micros(-1).floor_minute().micros();
            let split = HourSplit::of(w);
            if let Some(split) = split {
                prop_assert_eq!(split.first().micros() % HOUR, 0);
                prop_assert_eq!(split.end().micros() % HOUR, 0);
                prop_assert!(split.first().micros() >= first_minute);
                prop_assert!(split.end().micros() - MINUTE <= last_minute);
                prop_assert_eq!(split.hours().len_micros(), split.end().micros() - split.first().micros());
            }
            // Walk the hours that touch the window: each is whole (read from the tier) or partial (from minutes).
            let mut hour = first_minute - first_minute.rem_euclid(HOUR);
            while hour <= last_minute {
                let whole = hour >= first_minute && hour + HOUR - MINUTE <= last_minute;
                let tiered = split.is_some_and(|s| s.covers(Timestamp::from_micros(hour)));
                prop_assert_eq!(whole, tiered, "hour {} of window {:?}", hour, w);
                for minute in [hour, hour + HOUR - MINUTE] {
                    let in_window = first_minute <= minute && minute <= last_minute;
                    let from_tier = split.is_some_and(|s| s.covers(Timestamp::from_micros(minute)));
                    prop_assert!(!from_tier || in_window);
                }
                hour += HOUR;
            }
            // The split exists exactly when the window holds a whole hour.
            let any_whole = {
                let first = (first_minute + HOUR - 1).div_euclid(HOUR) * HOUR;
                first + HOUR - MINUTE <= last_minute
            };
            prop_assert_eq!(split.is_some(), any_whole);
        }
    }
}
