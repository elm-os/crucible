//! Partition routing (contract section 5.2) and the retention boundary arithmetic (sections 13.1 and 13.2).
//!
//! The kernel is integer-only `u32` arithmetic over day numbers in the valid time range
//! (`0 <= t < 2100-01-01`, days `0..DAY_COUNT`). Kani proves that it does not overflow and is monotone in time
//! (contract section 4.9); property tests check it against `crucible_records::CivilDate`.
//!
//! Routing semantics are frozen: changing a rule requires a new `wal_epoch`.

use crucible_records::{CivilDate, PartitionId, TableId, Timestamp};

use crate::ddl::Granularity;
use crate::table::{PartitionExpr, RetentionBinding};
use crate::tables::table;

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The number of UTC days in the valid time range: 1970-01-01 up to and excluding 2100-01-01.
pub const DAY_COUNT: u32 = 47_482;

/// A time outside the valid range `0 <= t < 2100-01-01`, or a partition id that no valid time routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("time or partition outside the valid range")]
pub struct OutOfRange;

/// How a table's rows map to partitions once the granularity option is applied.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PartitionRule {
    /// Unpartitioned: the id is `all`.
    None,
    /// `toDate`: the id is `YYYYMMDD`.
    Day,
    /// `toMonday`: the id is `YYYYMMDD` of the Monday on or before the day (`19700101` for the first four days
    /// of 1970).
    Week,
    /// `toYYYYMM`: the id is `YYYYMM`.
    Month,
}

impl PartitionRule {
    /// The rule for a partition expression at a granularity.
    pub const fn of(expr: PartitionExpr, granularity: Granularity) -> Self {
        match (expr, granularity) {
            (PartitionExpr::None, _) => Self::None,
            (PartitionExpr::Month(_), _) => Self::Month,
            (PartitionExpr::Day(_) | PartitionExpr::DateColumn(_), Granularity::Day) => Self::Day,
            (PartitionExpr::Day(_) | PartitionExpr::DateColumn(_), Granularity::Week) => Self::Week,
        }
    }

    /// The rule of a table.
    pub fn of_table(id: TableId, granularity: Granularity) -> Self {
        Self::of(table(id).partition, granularity)
    }
}

/// The UTC day number of an instant in the valid range.
pub const fn day_of(micros: i64) -> Option<u32> {
    if micros < 0 || micros >= Timestamp::MAX_EXCLUSIVE.micros() {
        None
    } else {
        // The quotient is below DAY_COUNT, so the cast cannot truncate.
        Some((micros / MICROS_PER_DAY) as u32)
    }
}

/// Year, month and day of a day number below [`DAY_COUNT`].
const fn civil(day: u32) -> (u32, u32, u32) {
    let z = day + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// The day number of a date with `1970 <= y <= 2100`, `1 <= m <= 12`, `1 <= d <= 31`.
const fn days_from_civil(y: u32, m: u32, d: u32) -> u32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y % 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The first and one-past-last day of the partition that holds `day` (`day < DAY_COUNT`). An unpartitioned
/// table spans the whole range.
///
/// 1970-01-01 was a Thursday and a `Date` cannot hold the Monday before it, so ClickHouse's `toMonday` saturates:
/// the first four days form one short partition whose id is `19700101`, and the first Monday is day 4. The
/// conformance outputs pin this on every supported version.
const fn partition_days(rule: PartitionRule, day: u32) -> (u32, u32) {
    match rule {
        PartitionRule::None => (0, DAY_COUNT),
        PartitionRule::Day => (day, day + 1),
        PartitionRule::Week => {
            if day < 4 {
                (0, 4)
            } else {
                let monday = day - (day + 3) % 7;
                (monday, monday + 7)
            }
        }
        PartitionRule::Month => {
            let (y, m, _) = civil(day);
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            (days_from_civil(y, m, 1), days_from_civil(ny, nm, 1))
        }
    }
}

/// The partition id as a number: `YYYYMMDD`, `YYYYMM`, or 0 for an unpartitioned table.
const fn partition_key(rule: PartitionRule, day: u32) -> u32 {
    match rule {
        PartitionRule::None => 0,
        PartitionRule::Day | PartitionRule::Week => {
            let (first, _) = partition_days(rule, day);
            let (y, m, d) = civil(first);
            y * 10_000 + m * 100 + d
        }
        PartitionRule::Month => {
            let (y, m, _) = civil(day);
            y * 100 + m
        }
    }
}

fn partition_id(rule: PartitionRule, day: u32) -> Result<PartitionId, OutOfRange> {
    let key = partition_key(rule, day);
    let id = match rule {
        PartitionRule::None => Some(PartitionId::UNPARTITIONED),
        PartitionRule::Day | PartitionRule::Week => {
            // The casts cannot truncate: the year is below 2100, the month and day below 100.
            CivilDate::new(
                (key / 10_000) as i32,
                (key / 100 % 100) as u8,
                (key % 100) as u8,
            )
            .map(PartitionId::date)
        }
        PartitionRule::Month => PartitionId::month((key / 100) as i32, (key % 100) as u8),
    };
    // The kernel only yields real dates (checked for every day of the range by a test).
    id.ok_or(OutOfRange)
}

/// Routes a row to its partition by the row's time column, read verbatim (UTC): `start_time`, `target_time`,
/// `minute`, or the first instant of `day` or `row_day`. Unpartitioned tables route every time to `all`.
pub fn route(
    table_id: TableId,
    granularity: Granularity,
    row_time: Timestamp,
) -> Result<PartitionId, OutOfRange> {
    let rule = PartitionRule::of_table(table_id, granularity);
    let day = day_of(row_time.micros()).ok_or(OutOfRange)?;
    partition_id(rule, day)
}

/// The first day of a retention window: rows whose time is before the start of this UTC day are expired.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CutoffDay(u32);

impl CutoffDay {
    /// The UTC day number.
    pub const fn day_number(self) -> u32 {
        self.0
    }

    /// The first instant that is still retained.
    pub const fn start(self) -> Timestamp {
        Timestamp::from_micros(self.0 as i64 * MICROS_PER_DAY)
    }
}

const fn cutoff_day(now_day: u32, retention_days: u32) -> u32 {
    now_day.saturating_sub(retention_days)
}

/// The retention cut-off at `now` for a retention of `retention_days`: the start of today's UTC day minus that
/// many days, never before the epoch (contract section 13.2: cut-offs are aligned to UTC days).
pub const fn retention_cutoff(
    now: Timestamp,
    retention_days: u32,
) -> Result<CutoffDay, OutOfRange> {
    match day_of(now.micros()) {
        Some(day) => Ok(CutoffDay(cutoff_day(day, retention_days))),
        None => Err(OutOfRange),
    }
}

/// Clamps the start of a read window to the cut-off, so expired rows are hidden at once (section 13.1).
pub const fn clamp_from(from: Timestamp, cutoff: CutoffDay) -> Timestamp {
    if from.micros() < cutoff.start().micros() {
        cutoff.start()
    } else {
        from
    }
}

/// What the retention executor does with a partition for one project (contract section 13.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PartitionFate {
    /// No row of the partition is expired.
    Live,
    /// The cut-off falls inside the partition: delete with the predicate `<time column> < <cut-off day>`.
    Straddles,
    /// The partition lies wholly before the cut-off: drop it, or delete the project's rows in it.
    Expired,
}

const fn fate(first: u32, end: u32, cutoff: u32) -> PartitionFate {
    if end <= cutoff {
        PartitionFate::Expired
    } else if first < cutoff {
        PartitionFate::Straddles
    } else {
        PartitionFate::Live
    }
}

/// The fate of one partition of a table at a cut-off, or `None` for the `all` partition of an unpartitioned
/// table (those tables are purged by key, section 13.2 step 4).
pub fn partition_fate(
    table_id: TableId,
    granularity: Granularity,
    partition: PartitionId,
    cutoff: CutoffDay,
) -> Result<Option<PartitionFate>, OutOfRange> {
    let rule = PartitionRule::of_table(table_id, granularity);
    let day = if let Some(date) = partition.as_date() {
        u32::try_from(date.day_number()).map_err(|_| OutOfRange)?
    } else if let Some((year, month)) = partition.as_month() {
        if !(1970..2100).contains(&year) {
            return Err(OutOfRange);
        }
        days_from_civil(year as u32, u32::from(month), 1)
    } else {
        return if rule == PartitionRule::None {
            Ok(None)
        } else {
            Err(OutOfRange)
        };
    };
    // The id must be one this table's rule produces.
    if day >= DAY_COUNT || partition_id(rule, day)? != partition {
        return Err(OutOfRange);
    }
    let (first, end) = partition_days(rule, day);
    Ok(Some(fate(first, end, cutoff.0)))
}

/// A project's retention settings (contract section 13.1). `None` keeps data forever.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct RetentionSettings {
    /// `retention_days`.
    pub retention_days: Option<u32>,
    /// `payload_retention_days`; never longer than `retention_days`.
    pub payload_retention_days: Option<u32>,
    /// `mcp_content_retention_days`; never longer than `retention_days`.
    pub mcp_content_retention_days: Option<u32>,
}

impl RetentionSettings {
    /// The retention in days that applies to a binding. The payload and MCP-content settings are capped by
    /// `retention_days`.
    pub fn days(&self, binding: RetentionBinding) -> Option<u32> {
        let capped = |specific: Option<u32>| match (specific, self.retention_days) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match binding {
            RetentionBinding::RetentionDays => self.retention_days,
            RetentionBinding::PayloadRetentionDays => capped(self.payload_retention_days),
            RetentionBinding::McpContentRetentionDays => capped(self.mcp_content_retention_days),
            RetentionBinding::OneDay => Some(1),
            RetentionBinding::None => None,
        }
    }

    /// The cut-off for a table at `now`, or `None` when its rows are kept forever.
    pub fn cutoff(
        &self,
        table_id: TableId,
        now: Timestamp,
    ) -> Result<Option<CutoffDay>, OutOfRange> {
        match self.days(table(table_id).retention.binding) {
            Some(days) => retention_cutoff(now, days).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn any_rule() -> PartitionRule {
        match kani::any::<u8>() % 4 {
            0 => PartitionRule::None,
            1 => PartitionRule::Day,
            2 => PartitionRule::Week,
            _ => PartitionRule::Month,
        }
    }

    fn any_day() -> u32 {
        let day: u32 = kani::any();
        kani::assume(day < DAY_COUNT);
        day
    }

    /// `day_of` is total, bounded and monotone over the valid range, and rejects everything outside it.
    #[kani::proof]
    fn day_of_is_bounded_and_monotone() {
        let (a, b): (i64, i64) = (kani::any(), kani::any());
        let valid = |t: i64| t >= 0 && t < Timestamp::MAX_EXCLUSIVE.micros();
        assert!(day_of(a).is_some() == valid(a));
        if let (Some(da), Some(db)) = (day_of(a), day_of(b)) {
            assert!(da < DAY_COUNT);
            if a <= b {
                assert!(da <= db);
            }
        }
    }

    /// The civil conversion never overflows, yields a real calendar position, and round-trips.
    #[kani::proof]
    fn civil_is_in_range_and_round_trips() {
        let day = any_day();
        let (y, m, d) = civil(day);
        assert!(1970 <= y && y <= 2099);
        assert!(1 <= m && m <= 12);
        assert!(1 <= d && d <= 31);
        assert!(days_from_civil(y, m, d) == day);
    }

    /// A partition contains the day routed to it, and its bounds stay inside `u32`.
    #[kani::proof]
    fn partition_span_contains_its_day() {
        let day = any_day();
        let (first, end) = partition_days(any_rule(), day);
        assert!(first <= day && day < end);
        assert!(end <= DAY_COUNT + 6);
    }

    /// Routing is monotone in time for every rule: a later day never routes to an earlier partition.
    #[kani::proof]
    fn partition_key_is_monotone() {
        let (a, b) = (any_day(), any_day());
        kani::assume(a <= b);
        let rule = any_rule();
        assert!(partition_key(rule, a) <= partition_key(rule, b));
    }

    /// The retention cut-off never overflows, is monotone in `now` and antitone in the retention length.
    #[kani::proof]
    fn retention_cutoff_is_monotone() {
        let (a, b) = (any_day(), any_day());
        let (short, long): (u32, u32) = (kani::any(), kani::any());
        kani::assume(a <= b && short <= long);
        assert!(cutoff_day(a, short) <= cutoff_day(b, short));
        assert!(cutoff_day(a, long) <= cutoff_day(a, short));
        assert!(cutoff_day(a, short) <= a);
        let cutoff = CutoffDay(cutoff_day(a, short));
        assert!(cutoff.start().micros() >= 0);
        assert!(cutoff.start().micros() < Timestamp::MAX_EXCLUSIVE.micros());
    }

    /// As the cut-off advances a partition only moves from live to straddling to expired.
    #[kani::proof]
    fn partition_fate_is_monotone_in_the_cutoff() {
        let (first, end, c1, c2): (u32, u32, u32, u32) =
            (kani::any(), kani::any(), kani::any(), kani::any());
        kani::assume(first < end && c1 <= c2);
        assert!(fate(first, end, c1) <= fate(first, end, c2));
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const RULES: [PartitionRule; 4] = [
        PartitionRule::None,
        PartitionRule::Day,
        PartitionRule::Week,
        PartitionRule::Month,
    ];

    fn ts(day: u32, micros_in_day: i64) -> Timestamp {
        Timestamp::from_micros(i64::from(day) * MICROS_PER_DAY + micros_in_day)
    }

    /// The reference: the same rules written with `CivilDate`.
    fn reference(rule: PartitionRule, t: Timestamp) -> PartitionId {
        let date = t.date();
        match rule {
            PartitionRule::None => PartitionId::UNPARTITIONED,
            PartitionRule::Day => PartitionId::date(date),
            PartitionRule::Week if date.day_number() < 4 => {
                PartitionId::date(CivilDate::from_day_number(0))
            }
            PartitionRule::Week => PartitionId::date(date.monday_on_or_before()),
            PartitionRule::Month => PartitionId::month(date.year(), date.month()).unwrap(),
        }
    }

    #[test]
    fn every_day_of_the_range_matches_civil_date() {
        assert_eq!(
            i64::from(DAY_COUNT) * MICROS_PER_DAY,
            Timestamp::MAX_EXCLUSIVE.micros()
        );
        let mut previous = [0u32; 4];
        for day in 0..DAY_COUNT {
            let date = CivilDate::from_day_number(i64::from(day));
            let (y, m, d) = civil(day);
            assert_eq!(
                (y as i32, m as u8, d as u8),
                (date.year(), date.month(), date.day())
            );
            assert_eq!(days_from_civil(y, m, d), day);
            for (i, rule) in RULES.into_iter().enumerate() {
                let id = partition_id(rule, day).unwrap();
                assert_eq!(id, reference(rule, ts(day, 0)), "{rule:?} {date}");
                let key = partition_key(rule, day);
                assert!(previous[i] <= key);
                previous[i] = key;
                if rule != PartitionRule::None {
                    assert_eq!(id.to_string(), key.to_string());
                }
                let (first, end) = partition_days(rule, day);
                assert!(first <= day && day < end);
                assert_eq!(partition_key(rule, first), key);
                if end < DAY_COUNT {
                    assert_ne!(partition_key(rule, end), key);
                }
                if first > 0 {
                    assert_ne!(partition_key(rule, first - 1), key);
                }
            }
        }
    }

    #[test]
    fn known_routes() {
        // 2026-11-17T09:30:00Z, a Tuesday.
        let t = Timestamp::from_micros(1_794_907_800_000_000);
        let route_of = |table, granularity| route(table, granularity, t).map(|p| p.to_string());
        assert_eq!(
            route_of(TableId::SpanCore, Granularity::Day).as_deref(),
            Ok("20261117")
        );
        assert_eq!(
            route_of(TableId::SpanCore, Granularity::Week).as_deref(),
            Ok("20261116")
        );
        assert_eq!(
            route_of(TableId::TraceStats, Granularity::Week).as_deref(),
            Ok("20261116")
        );
        assert_eq!(
            route_of(TableId::FactScores, Granularity::Week).as_deref(),
            Ok("202611")
        );
        assert_eq!(
            route_of(TableId::SpanMetrics, Granularity::Day).as_deref(),
            Ok("202611")
        );
        assert_eq!(
            route_of(TableId::McpDeploys, Granularity::Day).as_deref(),
            Ok("all")
        );
        for bad in [-1, Timestamp::MAX_EXCLUSIVE.micros()] {
            let t = Timestamp::from_micros(bad);
            assert_eq!(
                route(TableId::SpanCore, Granularity::Day, t),
                Err(OutOfRange)
            );
            assert_eq!(
                route(TableId::McpDeploys, Granularity::Day, t),
                Err(OutOfRange)
            );
        }
    }

    #[test]
    fn every_table_has_the_rule_of_the_contract() {
        use PartitionRule::{Day, Month, None, Week};
        let expected = [
            Day, Day, Day, Day, None, Month, Month, Month, Month, Day, Day, Month, Month, Month,
            Month, None, None, None,
        ];
        for (id, rule) in TableId::ALL.into_iter().zip(expected) {
            assert_eq!(
                PartitionRule::of_table(id, Granularity::Day),
                rule,
                "{id:?}"
            );
            let weekly = if rule == Day { Week } else { rule };
            assert_eq!(PartitionRule::of_table(id, Granularity::Week), weekly);
        }
    }

    #[test]
    fn retention_settings_cap_the_specific_bindings() {
        let s = RetentionSettings {
            retention_days: Some(30),
            payload_retention_days: Some(90),
            mcp_content_retention_days: Some(7),
        };
        assert_eq!(s.days(RetentionBinding::RetentionDays), Some(30));
        assert_eq!(s.days(RetentionBinding::PayloadRetentionDays), Some(30));
        assert_eq!(s.days(RetentionBinding::McpContentRetentionDays), Some(7));
        assert_eq!(s.days(RetentionBinding::OneDay), Some(1));
        assert_eq!(s.days(RetentionBinding::None), Option::None);
        let only_total = RetentionSettings {
            retention_days: Some(30),
            ..RetentionSettings::default()
        };
        assert_eq!(
            only_total.days(RetentionBinding::PayloadRetentionDays),
            Some(30)
        );
        let forever = RetentionSettings::default();
        assert_eq!(forever.days(RetentionBinding::RetentionDays), Option::None);
        let now = ts(20_774, 0);
        assert_eq!(forever.cutoff(TableId::SpanCore, now), Ok(Option::None));
        assert_eq!(
            s.cutoff(TableId::SpanPayload, now)
                .unwrap()
                .map(CutoffDay::day_number),
            Some(20_744)
        );
        assert_eq!(
            s.cutoff(TableId::McpContent, now)
                .unwrap()
                .map(CutoffDay::day_number),
            Some(20_767)
        );
    }

    #[test]
    fn fates_at_a_cutoff() {
        // Cut-off 2026-11-17 (day 20774, a Tuesday).
        let cutoff = retention_cutoff(ts(20_804, 5), 30).unwrap();
        assert_eq!(cutoff.day_number(), 20_774);
        assert_eq!(cutoff.start().to_string(), "2026-11-17T00:00:00.000000Z");
        let fate_of = |table, granularity, id: &str| {
            partition_fate(table, granularity, PartitionId::parse(id).unwrap(), cutoff)
        };
        use PartitionFate::{Expired, Live, Straddles};
        let day = Granularity::Day;
        let week = Granularity::Week;
        assert_eq!(
            fate_of(TableId::SpanCore, day, "20261116"),
            Ok(Some(Expired))
        );
        assert_eq!(fate_of(TableId::SpanCore, day, "20261117"), Ok(Some(Live)));
        assert_eq!(
            fate_of(TableId::SpanCore, week, "20261109"),
            Ok(Some(Expired))
        );
        assert_eq!(
            fate_of(TableId::SpanCore, week, "20261116"),
            Ok(Some(Straddles))
        );
        assert_eq!(fate_of(TableId::SpanCore, week, "20261123"), Ok(Some(Live)));
        assert_eq!(fate_of(TableId::FactTags, day, "202610"), Ok(Some(Expired)));
        assert_eq!(
            fate_of(TableId::FactTags, day, "202611"),
            Ok(Some(Straddles))
        );
        assert_eq!(fate_of(TableId::FactTags, day, "202612"), Ok(Some(Live)));
        assert_eq!(fate_of(TableId::McpDeploys, day, "all"), Ok(Option::None));
        // Ids the table's rule cannot produce.
        assert_eq!(
            fate_of(TableId::SpanCore, week, "20261117"),
            Err(OutOfRange)
        );
        assert_eq!(fate_of(TableId::SpanCore, day, "202611"), Err(OutOfRange));
        assert_eq!(fate_of(TableId::SpanCore, day, "all"), Err(OutOfRange));
        assert_eq!(fate_of(TableId::FactTags, day, "210001"), Err(OutOfRange));
        assert_eq!(fate_of(TableId::SpanCore, day, "21000101"), Err(OutOfRange));
    }

    proptest! {
        #[test]
        fn routes_match_civil_date(us in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), table in 0usize..18, week in any::<bool>()) {
            let id = TableId::ALL[table];
            let granularity = if week { Granularity::Week } else { Granularity::Day };
            let t = Timestamp::from_micros(us);
            let rule = PartitionRule::of_table(id, granularity);
            prop_assert_eq!(route(id, granularity, t), Ok(reference(rule, t)));
        }

        #[test]
        fn routing_is_monotone_in_time(a in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), b in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), table in 0usize..18, week in any::<bool>()) {
            let id = TableId::ALL[table];
            let granularity = if week { Granularity::Week } else { Granularity::Day };
            let (lo, hi) = (a.min(b), a.max(b));
            let lo = route(id, granularity, Timestamp::from_micros(lo)).unwrap();
            let hi = route(id, granularity, Timestamp::from_micros(hi)).unwrap();
            prop_assert!(lo <= hi);
        }

        #[test]
        fn out_of_range_times_are_refused(us in prop_oneof![i64::MIN..0, Timestamp::MAX_EXCLUSIVE.micros()..i64::MAX], table in 0usize..18) {
            let t = Timestamp::from_micros(us);
            prop_assert_eq!(route(TableId::ALL[table], Granularity::Day, t), Err(OutOfRange));
            prop_assert_eq!(retention_cutoff(t, 1), Err(OutOfRange));
        }

        #[test]
        fn cutoff_matches_day_arithmetic(us in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), days in 0u32..60_000) {
            let now = Timestamp::from_micros(us);
            let cutoff = retention_cutoff(now, days).unwrap();
            let expected = (now.day_number() - i64::from(days)).max(0);
            prop_assert_eq!(i64::from(cutoff.day_number()), expected);
            prop_assert_eq!(cutoff.start(), CivilDate::from_day_number(expected).start());
            prop_assert!(cutoff.start() <= now);
        }

        #[test]
        fn clamp_hides_exactly_the_expired_prefix(from in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), now in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), days in 0u32..4_000) {
            let cutoff = retention_cutoff(Timestamp::from_micros(now), days).unwrap();
            let from = Timestamp::from_micros(from);
            let clamped = clamp_from(from, cutoff);
            prop_assert_eq!(clamped, from.max(cutoff.start()));
        }

        #[test]
        fn fate_agrees_with_the_rows_of_the_partition(us in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), now in 0i64..Timestamp::MAX_EXCLUSIVE.micros(), days in 0u32..4_000, table in 0usize..18, week in any::<bool>()) {
            let id = TableId::ALL[table];
            let granularity = if week { Granularity::Week } else { Granularity::Day };
            let row = Timestamp::from_micros(us);
            let cutoff = retention_cutoff(Timestamp::from_micros(now), days).unwrap();
            let partition = route(id, granularity, row).unwrap();
            let expired_row = row < cutoff.start();
            match partition_fate(id, granularity, partition, cutoff).unwrap() {
                Some(PartitionFate::Expired) => prop_assert!(expired_row),
                Some(PartitionFate::Live) => prop_assert!(!expired_row),
                Some(PartitionFate::Straddles) => {}
                Option::None => prop_assert!(partition.is_unpartitioned()),
            }
        }
    }
}
