//! UUIDv7 minting (contract 1.1).

use std::sync::{Mutex, PoisonError};

use crucible_records::Uuid;

use crate::clock::Clock;
use crate::rng::Rng;

const UNIX_MS_MAX: u64 = (1 << 48) - 1;
const RAND_BITS: u32 = 74;
const RAND_MAX: u128 = (1 << RAND_BITS) - 1;

/// The state that makes ids strictly increasing: the last id's milliseconds and random field.
///
/// Contract 1.1 names `crucible_env::new_id(&Clock, &Rng)`, but "strictly increasing per generator" needs
/// the previous id, and product code may keep no state in statics (contract 2.3). The state therefore lives
/// in this value. [`crate::Env`] owns one, shared by all of its clones, and [`crate::Env::new_id`] is what
/// product code calls; a component that wants an independent sequence builds its own `IdGen` and calls
/// [`new_id`].
#[derive(Debug, Default)]
pub struct IdGen {
    last: Mutex<Option<(u64, u128)>>,
}

impl IdGen {
    /// Creates a generator with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints the next id: 48 bits of Unix milliseconds from `Clock::wall_now` and 74 random bits from `rng`.
    ///
    /// When the wall clock has not advanced past the previous id's millisecond (same millisecond, or a
    /// backward step) the previous value's random field is incremented by one instead. If that field is
    /// exhausted the millisecond is advanced by one with fresh random bits. Ids stop increasing only at
    /// the end of the 48-bit millisecond range (year 10889).
    pub fn next(&self, clock: &dyn Clock, rng: &dyn Rng) -> Uuid {
        let now_ms = u64::try_from(clock.wall_now().micros() / 1000)
            .unwrap_or(0)
            .min(UNIX_MS_MAX);
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let (ms, rand) = match *last {
            Some((last_ms, last_rand)) if now_ms <= last_ms => {
                if last_rand < RAND_MAX {
                    (last_ms, last_rand + 1)
                } else {
                    ((last_ms + 1).min(UNIX_MS_MAX), random_field(rng))
                }
            }
            _ => (now_ms, random_field(rng)),
        };
        *last = Some((ms, rand));
        Uuid::from_v7_parts(ms, rand)
    }
}

fn random_field(rng: &dyn Rng) -> u128 {
    let mut bytes = [0; 16];
    rng.fill_bytes(&mut bytes);
    u128::from_le_bytes(bytes) & RAND_MAX
}

/// Mints a UUIDv7 from `generator`'s state, `clock` and `rng`. See [`IdGen`] for why the generator is
/// explicit.
pub fn new_id(generator: &IdGen, clock: &dyn Clock, rng: &dyn Rng) -> Uuid {
    generator.next(clock, rng)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::*;
    use crate::rng::ChaChaRng;
    use crate::testing::ManualClock;

    /// A generator bundled with its clock-independent random stream.
    struct Ids(IdGen, ChaChaRng);

    impl Ids {
        fn next(&self, clock: &ManualClock) -> Uuid {
            new_id(&self.0, clock, &self.1)
        }
    }

    fn generator(seed: u64) -> Ids {
        Ids(IdGen::new(), ChaChaRng::from_u64(seed))
    }

    #[test]
    fn ids_carry_the_wall_clock_milliseconds_and_the_v7_layout() {
        let clock = ManualClock::new();
        clock.set_wall_micros(1_800_000_000_123_999);
        let id = generator(1).next(&clock);
        let (ms, _) = id.v7_parts().unwrap();
        assert_eq!(ms, 1_800_000_000_123);
    }

    #[test]
    fn a_frozen_clock_increments_the_random_field() {
        let clock = ManualClock::new();
        let ids = generator(2);
        let (ms, rand) = ids.next(&clock).v7_parts().unwrap();
        for i in 1..=1000 {
            assert_eq!(ids.next(&clock).v7_parts().unwrap(), (ms, rand + i));
        }
    }

    #[test]
    fn an_exhausted_random_field_moves_to_the_next_millisecond() {
        let clock = ManualClock::new();
        let ids = generator(3);
        let first = ids.next(&clock);
        let (ms, _) = first.v7_parts().unwrap();
        *ids.0.last.lock().unwrap() = Some((ms, RAND_MAX));
        let top = Uuid::from_v7_parts(ms, RAND_MAX);
        let next = ids.next(&clock);
        assert!(next > top);
        assert_eq!(next.v7_parts().unwrap().0, ms + 1);
        // The clock is still behind the generator; ids keep increasing.
        assert!(ids.next(&clock) > next);
    }

    #[test]
    fn a_time_before_the_epoch_is_clamped() {
        let clock = ManualClock::new();
        clock.set_wall_micros(-5);
        let ids = generator(4);
        let a = ids.next(&clock);
        assert_eq!(a.v7_parts().unwrap().0, 0);
        assert!(ids.next(&clock) > a);
    }

    #[test]
    fn same_seed_and_clock_give_the_same_ids() {
        let clock = ManualClock::new();
        let (a, b) = (generator(5), generator(5));
        for _ in 0..10 {
            assert_eq!(a.next(&clock), b.next(&clock));
            clock.advance(Duration::from_millis(1));
        }
    }

    proptest! {
        /// Strictly increasing under a wall clock that freezes, advances and steps backward.
        #[test]
        fn ids_strictly_increase_under_any_wall_clock(
            seed: u64,
            steps in prop::collection::vec(-5_000_000i64..5_000_000, 1..200),
        ) {
            let clock = ManualClock::new();
            let ids = generator(seed);
            let mut wall = 1_800_000_000_000_000i64;
            let mut previous = ids.next(&clock);
            for (i, step) in steps.into_iter().enumerate() {
                // Every third id is minted without moving the clock at all.
                if i % 3 != 0 {
                    wall += step;
                    clock.set_wall_micros(wall);
                }
                let id = ids.next(&clock);
                prop_assert!(id > previous, "{id:?} is not above {previous:?}");
                prop_assert!(id.v7_parts().is_some());
                previous = id;
            }
        }
    }
}
