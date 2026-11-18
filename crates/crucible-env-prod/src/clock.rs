//! The OS clocks, with tokio timers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crucible_env::{Clock, Instant};
use crucible_records::Timestamp;
use futures::future::BoxFuture;

/// `std::time::Instant` for monotonic time (origin: construction), `SystemTime` for the wall clock and
/// tokio's timer wheel for sleeps. Sleep futures must be polled on a tokio runtime with the time driver.
#[derive(Debug)]
pub struct SystemClock {
    origin: std::time::Instant,
}

impl SystemClock {
    /// Starts the monotonic clock at zero.
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::from_nanos(u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    fn wall_now(&self) -> Timestamp {
        let micros = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_micros()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_micros()).map_or(i64::MIN, |m| -m),
        };
        Timestamp::from_micros(micros)
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'static, ()> {
        match self
            .origin
            .checked_add(Duration::from_nanos(deadline.nanos()))
        {
            // The timer is created on first poll, so this method itself needs no runtime context.
            Some(at) => Box::pin(async move {
                tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await
            }),
            // Beyond what the OS clock can represent: never.
            None => Box::pin(futures::future::pending()),
        }
    }
}
