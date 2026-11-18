//! The admission decision of contract 4.8 and its `Retry-After` rules.
//!
//! Server-side conditions (503) take precedence over client throttling (429): when the WAL cannot
//! take the request, waiting for client tokens would not help.

/// The shortest `Retry-After` of a 429, in seconds.
pub const THROTTLED_MIN_S: u32 = 1;
/// The longest `Retry-After` of a 429, in seconds.
pub const THROTTLED_MAX_S: u32 = 5;
/// The shortest `Retry-After` of a 503, in seconds.
pub const UNAVAILABLE_MIN_S: u32 = 5;
/// The longest `Retry-After` of a 503, in seconds.
pub const UNAVAILABLE_MAX_S: u32 = 30;

/// What admission knows about one request. The caller reads every signal before deciding; the
/// decision itself is a pure function of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Signals {
    /// The API key is not in the key snapshot and Postgres is unreachable (contract 11.8).
    pub key_unknown_control_plane_down: bool,
    /// The WAL is `failed`: a group sync failed and the rotation after it failed too (contract 4.5).
    pub wal_failed: bool,
    /// Bytes in WAL segments.
    pub wal_bytes: u64,
    /// `wal.max_bytes`. Above 90% of it ingest answers 503 (contract 4.7).
    pub wal_max_bytes: u64,
    /// Free bytes on the WAL's disk.
    pub disk_free_bytes: u64,
    /// `wal.min_free_bytes`. Below it ingest answers 503 (contract 4.7).
    pub disk_min_free_bytes: u64,
    /// The rate at which shipping drains the WAL, in bytes per second.
    pub drain_bytes_per_s: u64,
    /// The in-flight byte budget refused the request's bytes.
    pub inflight_exceeded: bool,
    /// The project's token bucket refused the request.
    pub bucket_exceeded: bool,
    /// A uniform random value from `Rng`, for the 429 jitter.
    pub jitter: u64,
}

/// The admission outcome before the ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Append the request to the WAL.
    Admit,
    /// HTTP 429 with `Retry-After`; gRPC `RESOURCE_EXHAUSTED` with `RetryInfo`.
    TooManyRequests {
        /// Seconds, from [`THROTTLED_MIN_S`] to [`THROTTLED_MAX_S`].
        retry_after_s: u32,
    },
    /// HTTP 503 with `Retry-After`; gRPC `UNAVAILABLE` with `RetryInfo`.
    Unavailable {
        /// Seconds, from [`UNAVAILABLE_MIN_S`] to [`UNAVAILABLE_MAX_S`].
        retry_after_s: u32,
    },
}

/// The 90% high-water mark of the WAL, rounded down and free of overflow.
const fn high_water(max_bytes: u64) -> u64 {
    max_bytes - max_bytes.div_ceil(10)
}

/// The admission decision table (contract 4.8), in precedence order:
///
/// 1. Key unknown while Postgres is unreachable: 503 after [`UNAVAILABLE_MIN_S`], since the key feed
///    picks the key up within seconds of Postgres returning.
/// 2. WAL failed: 503 after [`UNAVAILABLE_MAX_S`]; the WAL stays failed until a restart.
/// 3. WAL above its high-water mark or disk below its floor: 503 after the time the backlog takes to
///    drain ([`retry_after_unavailable`]).
/// 4. In-flight byte budget or project token bucket exceeded: 429 after a jittered 1 to 5 s.
/// 5. Otherwise admit.
pub const fn decide(signals: &Signals) -> Decision {
    if signals.key_unknown_control_plane_down {
        return Decision::Unavailable {
            retry_after_s: UNAVAILABLE_MIN_S,
        };
    }
    if signals.wal_failed {
        return Decision::Unavailable {
            retry_after_s: UNAVAILABLE_MAX_S,
        };
    }
    let above_high_water = signals
        .wal_bytes
        .saturating_sub(high_water(signals.wal_max_bytes));
    let below_floor = signals
        .disk_min_free_bytes
        .saturating_sub(signals.disk_free_bytes);
    let backlog = if above_high_water > below_floor {
        above_high_water
    } else {
        below_floor
    };
    if backlog > 0 {
        return Decision::Unavailable {
            retry_after_s: retry_after_unavailable(backlog, signals.drain_bytes_per_s),
        };
    }
    if signals.inflight_exceeded || signals.bucket_exceeded {
        return Decision::TooManyRequests {
            retry_after_s: retry_after_throttled(signals.jitter),
        };
    }
    Decision::Admit
}

/// The `Retry-After` of a 429: 1 to 5 s, chosen by `jitter` (a uniform value from `Rng`) so that
/// throttled clients spread their retries.
pub const fn retry_after_throttled(jitter: u64) -> u32 {
    let span = (THROTTLED_MAX_S - THROTTLED_MIN_S + 1) as u64;
    THROTTLED_MIN_S + (jitter % span) as u32
}

/// The `Retry-After` of a 503, scaled to the drain rate: the seconds it takes to drain
/// `backlog_bytes` at `drain_bytes_per_s`, rounded up and clamped to 5 to 30 s. A stalled drain
/// waits the longest.
pub const fn retry_after_unavailable(backlog_bytes: u64, drain_bytes_per_s: u64) -> u32 {
    if drain_bytes_per_s == 0 {
        return UNAVAILABLE_MAX_S;
    }
    let seconds = backlog_bytes.div_ceil(drain_bytes_per_s);
    if seconds < UNAVAILABLE_MIN_S as u64 {
        UNAVAILABLE_MIN_S
    } else if seconds > UNAVAILABLE_MAX_S as u64 {
        UNAVAILABLE_MAX_S
    } else {
        seconds as u32
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// For every combination of signals, a 429 waits 1 to 5 s, a 503 waits 5 to 30 s, and the
    /// request is admitted only when no condition applies.
    #[kani::proof]
    fn every_decision_is_in_its_band() {
        let signals = Signals {
            key_unknown_control_plane_down: kani::any(),
            wal_failed: kani::any(),
            wal_bytes: kani::any(),
            wal_max_bytes: kani::any(),
            disk_free_bytes: kani::any(),
            disk_min_free_bytes: kani::any(),
            drain_bytes_per_s: kani::any(),
            inflight_exceeded: kani::any(),
            bucket_exceeded: kani::any(),
            jitter: kani::any(),
        };
        match decide(&signals) {
            Decision::Admit => {
                assert!(!signals.key_unknown_control_plane_down && !signals.wal_failed);
                assert!(!signals.inflight_exceeded && !signals.bucket_exceeded);
                assert!(signals.wal_bytes <= high_water(signals.wal_max_bytes));
                assert!(signals.disk_free_bytes >= signals.disk_min_free_bytes);
            }
            Decision::TooManyRequests { retry_after_s } => {
                assert!(retry_after_s >= THROTTLED_MIN_S && retry_after_s <= THROTTLED_MAX_S);
            }
            Decision::Unavailable { retry_after_s } => {
                assert!(retry_after_s >= UNAVAILABLE_MIN_S && retry_after_s <= UNAVAILABLE_MAX_S);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const GIB: u64 = 1 << 30;

    fn healthy() -> Signals {
        Signals {
            wal_bytes: GIB,
            wal_max_bytes: 16 * GIB,
            disk_free_bytes: 100 * GIB,
            disk_min_free_bytes: GIB,
            drain_bytes_per_s: 100_000_000,
            ..Signals::default()
        }
    }

    #[test]
    fn the_table() {
        assert_eq!(decide(&healthy()), Decision::Admit);
        let throttled = |s: Signals| match decide(&s) {
            Decision::TooManyRequests { retry_after_s } => Some(retry_after_s),
            _ => None,
        };
        let jitter = |jitter| Signals {
            inflight_exceeded: true,
            jitter,
            ..healthy()
        };
        let waits: Vec<_> = (0..5).map(|j| throttled(jitter(j))).collect();
        assert_eq!(waits, [Some(1), Some(2), Some(3), Some(4), Some(5)]);
        let bucket = Signals {
            bucket_exceeded: true,
            ..healthy()
        };
        assert_eq!(throttled(bucket), Some(1));
        // The WAL above 90% of 16 GiB by 1 GiB, draining 100 MB/s: 11 s.
        let high = Signals {
            wal_bytes: 16 * GIB - 16 * GIB / 10 + GIB,
            inflight_exceeded: true,
            ..healthy()
        };
        assert_eq!(decide(&high), Decision::Unavailable { retry_after_s: 11 });
        // Exactly at the high-water mark is still admitted.
        let at_mark = Signals {
            wal_bytes: high_water(16 * GIB),
            ..healthy()
        };
        assert_eq!(decide(&at_mark), Decision::Admit);
        // Disk below its floor by 100 MB: the shortest 503.
        let low_disk = Signals {
            disk_free_bytes: GIB - 100_000_000,
            ..healthy()
        };
        assert_eq!(
            decide(&low_disk),
            Decision::Unavailable { retry_after_s: 5 }
        );
        let stalled = Signals {
            drain_bytes_per_s: 0,
            ..low_disk
        };
        assert_eq!(
            decide(&stalled),
            Decision::Unavailable { retry_after_s: 30 }
        );
        let failed = Signals {
            wal_failed: true,
            ..healthy()
        };
        assert_eq!(decide(&failed), Decision::Unavailable { retry_after_s: 30 });
        let unknown_key = Signals {
            key_unknown_control_plane_down: true,
            wal_failed: true,
            ..healthy()
        };
        assert_eq!(
            decide(&unknown_key),
            Decision::Unavailable { retry_after_s: 5 }
        );
    }

    #[test]
    fn unavailable_waits_scale_with_the_drain_rate() {
        assert_eq!(retry_after_unavailable(1, 1_000), 5);
        assert_eq!(retry_after_unavailable(6_000, 1_000), 6);
        assert_eq!(retry_after_unavailable(6_001, 1_000), 7);
        assert_eq!(retry_after_unavailable(30_000, 1_000), 30);
        assert_eq!(retry_after_unavailable(u64::MAX, 1), 30);
        assert_eq!(retry_after_unavailable(0, 0), 30);
    }

    fn signals() -> impl Strategy<Value = Signals> {
        let bytes = prop_oneof![0..64u64, any::<u64>()];
        (
            (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()),
            (bytes.clone(), bytes.clone(), bytes.clone(), bytes.clone()),
            (bytes, any::<u64>()),
        )
            .prop_map(
                |((key, failed, inflight, bucket), (wal, max, free, floor), (drain, jitter))| {
                    Signals {
                        key_unknown_control_plane_down: key,
                        wal_failed: failed,
                        wal_bytes: wal,
                        wal_max_bytes: max,
                        disk_free_bytes: free,
                        disk_min_free_bytes: floor,
                        drain_bytes_per_s: drain,
                        inflight_exceeded: inflight,
                        bucket_exceeded: bucket,
                        jitter,
                    }
                },
            )
    }

    proptest! {
        /// The precedence of the table holds for random signals, and every wait is in its band.
        #[test]
        fn precedence_and_bands(s in signals()) {
            let above = u128::from(s.wal_bytes) * 10 > u128::from(s.wal_max_bytes) * 9;
            let below = s.disk_free_bytes < s.disk_min_free_bytes;
            let decision = decide(&s);
            if s.key_unknown_control_plane_down {
                prop_assert_eq!(decision, Decision::Unavailable { retry_after_s: 5 });
            } else if s.wal_failed {
                prop_assert_eq!(decision, Decision::Unavailable { retry_after_s: 30 });
            } else if above || below {
                let Decision::Unavailable { retry_after_s } = decision else {
                    return Err(TestCaseError::fail("expected 503"));
                };
                prop_assert!((5..=30).contains(&retry_after_s));
            } else if s.inflight_exceeded || s.bucket_exceeded {
                let Decision::TooManyRequests { retry_after_s } = decision else {
                    return Err(TestCaseError::fail("expected 429"));
                };
                prop_assert!((1..=5).contains(&retry_after_s));
            } else {
                prop_assert_eq!(decision, Decision::Admit);
            }
        }
    }
}
