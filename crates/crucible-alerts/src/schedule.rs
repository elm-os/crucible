//! When a rule is evaluated and what each evaluation reads.
//!
//! A rule's ticks are `interval` apart and shifted by an offset inside the interval that is a function of the
//! rule's id. The ticks of the rules of a deployment are therefore spread over the minute instead of all
//! falling on its first second, where they would run together and compete with the reads of people. Tick
//! `seq` is due at `seq * interval + offset`, and reads the window that ends at the last whole minute at least
//! 30 s before it, which leaves the rollup time to receive that minute.

use std::time::Duration;

use crucible_records::{AlertRuleId, InvalidWindow, TimeWindow, Timestamp};

/// How far behind its tick an evaluation reads, at least (contract 12.6): the rollup's freshness.
pub const EVALUATION_LAG: Duration = Duration::from_secs(30);

const MICROS: i64 = 1_000_000;

/// The rule's offset inside its interval, in seconds: FNV-1a over the id's bytes, reduced to the interval. A
/// function of the id only, so every node and every restart computes the same schedule.
pub fn offset_secs(rule_id: AlertRuleId, interval_secs: u32) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in rule_id.to_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // The interval is at least one second; a zero would be a stored defect, and the offset is then zero.
    (hash % u64::from(interval_secs.max(1))) as u32
}

/// When tick `seq` of the rule is due.
pub fn tick_at(rule_id: AlertRuleId, interval_secs: u32, seq: u64) -> Timestamp {
    let interval = i64::from(interval_secs.max(1));
    let secs = i64::try_from(seq)
        .unwrap_or(i64::MAX)
        .saturating_mul(interval)
        .saturating_add(i64::from(offset_secs(rule_id, interval_secs)));
    Timestamp::from_micros(secs.saturating_mul(MICROS))
}

/// The latest tick that is due at `now`; `None` before the rule's first tick.
pub fn current_seq(rule_id: AlertRuleId, interval_secs: u32, now: Timestamp) -> Option<u64> {
    let since = now
        .micros()
        .div_euclid(MICROS)
        .checked_sub(i64::from(offset_secs(rule_id, interval_secs)))?;
    u64::try_from(since.div_euclid(i64::from(interval_secs.max(1)))).ok()
}

/// The window tick `seq` reads: `window_secs` long, ending at the last whole minute at least
/// [`EVALUATION_LAG`] before the tick.
pub fn window_of(
    rule_id: AlertRuleId,
    interval_secs: u32,
    seq: u64,
    window_secs: u32,
) -> Result<TimeWindow, InvalidWindow> {
    let lag = i64::try_from(EVALUATION_LAG.as_micros()).unwrap_or(i64::MAX);
    let end = tick_at(rule_id, interval_secs, seq)
        .saturating_add_micros(-lag)
        .floor_minute();
    let start = end.saturating_add_micros(-i64::from(window_secs).saturating_mul(MICROS));
    TimeWindow::new(start, end)
}

#[cfg(test)]
mod tests {
    use crucible_records::Uuid;
    use proptest::prelude::*;

    use super::*;

    fn rule(bytes: [u8; 16]) -> AlertRuleId {
        AlertRuleId::from_uuid(Uuid::from_bytes(bytes))
    }

    proptest! {
        /// The offset is a function of the id and the interval, and lies inside the interval.
        #[test]
        fn the_offset_is_stable_and_inside_the_interval(bytes in any::<[u8; 16]>(), interval in 60u32..=86_400) {
            let offset = offset_secs(rule(bytes), interval);
            prop_assert!(offset < interval);
            prop_assert_eq!(offset, offset_secs(rule(bytes), interval));
        }

        /// A tick is the latest one due from its own instant until the next tick's.
        #[test]
        fn the_current_tick_is_the_latest_one_due(
            bytes in any::<[u8; 16]>(),
            interval in 60u32..=86_400,
            seq in 0u64..40_000_000,
            into in 0u32..86_400,
        ) {
            let id = rule(bytes);
            let due = tick_at(id, interval, seq);
            let within = i64::from(into % interval) * MICROS;
            prop_assert_eq!(current_seq(id, interval, due.saturating_add_micros(within)), Some(seq));
            prop_assert_eq!(current_seq(id, interval, due.saturating_add_micros(-1)), seq.checked_sub(1));
            prop_assert_eq!(
                tick_at(id, interval, seq + 1).micros() - due.micros(),
                i64::from(interval) * MICROS
            );
        }

        /// The window is whole minutes, as long as asked, and ends between 30 s and 90 s before the tick.
        #[test]
        fn the_window_lags_the_tick(
            bytes in any::<[u8; 16]>(),
            interval in 60u32..=86_400,
            seq in 1_000u64..40_000_000,
            minutes in 1u32..=1_440,
        ) {
            let id = rule(bytes);
            let window = window_of(id, interval, seq, minutes * 60).unwrap();
            let due = tick_at(id, interval, seq);
            prop_assert_eq!(window.to(), window.to().floor_minute());
            prop_assert_eq!(window.len_micros(), i64::from(minutes) * 60 * MICROS);
            let lag = due.micros() - window.to().micros();
            prop_assert!((30 * MICROS..90 * MICROS).contains(&lag), "lag {}", lag);
        }
    }

    #[test]
    fn rules_do_not_all_tick_on_the_minute() {
        // 200 rules with a 60 s interval: their offsets cover most of the minute.
        let offsets: std::collections::BTreeSet<u32> = (0..200u8)
            .map(|n| offset_secs(rule([n; 16]), 60))
            .collect();
        assert!(offsets.len() > 40, "{} distinct offsets", offsets.len());
        assert!(offsets.iter().any(|o| *o >= 30) && offsets.iter().any(|o| *o < 30));
    }
}
