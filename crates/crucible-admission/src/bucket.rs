//! Per-project token buckets (contract 4.8). Time is the monotonic clock in nanoseconds, passed in by
//! the caller.

use crucible_records::{DetMap, ProjectId, det_map};

const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// The refill rate and burst of a token bucket. Tokens count whatever the caller meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketConfig {
    /// Tokens added per second.
    pub rate_per_s: u64,
    /// The most tokens the bucket holds, and so the largest single take that can succeed.
    pub burst: u64,
}

/// A take that the bucket cannot cover now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("token bucket exhausted")]
pub struct Throttled;

/// A token bucket holding up to `burst` tokens and refilling at `rate_per_s`. The level is kept in
/// nano-tokens (10^9 per token), so refilling is exact however often the bucket is read.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    config: BucketConfig,
    level: u128,
    last_ns: u64,
}

const fn capacity(config: BucketConfig) -> u128 {
    config.burst as u128 * NANOS_PER_SECOND
}

impl TokenBucket {
    /// A full bucket at monotonic time `now_ns`.
    pub const fn full(config: BucketConfig, now_ns: u64) -> Self {
        Self {
            config,
            level: capacity(config),
            last_ns: now_ns,
        }
    }

    /// Takes `tokens` at monotonic time `now_ns`, or takes nothing and refuses. A time earlier than
    /// the latest one seen adds no tokens.
    pub fn try_take(&mut self, tokens: u64, now_ns: u64) -> Result<(), Throttled> {
        self.refill(now_ns);
        let cost = u128::from(tokens) * NANOS_PER_SECOND;
        match self.level.checked_sub(cost) {
            Some(level) => {
                self.level = level;
                Ok(())
            }
            None => Err(Throttled),
        }
    }

    fn refill(&mut self, now_ns: u64) {
        let elapsed = now_ns.saturating_sub(self.last_ns);
        let added = u128::from(elapsed) * u128::from(self.config.rate_per_s);
        self.level = self.level.saturating_add(added).min(capacity(self.config));
        self.last_ns = self.last_ns.max(now_ns);
    }
}

/// One token bucket per project, each created full on the project's first request.
#[derive(Debug)]
pub struct ProjectBuckets {
    config: BucketConfig,
    buckets: DetMap<ProjectId, TokenBucket>,
}

impl ProjectBuckets {
    /// Buckets that all use `config`.
    pub fn new(config: BucketConfig) -> Self {
        Self {
            config,
            buckets: det_map(),
        }
    }

    /// Takes `tokens` from `project`'s bucket at monotonic time `now_ns`.
    pub fn try_take(
        &mut self,
        project: ProjectId,
        tokens: u64,
        now_ns: u64,
    ) -> Result<(), Throttled> {
        let config = self.config;
        self.buckets
            .entry(project)
            .or_insert_with(|| TokenBucket::full(config, now_ns))
            .try_take(tokens, now_ns)
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Any bucket within its burst.
    fn any_bucket() -> TokenBucket {
        let config = BucketConfig {
            rate_per_s: kani::any(),
            burst: kani::any(),
        };
        let level: u128 = kani::any();
        kani::assume(level <= capacity(config));
        TokenBucket {
            config,
            level,
            last_ns: kani::any(),
        }
    }

    /// Taking never overflows for any configuration, state, time or take, and the level stays
    /// within the burst.
    #[kani::proof]
    fn token_bucket_arithmetic_never_overflows() {
        let mut bucket = any_bucket();
        let _ = bucket.try_take(kani::any(), kani::any());
        assert!(bucket.level <= capacity(bucket.config));
    }

    /// A take at the instant of the latest refill removes exactly its tokens, or nothing when it is
    /// refused.
    #[kani::proof]
    fn a_take_removes_exactly_its_tokens() {
        let mut bucket = any_bucket();
        let before = bucket.level;
        let tokens: u64 = kani::any();
        let cost = u128::from(tokens) * NANOS_PER_SECOND;
        match bucket.try_take(tokens, bucket.last_ns) {
            Ok(()) => assert!(cost <= before && before - bucket.level == cost),
            Err(Throttled) => assert!(cost > before && bucket.level == before),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const SECOND: u64 = 1_000_000_000;

    #[test]
    fn refills_at_the_rate_up_to_the_burst() {
        let config = BucketConfig {
            rate_per_s: 10,
            burst: 20,
        };
        let mut bucket = TokenBucket::full(config, 0);
        assert_eq!(bucket.try_take(20, 0), Ok(()));
        assert_eq!(bucket.try_take(1, 0), Err(Throttled));
        // A tenth of a second adds one token.
        assert_eq!(bucket.try_take(1, SECOND / 10), Ok(()));
        assert_eq!(bucket.try_take(1, SECOND / 10), Err(Throttled));
        // Frequent small reads lose no fractional refill: ten reads 10 ms apart add one token.
        for i in 1..=10 {
            assert_eq!(bucket.try_take(0, SECOND / 10 + i * SECOND / 100), Ok(()));
        }
        assert_eq!(bucket.try_take(1, SECOND / 5), Ok(()));
        // A long pause refills only up to the burst.
        assert_eq!(bucket.try_take(20, 100 * SECOND), Ok(()));
        assert_eq!(bucket.try_take(1, 100 * SECOND), Err(Throttled));
        // Time going backwards adds nothing.
        assert_eq!(bucket.try_take(1, SECOND), Err(Throttled));
        // A take above the burst never succeeds.
        assert_eq!(bucket.try_take(21, 1_000 * SECOND), Err(Throttled));
    }

    #[test]
    fn projects_have_separate_buckets() {
        let mut buckets = ProjectBuckets::new(BucketConfig {
            rate_per_s: 1,
            burst: 5,
        });
        let a = ProjectId::from_bytes([1; 16]);
        let b = ProjectId::from_bytes([2; 16]);
        assert_eq!(buckets.try_take(a, 5, 0), Ok(()));
        assert_eq!(buckets.try_take(a, 1, 0), Err(Throttled));
        assert_eq!(buckets.try_take(b, 5, 0), Ok(()));
        assert_eq!(buckets.try_take(a, 1, SECOND), Ok(()));
    }

    /// The same limiter in its virtual-scheduling form: the theoretical arrival time, in units of
    /// nanoseconds times the rate, so one token costs exactly 10^9.
    struct Scheduled {
        tat: u128,
        rate: u128,
        tolerance: u128,
    }

    impl Scheduled {
        fn take(&mut self, now_ns: u64, tokens: u64) -> bool {
            let now = u128::from(now_ns) * self.rate;
            let cost = u128::from(tokens) * NANOS_PER_SECOND;
            let start = self.tat.max(now);
            if start + cost - now <= self.tolerance {
                self.tat = start + cost;
                true
            } else {
                false
            }
        }
    }

    proptest! {
        /// The bucket admits exactly what the virtual-scheduling formulation admits, and never more
        /// than the burst plus the rate times the elapsed time.
        #[test]
        fn matches_the_virtual_scheduling_form(
            rate in prop_oneof![0..4u64, 0..1_000_000u64],
            burst in prop_oneof![0..4u64, 0..1_000_000u64],
            start in 0..1_000_000_000_000u64,
            ops in proptest::collection::vec((0..3 * SECOND, 0..1_000_000u64), 0..64),
        ) {
            let config = BucketConfig { rate_per_s: rate, burst };
            let mut bucket = TokenBucket::full(config, start);
            let mut model = Scheduled {
                tat: u128::from(start) * u128::from(rate),
                rate: u128::from(rate),
                tolerance: capacity(config),
            };
            let mut now = start;
            let mut granted: u128 = 0;
            for (step, tokens) in ops {
                now += step;
                let tokens = tokens % (burst + 2);
                let ok = bucket.try_take(tokens, now).is_ok();
                prop_assert_eq!(ok, model.take(now, tokens));
                if ok {
                    granted += u128::from(tokens);
                }
                let allowance = capacity(config) + u128::from(now - start) * u128::from(rate);
                prop_assert!(granted * NANOS_PER_SECOND <= allowance);
            }
        }
    }
}
