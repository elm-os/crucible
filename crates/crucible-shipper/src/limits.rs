//! Pressure responses (contract 5.1 and 5.6): the seal limits widen by AIMD, the connection gate halves on a
//! memory rejection, and retries back off with full jitter.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crucible_env::Rng;
use tokio::sync::Notify;

use crate::config::ShipperConfig;

/// The seal limits of contract 5.1. Under pressure (too many parts, or a slow insert) each limit doubles up to
/// its maximum, which halves the insert rate; every unpressured insert takes one tenth of the base value off
/// again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SealLimits {
    linger: Duration,
    rows: usize,
    base_linger: Duration,
    base_rows: usize,
    max_linger: Duration,
    max_rows: usize,
}

impl SealLimits {
    /// The unpressured limits of a configuration.
    pub fn new(config: &ShipperConfig) -> Self {
        Self {
            linger: config.linger,
            rows: config.seal_rows,
            base_linger: config.linger,
            base_rows: config.seal_rows,
            max_linger: config.max_linger.max(config.linger),
            max_rows: config.max_seal_rows.max(config.seal_rows),
        }
    }

    /// The current linger.
    pub const fn linger(&self) -> Duration {
        self.linger
    }

    /// The current span-row limit.
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// Pressure: both limits double, up to their maximum.
    pub fn widen(&mut self) {
        self.linger = self.linger.saturating_mul(2).min(self.max_linger);
        self.rows = self.rows.saturating_mul(2).min(self.max_rows);
    }

    /// An unpressured insert: both limits step back toward their base.
    pub fn relax(&mut self) {
        self.linger = self
            .linger
            .saturating_sub(self.base_linger / 10)
            .max(self.base_linger);
        self.rows = self
            .rows
            .saturating_sub(self.base_rows / 10)
            .max(self.base_rows);
    }
}

#[derive(Debug)]
struct GateState {
    limit: usize,
    in_use: usize,
}

/// The connection pool as a counter: at most `limit` inserts and probes run at once. A memory rejection halves
/// the limit; every confirmed insert gives one connection back, up to the configured number.
#[derive(Debug)]
pub(crate) struct Gate {
    state: Mutex<GateState>,
    max: usize,
    freed: Notify,
}

/// One connection of the [`Gate`], returned on drop.
#[derive(Debug)]
pub(crate) struct Permit<'a>(&'a Gate);

impl Gate {
    pub(crate) fn new(connections: usize) -> Self {
        let max = connections.max(1);
        Self {
            state: Mutex::new(GateState {
                limit: max,
                in_use: 0,
            }),
            max,
            freed: Notify::new(),
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut GateState) -> T) -> T {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) async fn acquire(&self) -> Permit<'_> {
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            // Registered before the check, so a release between the check and the wait is not missed.
            freed.as_mut().enable();
            let taken = self.with(|s| {
                let free = s.in_use < s.limit;
                if free {
                    s.in_use += 1;
                }
                free
            });
            if taken {
                return Permit(self);
            }
            freed.await;
        }
    }

    pub(crate) fn halve(&self) {
        self.with(|s| s.limit = (s.limit / 2).max(1));
    }

    pub(crate) fn grow(&self) {
        self.with(|s| s.limit = (s.limit + 1).min(self.max));
        self.freed.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn limit(&self) -> usize {
        self.with(|s| s.limit)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.with(|s| s.in_use = s.in_use.saturating_sub(1));
        self.0.freed.notify_waiters();
    }
}

/// Full-jitter backoff (contract 5.6): a uniform draw between `min` and `min * 2^attempt`, capped at `max`.
pub(crate) fn backoff(min: Duration, max: Duration, attempt: u32, rng: &dyn Rng) -> Duration {
    let floor = u64::try_from(min.as_micros()).unwrap_or(u64::MAX);
    let ceiling = u64::try_from(max.as_micros())
        .unwrap_or(u64::MAX)
        .max(floor);
    let cap = floor
        .saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX))
        .min(ceiling);
    let span = cap - floor;
    let jitter = if span == u64::MAX {
        rng.next_u64()
    } else {
        rng.next_u64() % (span + 1)
    };
    Duration::from_micros(floor + jitter)
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn limits_widen_to_the_contract_maximum_and_relax_to_the_base() {
        let mut limits = SealLimits::new(&ShipperConfig::default());
        assert_eq!(
            (limits.linger(), limits.rows()),
            (Duration::from_millis(500), 131_072)
        );
        limits.widen();
        assert_eq!(
            (limits.linger(), limits.rows()),
            (Duration::from_secs(1), 262_144)
        );
        limits.widen();
        limits.widen();
        assert_eq!(
            (limits.linger(), limits.rows()),
            (Duration::from_secs(2), 262_144)
        );
        limits.relax();
        assert_eq!(limits.linger(), Duration::from_millis(1950));
        for _ in 0..100 {
            limits.relax();
        }
        assert_eq!(
            (limits.linger(), limits.rows()),
            (Duration::from_millis(500), 131_072)
        );
    }

    #[test]
    fn the_gate_halves_on_memory_pressure_and_grows_back() {
        let gate = Gate::new(8);
        gate.halve();
        assert_eq!(gate.limit(), 4);
        gate.halve();
        gate.halve();
        gate.halve();
        assert_eq!(gate.limit(), 1);
        for _ in 0..20 {
            gate.grow();
        }
        assert_eq!(gate.limit(), 8);
    }

    proptest! {
        #[test]
        fn backoff_stays_between_its_bounds(seed in any::<u64>(), attempt in 0u32..80) {
            let rng = ChaChaRng::from_u64(seed);
            let (min, max) = (Duration::from_millis(100), Duration::from_secs(30));
            let delay = backoff(min, max, attempt, &rng);
            prop_assert!(delay >= min && delay <= max);
            let cap = min.saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX)).min(max);
            prop_assert!(delay <= cap);
        }
    }
}
