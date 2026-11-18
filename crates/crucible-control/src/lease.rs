//! The holder's side of a lease (contract 11.7): how long a node may act on a lease without asking Postgres.
//!
//! Pure arithmetic on the node's monotonic clock, in nanoseconds. The reading must be taken before the acquire
//! or renew call is sent, so that the local window ends before the database's.

use std::time::Duration;

use crate::types::Lease;

/// The lease time to live.
pub const LEASE_TTL: Duration = Duration::from_secs(15);
/// How often a holder renews.
pub const LEASE_RENEW_EVERY: Duration = Duration::from_secs(5);
/// How much earlier than the database a holder gives up its lease.
pub const LEASE_SAFETY_MARGIN: Duration = Duration::from_secs(2);

/// A lease with the local window in which its holder may act as leader.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HeldLease {
    /// The lease as Postgres returned it.
    pub lease: Lease,
    valid_until_nanos: u64,
}

impl HeldLease {
    /// The lease is valid locally until `asked_at_nanos + ttl - 2 s`, where `asked_at_nanos` is the monotonic
    /// clock read before the acquire or renew call was sent.
    pub fn new(lease: Lease, asked_at_nanos: u64, ttl: Duration) -> Self {
        let window = ttl.saturating_sub(LEASE_SAFETY_MARGIN);
        let window = u64::try_from(window.as_nanos()).unwrap_or(u64::MAX);
        Self {
            lease,
            valid_until_nanos: asked_at_nanos.saturating_add(window),
        }
    }

    /// Whether the holder may still act as leader at this monotonic time.
    pub const fn is_valid_at(&self, now_nanos: u64) -> bool {
        now_nanos < self.valid_until_nanos
    }

    /// The monotonic time at which the local window ends.
    pub const fn valid_until_nanos(&self) -> u64 {
        self.valid_until_nanos
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::Timestamp;
    use proptest::prelude::*;

    use super::*;
    use crate::types::LeaseName;

    fn lease() -> Lease {
        Lease {
            name: LeaseName::Lifecycle,
            holder: "node-1".to_owned(),
            fencing_token: 7,
            expires_at: Timestamp::from_micros(0),
        }
    }

    #[test]
    fn the_local_window_is_the_ttl_minus_two_seconds() {
        let held = HeldLease::new(lease(), 1_000, LEASE_TTL);
        assert_eq!(held.valid_until_nanos(), 1_000 + 13_000_000_000);
        assert!(held.is_valid_at(13_000_000_999));
        assert!(!held.is_valid_at(13_000_001_000));
        // A ttl at or under the margin is never valid.
        assert!(!HeldLease::new(lease(), 5, Duration::from_secs(2)).is_valid_at(5));
    }

    proptest! {
        /// The local window always ends at least the safety margin before the ttl, measured from the read
        /// taken before the call.
        #[test]
        fn the_local_window_ends_before_the_database_window(asked in 0u64..u64::MAX / 2, ttl_ms in 0u64..600_000, at in 0u64..u64::MAX) {
            let ttl = Duration::from_millis(ttl_ms);
            let held = HeldLease::new(lease(), asked, ttl);
            if held.is_valid_at(at) {
                let margin = u64::try_from(LEASE_SAFETY_MARGIN.as_nanos()).unwrap();
                let ttl_nanos = u64::try_from(ttl.as_nanos()).unwrap();
                prop_assert!(at + margin < asked + ttl_nanos);
            }
        }
    }
}
