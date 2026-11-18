//! The per-key limit on tool calls: GCRA on the monotonic clock of `Env`.
//!
//! Each key has a theoretical arrival time (TAT). A call at `now` is allowed when the TAT is at most the burst
//! tolerance ahead of `now`, and it moves the TAT one emission interval later; a refused call leaves it alone
//! and is told how long to wait. A key that has been quiet for `burst` intervals may make `burst` calls at once.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crucible_env::Instant;
use crucible_records::ApiKeyId;

use crate::McpConfig;

const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The limiter of one node.
#[derive(Debug)]
pub(crate) struct Limiter {
    /// The time one call uses up.
    interval: Duration,
    /// How far ahead of now a key's TAT may run: `burst - 1` intervals.
    tolerance: Duration,
    tats: Mutex<BTreeMap<ApiKeyId, Instant>>,
}

impl Limiter {
    /// The limiter of `config`, or `None` when the limit is off.
    pub(crate) fn new(config: McpConfig) -> Option<Self> {
        if config.calls_per_second == 0 {
            return None;
        }
        let interval = Duration::from_nanos(NANOS_PER_SECOND / u64::from(config.calls_per_second));
        let tolerance = interval.saturating_mul(config.burst.saturating_sub(1));
        Some(Self {
            interval,
            tolerance,
            tats: Mutex::new(BTreeMap::new()),
        })
    }

    /// Takes one call for `key` at `now`, or says how long to wait before the next call is allowed.
    pub(crate) fn check(&self, key: ApiKeyId, now: Instant) -> Result<(), Duration> {
        let mut tats = self.tats.lock().unwrap_or_else(PoisonError::into_inner);
        let tat = tats.get(&key).copied().unwrap_or(now).max(now);
        let ahead = tat.saturating_duration_since(now);
        if ahead > self.tolerance {
            return Err(ahead.saturating_sub(self.tolerance));
        }
        tats.insert(key, tat.saturating_add(self.interval));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn limiter(calls_per_second: u32, burst: u32) -> Limiter {
        Limiter::new(McpConfig {
            calls_per_second,
            burst,
        })
        .unwrap()
    }

    fn at(millis: u64) -> Instant {
        Instant::from_nanos(millis * 1_000_000)
    }

    const KEY: ApiKeyId = ApiKeyId::from_bytes([1; 16]);

    #[test]
    fn a_quiet_key_gets_its_burst_then_one_call_per_interval() {
        let limiter = limiter(10, 3);
        for _ in 0..3 {
            assert_eq!(limiter.check(KEY, at(0)), Ok(()));
        }
        assert_eq!(limiter.check(KEY, at(0)), Err(Duration::from_millis(100)));
        assert_eq!(limiter.check(KEY, at(99)), Err(Duration::from_millis(1)));
        assert_eq!(limiter.check(KEY, at(100)), Ok(()));
        assert!(limiter.check(KEY, at(100)).is_err());
        // Another key has its own budget.
        assert_eq!(
            limiter.check(ApiKeyId::from_bytes([2; 16]), at(100)),
            Ok(())
        );
        // A long quiet spell refills the burst, and no more.
        for _ in 0..3 {
            assert_eq!(limiter.check(KEY, at(10_000)), Ok(()));
        }
        assert!(limiter.check(KEY, at(10_000)).is_err());
    }

    #[test]
    fn zero_calls_per_second_turns_the_limit_off() {
        assert!(
            Limiter::new(McpConfig {
                calls_per_second: 0,
                burst: 1
            })
            .is_none()
        );
    }

    proptest! {
        /// Over any window, a key is allowed at most `burst` calls plus one per interval that passed, and a
        /// refused call is told to wait exactly until its call would be allowed.
        #[test]
        fn allowed_calls_never_exceed_the_burst_plus_the_rate(
            calls_per_second in 1u32..1_000,
            burst in 1u32..50,
            gaps in prop::collection::vec(0u64..20_000_000, 1..300),
        ) {
            let limiter = limiter(calls_per_second, burst);
            let interval = NANOS_PER_SECOND / u64::from(calls_per_second);
            let (mut now, mut allowed) = (0u64, 0u64);
            for gap in gaps {
                now += gap;
                match limiter.check(KEY, Instant::from_nanos(now)) {
                    Ok(()) => allowed += 1,
                    Err(wait) => {
                        let later = Instant::from_nanos(now + u64::try_from(wait.as_nanos()).unwrap());
                        prop_assert!(limiter.check(KEY, later).is_ok());
                        allowed += 1;
                        now = later.nanos();
                    }
                }
                prop_assert!(allowed <= u64::from(burst) + now / interval + 1);
            }
        }
    }
}
