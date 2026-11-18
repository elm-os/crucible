//! When a job runs next: its cadence, and the retry backoff of claimed work. Pure arithmetic; the only
//! input besides the arguments is the random stream the jitter is drawn from.

use std::time::Duration;

use crucible_env::Rng;

/// The first retry delay of a failed claim.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(5);
/// The longest retry delay of a failed claim.
pub const BACKOFF_MAX: Duration = Duration::from_secs(3_600);

/// How often a job runs: a fixed period plus a random extra of up to `jitter`, so that the nodes of a
/// deployment do not all wake in the same instant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cadence {
    every: Duration,
    jitter: Duration,
}

impl Cadence {
    /// A run every `every`, with a jitter of up to a tenth of it.
    pub const fn every(every: Duration) -> Self {
        Self {
            every,
            jitter: Duration::from_nanos(nanos(every) / 10),
        }
    }

    /// The same period with another jitter bound.
    #[must_use]
    pub const fn with_jitter(self, jitter: Duration) -> Self {
        Self { jitter, ..self }
    }

    /// The period.
    pub const fn period(&self) -> Duration {
        self.every
    }

    /// The jitter bound.
    pub const fn jitter(&self) -> Duration {
        self.jitter
    }

    /// The pause before the next run: the period plus a uniform draw from `0..=jitter`.
    pub fn next_delay(&self, rng: &dyn Rng) -> Duration {
        self.every.saturating_add(uniform(rng, self.jitter))
    }
}

const fn nanos(d: Duration) -> u64 {
    let nanos = d.as_nanos();
    if nanos > u64::MAX as u128 {
        u64::MAX
    } else {
        nanos as u64
    }
}

/// A uniform draw from `0..=max`.
pub(crate) fn uniform(rng: &dyn Rng, max: Duration) -> Duration {
    match nanos(max).checked_add(1) {
        Some(bound) => Duration::from_nanos(rng.next_u64() % bound),
        None => Duration::from_nanos(rng.next_u64()),
    }
}

/// The retry delay after the `attempt`-th attempt failed, before jitter: 5 s after the first, doubling with
/// each further attempt, at most 1 h. Attempt 0 counts as the first.
pub const fn backoff_base(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1);
    // 5 s doubled ten times is past the hour already, so the shift never overflows.
    if doublings >= 10 {
        return BACKOFF_MAX;
    }
    let secs = BACKOFF_FIRST.as_secs() << doublings;
    if secs >= BACKOFF_MAX.as_secs() {
        BACKOFF_MAX
    } else {
        Duration::from_secs(secs)
    }
}

/// The retry delay after the `attempt`-th attempt failed: [`backoff_base`] less a uniform draw of up to a
/// tenth of it, so that rows that failed together do not come back together and the delay never passes 1 h.
pub fn backoff(attempt: u32, rng: &dyn Rng) -> Duration {
    let base = backoff_base(attempt);
    base.saturating_sub(uniform(rng, base / 10))
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn the_backoff_starts_at_five_seconds_and_doubles_to_an_hour() {
        let secs: Vec<u64> = (0..=12).map(|n| backoff_base(n).as_secs()).collect();
        assert_eq!(
            secs,
            [5, 5, 10, 20, 40, 80, 160, 320, 640, 1_280, 2_560, 3_600, 3_600]
        );
        assert_eq!(backoff_base(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn the_default_jitter_is_a_tenth_of_the_period() {
        let cadence = Cadence::every(Duration::from_secs(60));
        assert_eq!(cadence.period(), Duration::from_secs(60));
        assert_eq!(cadence.jitter(), Duration::from_secs(6));
        let none = cadence.with_jitter(Duration::ZERO);
        let rng = ChaChaRng::from_u64(1);
        assert_eq!(none.next_delay(&rng), Duration::from_secs(60));
    }

    proptest! {
        /// The base delay never shrinks as attempts grow, stays between 5 s and 1 h, and doubles until the
        /// hour caps it.
        #[test]
        fn the_backoff_base_is_monotone_bounded_and_doubling(attempt in any::<u32>()) {
            let base = backoff_base(attempt);
            prop_assert!(base >= BACKOFF_FIRST && base <= BACKOFF_MAX);
            let next = backoff_base(attempt.saturating_add(1));
            prop_assert!(next >= base);
            if (1..u32::MAX).contains(&attempt) {
                prop_assert_eq!(next, (base * 2).min(BACKOFF_MAX));
            }
        }

        /// The jittered delay is within a tenth under the base and never above it, for every attempt and
        /// every random stream.
        #[test]
        fn the_jittered_backoff_stays_within_a_tenth_of_the_base(attempt in any::<u32>(), seed in any::<u64>()) {
            let rng = ChaChaRng::from_u64(seed);
            let base = backoff_base(attempt);
            for _ in 0..8 {
                let delay = backoff(attempt, &rng);
                prop_assert!(delay <= base);
                prop_assert!(delay >= base - base / 10);
                prop_assert!(delay <= BACKOFF_MAX);
            }
        }

        /// A cadence never fires early and never later than its period plus its jitter.
        #[test]
        fn a_cadence_delay_is_within_its_period_and_jitter(
            every_ms in 0u64..86_400_000,
            jitter_ms in proptest::option::of(0u64..3_600_000),
            seed in any::<u64>(),
        ) {
            let every = Duration::from_millis(every_ms);
            let mut cadence = Cadence::every(every);
            if let Some(jitter) = jitter_ms {
                cadence = cadence.with_jitter(Duration::from_millis(jitter));
            }
            let rng = ChaChaRng::from_u64(seed);
            for _ in 0..8 {
                let delay = cadence.next_delay(&rng);
                prop_assert!(delay >= every);
                prop_assert!(delay <= every + cadence.jitter());
            }
        }

        /// The draw is uniform over its whole range, the ends included, also at the largest duration.
        #[test]
        fn a_uniform_draw_never_passes_its_bound(max_ns in any::<u64>(), seed in any::<u64>()) {
            let rng = ChaChaRng::from_u64(seed);
            let max = Duration::from_nanos(max_ns);
            prop_assert!(uniform(&rng, max) <= max);
            prop_assert_eq!(uniform(&rng, Duration::ZERO), Duration::ZERO);
        }
    }
}
