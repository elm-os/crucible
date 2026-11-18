//! Time (contract 2.1).

use std::fmt;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use crucible_records::Timestamp;
use futures::future::{BoxFuture, Either, select};

/// A monotonic instant: nanoseconds since an origin fixed by the clock that produced it. Instants from
/// different clocks are not comparable.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Instant(u64);

impl Instant {
    /// Builds an instant from nanoseconds since the clock's origin.
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Nanoseconds since the clock's origin.
    pub const fn nanos(self) -> u64 {
        self.0
    }

    /// This instant plus `d`, saturating at the far future.
    pub const fn saturating_add(self, d: Duration) -> Self {
        Self(self.0.saturating_add(duration_nanos(d)))
    }

    /// Time elapsed from `earlier` to `self`, or zero when `earlier` is later.
    pub const fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

const fn duration_nanos(d: Duration) -> u64 {
    let nanos = d.as_nanos();
    if nanos > u64::MAX as u128 {
        u64::MAX
    } else {
        nanos as u64
    }
}

/// The time source. Production reads the OS clocks; simulation uses a paused virtual clock.
pub trait Clock: fmt::Debug + Send + Sync {
    /// Monotonic time. Never decreases, including across wall-clock steps.
    fn now(&self) -> Instant;

    /// UTC wall-clock time. May step forward or backward.
    fn wall_now(&self) -> Timestamp;

    /// Completes no earlier than `deadline`. Dropping the future cancels the wait.
    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'static, ()>;

    /// Completes no earlier than `d` from now. Dropping the future cancels the wait.
    fn sleep(&self, d: Duration) -> BoxFuture<'static, ()> {
        self.sleep_until(self.now().saturating_add(d))
    }
}

/// The error of [`ClockExt::timeout`]: the deadline passed first.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("deadline elapsed")]
pub struct Elapsed;

/// Generic conveniences over any [`Clock`], including `dyn Clock`.
pub trait ClockExt: Clock {
    /// Runs `fut` for at most `d`. The future is polled before the timer, so a future that is ready at the
    /// deadline still wins. On expiry the future is dropped before this returns.
    fn timeout<F: Future>(
        &self,
        d: Duration,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, Elapsed>> {
        self.timeout_at(self.now().saturating_add(d), fut)
    }

    /// Like [`ClockExt::timeout`] with an absolute deadline.
    fn timeout_at<F: Future>(
        &self,
        deadline: Instant,
        fut: F,
    ) -> impl Future<Output = Result<F::Output, Elapsed>> {
        let sleep = self.sleep_until(deadline);
        async move {
            let fut = pin!(fut);
            match select(fut, sleep).await {
                Either::Left((out, _)) => Ok(out),
                Either::Right(((), _)) => Err(Elapsed),
            }
        }
    }
}

impl<C: Clock + ?Sized> ClockExt for C {}

/// What an [`Interval`] does about ticks that were due while the caller was busy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MissedTicks {
    /// Drop them and resume on the original grid at the next multiple of the period.
    Skip,
    /// Deliver them back to back until the schedule has caught up.
    Burst,
}

/// A periodic timer. The first tick is one period after creation.
#[derive(Debug)]
pub struct Interval {
    clock: Arc<dyn Clock>,
    period: u64,
    next: Instant,
    missed: MissedTicks,
}

impl Interval {
    /// Creates a timer that first ticks one `period` from now. A zero period is treated as one nanosecond.
    pub fn new(clock: Arc<dyn Clock>, period: Duration, missed: MissedTicks) -> Self {
        let period = duration_nanos(period).max(1);
        let next = Instant(clock.now().0.saturating_add(period));
        Self {
            clock,
            period,
            next,
            missed,
        }
    }

    /// Waits for the next tick and returns the instant it was scheduled for. Cancel-safe: a dropped call
    /// loses no tick.
    pub async fn tick(&mut self) -> Instant {
        self.clock.sleep_until(self.next).await;
        let tick = self.next;
        let behind = self.clock.now().0.saturating_sub(tick.0);
        let periods = match self.missed {
            MissedTicks::Burst => 1,
            MissedTicks::Skip => behind / self.period + 1,
        };
        self.next = Instant(tick.0.saturating_add(self.period.saturating_mul(periods)));
        tick
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    use futures::FutureExt;
    use futures::executor::block_on;

    use super::*;
    use crate::testing::ManualClock;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn instant_arithmetic_saturates() {
        let t = Instant::from_nanos(5);
        assert_eq!(t.saturating_add(Duration::from_nanos(7)).nanos(), 12);
        assert_eq!(
            Instant::from_nanos(u64::MAX).saturating_add(MS).nanos(),
            u64::MAX
        );
        assert_eq!(t.saturating_add(Duration::MAX).nanos(), u64::MAX);
        assert_eq!(
            t.saturating_duration_since(Instant::from_nanos(9)),
            Duration::ZERO
        );
        assert_eq!(
            Instant::from_nanos(9).saturating_duration_since(t),
            Duration::from_nanos(4)
        );
    }

    #[test]
    fn sleep_wakes_no_earlier_than_requested() {
        let clock = ManualClock::new();
        let mut sleep = clock.sleep(10 * MS);
        assert_eq!(sleep.as_mut().now_or_never(), None);
        clock.advance(10 * MS - Duration::from_nanos(1));
        assert_eq!(sleep.as_mut().now_or_never(), None);
        clock.advance(Duration::from_nanos(1));
        assert_eq!(sleep.as_mut().now_or_never(), Some(()));
    }

    #[test]
    fn sleep_wakes_its_task_when_the_clock_advances() {
        let clock = Arc::new(ManualClock::new());
        let sleeper = clock.sleep(5 * MS);
        let advancer = {
            let clock = Arc::clone(&clock);
            async move { clock.advance(5 * MS) }
        };
        block_on(async { futures::join!(sleeper, advancer) });
        assert_eq!(clock.now().nanos(), 5_000_000);
    }

    struct SetOnDrop(Arc<AtomicBool>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn timeout_returns_the_value_when_the_future_finishes_first() {
        let clock = ManualClock::new();
        assert_eq!(block_on(clock.timeout(MS, async { 7 })), Ok(7));
    }

    #[test]
    fn timeout_prefers_a_future_that_is_ready_at_the_deadline() {
        let clock = ManualClock::new();
        let inner = clock.sleep(MS).map(|()| 7);
        let mut timed = pin!(clock.timeout(MS, inner));
        assert_eq!(timed.as_mut().now_or_never(), None);
        clock.advance(MS);
        assert_eq!(timed.as_mut().now_or_never(), Some(Ok(7)));
    }

    #[test]
    fn timeout_drops_the_inner_future_on_expiry() {
        let clock = ManualClock::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = SetOnDrop(Arc::clone(&dropped));
        let inner = async move {
            let _guard = guard;
            futures::future::pending::<()>().await;
        };
        let dyn_clock: &dyn Clock = &clock;
        let mut timed = pin!(dyn_clock.timeout(3 * MS, inner));
        assert_eq!(timed.as_mut().now_or_never(), None);
        clock.advance(2 * MS);
        assert_eq!(timed.as_mut().now_or_never(), None);
        assert!(!dropped.load(Ordering::SeqCst));
        clock.advance(MS);
        assert_eq!(timed.as_mut().now_or_never(), Some(Err(Elapsed)));
        assert!(dropped.load(Ordering::SeqCst));
    }

    fn poll_tick(interval: &mut Interval) -> Poll<Instant> {
        match interval.tick().now_or_never() {
            Some(t) => Poll::Ready(t),
            None => Poll::Pending,
        }
    }

    fn ms(n: u64) -> Instant {
        Instant::from_nanos(n * 1_000_000)
    }

    #[test]
    fn interval_first_tick_is_after_one_period() {
        let clock = Arc::new(ManualClock::new());
        let mut interval = Interval::new(clock.clone(), 10 * MS, MissedTicks::Burst);
        assert_eq!(poll_tick(&mut interval), Poll::Pending);
        clock.advance(9 * MS);
        assert_eq!(poll_tick(&mut interval), Poll::Pending);
        clock.advance(MS);
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(10)));
        assert_eq!(poll_tick(&mut interval), Poll::Pending);
    }

    #[test]
    fn interval_burst_delivers_every_missed_tick() {
        let clock = Arc::new(ManualClock::new());
        let mut interval = Interval::new(clock.clone(), 10 * MS, MissedTicks::Burst);
        clock.advance(35 * MS);
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(10)));
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(20)));
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(30)));
        assert_eq!(poll_tick(&mut interval), Poll::Pending);
        clock.advance(5 * MS);
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(40)));
    }

    #[test]
    fn interval_skip_drops_missed_ticks_and_keeps_the_grid() {
        let clock = Arc::new(ManualClock::new());
        let mut interval = Interval::new(clock.clone(), 10 * MS, MissedTicks::Skip);
        clock.advance(35 * MS);
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(10)));
        assert_eq!(poll_tick(&mut interval), Poll::Pending);
        clock.advance(5 * MS);
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(40)));
        clock.advance(10 * MS);
        assert_eq!(poll_tick(&mut interval), Poll::Ready(ms(50)));
    }

    #[test]
    fn interval_with_a_zero_period_does_not_divide_by_zero() {
        let clock = Arc::new(ManualClock::new());
        let mut interval = Interval::new(clock.clone(), Duration::ZERO, MissedTicks::Skip);
        clock.advance(MS);
        assert_eq!(
            poll_tick(&mut interval),
            Poll::Ready(Instant::from_nanos(1))
        );
    }
}
