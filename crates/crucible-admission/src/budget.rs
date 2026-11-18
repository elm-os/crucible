//! Byte budgets: reserve and release bytes against a cap (contract 4.8; Kani obligation in 4.9).

use core::sync::atomic::{AtomicU64, Ordering};

/// A byte budget shared by concurrent holders, such as `ingest.inflight_bytes` or
/// `wal.unshipped_bytes`. A reservation never takes the total above the cap and a release never takes
/// it below zero; either is refused instead, leaving the budget unchanged.
#[derive(Debug)]
pub struct ByteBudget {
    cap: u64,
    used: AtomicU64,
}

/// A reservation that would take the budget above its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("reserving {requested} bytes would exceed the budget ({available} available)")]
pub struct BudgetExceeded {
    /// The bytes asked for.
    pub requested: u64,
    /// The bytes that were free when the reservation was refused.
    pub available: u64,
}

/// A release of more bytes than are reserved. This is a caller bug; the budget stays unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("releasing {released} bytes exceeds the {reserved} reserved")]
pub struct OverRelease {
    /// The bytes the caller tried to release.
    pub released: u64,
    /// The bytes reserved at the time.
    pub reserved: u64,
}

impl ByteBudget {
    /// An empty budget with room for `cap` bytes.
    pub const fn new(cap: u64) -> Self {
        Self {
            cap,
            used: AtomicU64::new(0),
        }
    }

    /// The cap.
    pub const fn cap(&self) -> u64 {
        self.cap
    }

    /// The bytes reserved now; the budget's metric.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Reserves `bytes`, or refuses if the total would exceed the cap.
    pub fn try_reserve(&self, bytes: u64) -> Result<(), BudgetExceeded> {
        // The counter guards no other memory, so relaxed ordering is enough.
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                reserve(used, self.cap, bytes)
            })
            .map(|_| ())
            .map_err(|used| BudgetExceeded {
                requested: bytes,
                available: self.cap.saturating_sub(used),
            })
    }

    /// Releases `bytes` reserved earlier, or refuses if fewer are reserved.
    pub fn release(&self, bytes: u64) -> Result<(), OverRelease> {
        self.used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                release(used, bytes)
            })
            .map(|_| ())
            .map_err(|used| OverRelease {
                released: bytes,
                reserved: used,
            })
    }
}

/// The accounting kernel: the total after reserving `bytes`, if it stays within `cap`.
const fn reserve(used: u64, cap: u64, bytes: u64) -> Option<u64> {
    match used.checked_add(bytes) {
        Some(total) if total <= cap => Some(total),
        _ => None,
    }
}

/// The accounting kernel: the total after releasing `bytes`, if that many are reserved.
const fn release(used: u64, bytes: u64) -> Option<u64> {
    used.checked_sub(bytes)
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// A reservation never exceeds the cap or wraps, adds exactly the bytes asked for, and is refused
    /// only when it would exceed the cap.
    #[kani::proof]
    fn reserve_never_exceeds_the_cap() {
        let used: u64 = kani::any();
        let cap: u64 = kani::any();
        let bytes: u64 = kani::any();
        kani::assume(used <= cap);
        match reserve(used, cap, bytes) {
            Some(total) => {
                assert!(total <= cap);
                assert!(total >= used && total - used == bytes);
            }
            None => assert!(bytes > cap - used),
        }
    }

    /// A release never underflows, removes exactly the bytes asked for, and is refused only when
    /// fewer bytes are reserved.
    #[kani::proof]
    fn release_never_underflows() {
        let used: u64 = kani::any();
        let bytes: u64 = kani::any();
        match release(used, bytes) {
            Some(rest) => assert!(rest <= used && used - rest == bytes),
            None => assert!(bytes > used),
        }
    }

    /// Any sequence of reservations and releases on a shared budget keeps it within the cap, and a
    /// refused operation leaves it unchanged.
    #[kani::proof]
    #[kani::unwind(5)]
    fn budget_stays_within_the_cap() {
        let budget = ByteBudget::new(kani::any());
        for _ in 0..4 {
            let bytes: u64 = kani::any();
            let before = budget.used();
            let accepted = if kani::any() {
                budget.try_reserve(bytes).is_ok()
            } else {
                budget.release(bytes).is_ok()
            };
            if !accepted {
                assert!(budget.used() == before);
            }
            assert!(budget.used() <= budget.cap());
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn refusals_leave_the_budget_unchanged() {
        let budget = ByteBudget::new(100);
        assert_eq!(budget.try_reserve(60), Ok(()));
        assert_eq!(
            budget.try_reserve(41),
            Err(BudgetExceeded {
                requested: 41,
                available: 40
            })
        );
        assert_eq!(budget.try_reserve(40), Ok(()));
        assert_eq!(budget.used(), 100);
        assert_eq!(
            budget.release(101),
            Err(OverRelease {
                released: 101,
                reserved: 100
            })
        );
        assert_eq!(budget.release(100), Ok(()));
        assert_eq!(budget.used(), 0);
        // Sums that would wrap are refused, not wrapped.
        let full = ByteBudget::new(u64::MAX);
        assert_eq!(full.try_reserve(u64::MAX), Ok(()));
        assert!(full.try_reserve(1).is_err());
        assert_eq!(full.used(), u64::MAX);
    }

    #[derive(Debug, Clone)]
    enum Op {
        /// A holder reserves bytes.
        Reserve(usize, u64),
        /// A holder releases its latest reservation, or tries to release more than is reserved
        /// when it holds none.
        Release(usize, u64),
    }

    fn op() -> impl Strategy<Value = Op> {
        let bytes = prop_oneof![0..64u64, any::<u64>(), Just(u64::MAX)];
        prop_oneof![
            (0..4usize, bytes.clone()).prop_map(|(h, b)| Op::Reserve(h, b)),
            (0..4usize, bytes).prop_map(|(h, b)| Op::Release(h, b)),
        ]
    }

    proptest! {
        /// Under random interleavings of four holders reserving and releasing, the budget always
        /// equals the sum of the outstanding reservations, never exceeds its cap, admits exactly the
        /// reservations that fit, and refuses every over-release without change.
        #[test]
        fn accounting_under_random_interleavings(
            cap in prop_oneof![0..256u64, any::<u64>(), Just(u64::MAX)],
            ops in proptest::collection::vec(op(), 0..64),
        ) {
            let budget = ByteBudget::new(cap);
            let mut held: [Vec<u64>; 4] = Default::default();
            let mut total: u128 = 0;
            for op in ops {
                match op {
                    Op::Reserve(holder, bytes) => {
                        let fits = total + u128::from(bytes) <= u128::from(cap);
                        prop_assert_eq!(budget.try_reserve(bytes).is_ok(), fits);
                        if fits {
                            held[holder].push(bytes);
                            total += u128::from(bytes);
                        }
                    }
                    Op::Release(holder, extra) => match held[holder].pop() {
                        Some(bytes) => {
                            prop_assert_eq!(budget.release(bytes), Ok(()));
                            total -= u128::from(bytes);
                        }
                        None => {
                            let over = u64::try_from(total).unwrap().saturating_add(extra.max(1));
                            if u128::from(over) > total {
                                prop_assert!(budget.release(over).is_err());
                            }
                        }
                    },
                }
                prop_assert_eq!(u128::from(budget.used()), total);
                prop_assert!(budget.used() <= budget.cap());
            }
        }
    }
}
