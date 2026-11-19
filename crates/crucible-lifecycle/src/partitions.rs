//! Partitions and time: the window of a partition and the partitions of a span of time (contract 6.3). Pure.

use std::collections::BTreeSet;

use crucible_records::{CivilDate, PartitionId, TableId, TimeWindow, Timestamp};
use crucible_schema::{Granularity, PartitionRule, route};

fn day_start(day: i64) -> Timestamp {
    let start = CivilDate::from_day_number(day).start();
    if start > Timestamp::MAX_EXCLUSIVE {
        Timestamp::MAX_EXCLUSIVE
    } else {
        start
    }
}

/// The time a partition of a table covers: `[first day, day after the last)`. `None` for an unpartitioned
/// table and for an id the table's partition rule does not produce.
pub fn partition_window(
    table_id: TableId,
    granularity: Granularity,
    partition: PartitionId,
) -> Option<TimeWindow> {
    let rule = PartitionRule::of_table(table_id, granularity);
    let (first, end) = if let Some(date) = partition.as_date() {
        let first = date.day_number();
        match rule {
            PartitionRule::Day => (first, first + 1),
            // The first four days of 1970 form one short week (see `crucible_schema::route`).
            PartitionRule::Week if first == 0 => (0, 4),
            PartitionRule::Week => (first, first + 7),
            PartitionRule::Month | PartitionRule::None => return None,
        }
    } else if let Some((year, month)) = partition.as_month() {
        if rule != PartitionRule::Month {
            return None;
        }
        let (next_year, next_month) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        (
            CivilDate::new(year, month, 1)?.day_number(),
            CivilDate::new(next_year, next_month, 1)?.day_number(),
        )
    } else {
        return None;
    };
    let window = TimeWindow::new(day_start(first), day_start(end)).ok()?;
    // The id must be the one the rule gives its own first instant.
    (route(table_id, granularity, window.from()).ok()? == partition).then_some(window)
}

/// The UTC day number of an instant in the valid range.
pub fn day_of(t: Timestamp) -> Option<u32> {
    crucible_schema::day_of(t.micros())
}

/// The partition of a table that holds the rows of a UTC day.
pub fn partition_of_day(
    table_id: TableId,
    granularity: Granularity,
    day: u32,
) -> Option<PartitionId> {
    route(table_id, granularity, day_start(i64::from(day))).ok()
}

/// The partitions of a table that hold rows of these UTC days.
pub fn partitions_of_days(
    table_id: TableId,
    granularity: Granularity,
    days: impl IntoIterator<Item = u32>,
) -> BTreeSet<PartitionId> {
    days.into_iter()
        .filter_map(|day| partition_of_day(table_id, granularity, day))
        .filter(|partition| !partition.is_unpartitioned())
        .collect()
}

/// The partitions of a table that hold rows with a time in `[from, to)`, oldest first, at most `max` of them.
/// The flag says whether the list was cut.
pub fn partitions_between(
    table_id: TableId,
    granularity: Granularity,
    from: Timestamp,
    to: Timestamp,
    max: usize,
) -> (Vec<PartitionId>, bool) {
    let mut partitions = Vec::new();
    let (Some(first), Some(last)) = (day_of(from), day_of(to.saturating_add_micros(-1))) else {
        return (partitions, false);
    };
    if from >= to {
        return (partitions, false);
    }
    for day in first..=last {
        let Some(partition) = partition_of_day(table_id, granularity, day) else {
            continue;
        };
        if partition.is_unpartitioned() || partitions.last() == Some(&partition) {
            continue;
        }
        if partitions.len() == max {
            return (partitions, true);
        }
        partitions.push(partition);
    }
    (partitions, false)
}

/// The UTC days a span of time `[first, last]` touches.
pub fn days_spanned(first: Timestamp, last: Timestamp) -> std::ops::RangeInclusive<u32> {
    match (day_of(first), day_of(last)) {
        (Some(a), Some(b)) if a <= b => a..=b,
        (Some(a), _) => a..=a,
        // An empty range.
        _ => 1..=0,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const DAY: i64 = 86_400_000_000;

    fn table() -> impl Strategy<Value = TableId> {
        prop::sample::select(vec![
            TableId::SpanCore,
            TableId::SpanPayload,
            TableId::McpOps,
            TableId::FactScores,
            TableId::TraceHeads,
            TableId::TraceStats,
            TableId::SessionRollup,
            TableId::SpanMetrics,
        ])
    }

    fn granularity() -> impl Strategy<Value = Granularity> {
        prop::sample::select(vec![Granularity::Day, Granularity::Week])
    }

    #[test]
    fn an_unpartitioned_table_and_a_foreign_id_have_no_window() {
        let day = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
        let month = PartitionId::month(2026, 11).unwrap();
        let none = PartitionId::UNPARTITIONED;
        assert!(partition_window(TableId::McpDeploys, Granularity::Day, none).is_none());
        assert!(partition_window(TableId::SpanCore, Granularity::Day, month).is_none());
        assert!(partition_window(TableId::FactScores, Granularity::Day, day).is_none());
        // A Tuesday is not the id of a weekly partition.
        assert!(partition_window(TableId::SpanCore, Granularity::Week, day).is_none());
        let window = partition_window(TableId::FactScores, Granularity::Day, month).unwrap();
        assert_eq!(window.from().to_string(), "2026-11-01T00:00:00.000000Z");
        assert_eq!(window.to().to_string(), "2026-12-01T00:00:00.000000Z");
        assert_eq!(days_spanned(window.from(), window.to()).count(), 31);
        assert_eq!(days_spanned(window.to(), window.from()).count(), 1);
    }

    proptest! {
        /// A partition's window holds exactly the instants that route to it.
        #[test]
        fn a_window_holds_what_routes_to_its_partition(
            table in table(), granularity in granularity(),
            t in 0i64..Timestamp::MAX_EXCLUSIVE.micros(),
        ) {
            let t = Timestamp::from_micros(t);
            let partition = route(table, granularity, t).unwrap();
            let window = partition_window(table, granularity, partition).unwrap();
            prop_assert!(window.contains(t));
            prop_assert_eq!(route(table, granularity, window.from()).unwrap(), partition);
            let last = window.to().saturating_add_micros(-1);
            prop_assert_eq!(route(table, granularity, last).unwrap(), partition);
            if window.to() < Timestamp::MAX_EXCLUSIVE {
                prop_assert_ne!(route(table, granularity, window.to()).unwrap(), partition);
            }
        }

        /// The partitions of a span of time are those its instants route to, in order and without repeats.
        #[test]
        fn the_partitions_of_a_span_cover_it(
            table in table(), granularity in granularity(),
            from in 0i64..40_000 * DAY, len in 1i64..500 * DAY, at in 0.0f64..1.0,
        ) {
            let (from, to) = (Timestamp::from_micros(from), Timestamp::from_micros(from + len));
            let (partitions, cut) = partitions_between(table, granularity, from, to, usize::MAX);
            prop_assert!(!cut);
            prop_assert!(partitions.windows(2).all(|pair| pair[0] < pair[1]));
            let inside = Timestamp::from_micros(from.micros() + (len as f64 * at) as i64);
            prop_assert!(partitions.contains(&route(table, granularity, inside).unwrap()));
            prop_assert_eq!(partitions.first(), route(table, granularity, from).ok().as_ref());
            let last = to.saturating_add_micros(-1);
            prop_assert_eq!(partitions.last(), route(table, granularity, last).ok().as_ref());
            // A cut list is a prefix.
            let (some, cut) = partitions_between(table, granularity, from, to, 2);
            prop_assert_eq!(cut, partitions.len() > 2);
            prop_assert_eq!(&some[..], &partitions[..partitions.len().min(2)]);
        }
    }
}
