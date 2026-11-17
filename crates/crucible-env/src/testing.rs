//! Deterministic test doubles for this crate's unit tests. They use no ambient time, randomness or threads.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crucible_records::Timestamp;
use futures::future::BoxFuture;
use futures::task::noop_waker_ref;

use crate::clock::{Clock, Instant};
use crate::spawner::{ShutdownSignal, ShutdownTrigger, Spawner, TaskAbort, shutdown_channel};

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
struct ClockState {
    now: u64,
    wall_micros: i64,
    wakers: Vec<Waker>,
}

/// A clock that moves only when the test says so.
#[derive(Debug)]
pub(crate) struct ManualClock {
    state: Arc<Mutex<ClockState>>,
}

impl ManualClock {
    pub(crate) fn new() -> Self {
        let state = ClockState {
            now: 0,
            wall_micros: 1_800_000_000_000_000,
            wakers: Vec::new(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Advances the monotonic clock (the wall clock stays where it is) and wakes every sleeper.
    pub(crate) fn advance(&self, d: Duration) {
        let wakers = {
            let mut state = lock(&self.state);
            state.now += u64::try_from(d.as_nanos()).unwrap();
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    pub(crate) fn set_wall_micros(&self, micros: i64) {
        lock(&self.state).wall_micros = micros;
    }
}

struct ManualSleep {
    state: Arc<Mutex<ClockState>>,
    deadline: u64,
}

impl Future for ManualSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = lock(&self.state);
        if state.now >= self.deadline {
            Poll::Ready(())
        } else {
            state.wakers.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        Instant::from_nanos(lock(&self.state).now)
    }

    fn wall_now(&self) -> Timestamp {
        Timestamp::from_micros(lock(&self.state).wall_micros)
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'static, ()> {
        Box::pin(ManualSleep {
            state: Arc::clone(&self.state),
            deadline: deadline.nanos(),
        })
    }
}

type TaskMap = Mutex<BTreeMap<u64, (&'static str, BoxFuture<'static, ()>)>>;

/// A single-threaded spawner: tasks run only when the test calls [`ManualSpawner::run_until_stalled`], and
/// blocking closures run inline, as contract 2.3 requires of simulation.
pub(crate) struct ManualSpawner {
    tasks: Arc<TaskMap>,
    next_id: Mutex<u64>,
    trigger: ShutdownTrigger,
    signal: ShutdownSignal,
}

impl fmt::Debug for ManualSpawner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManualSpawner").finish_non_exhaustive()
    }
}

impl ManualSpawner {
    pub(crate) fn new() -> Self {
        let (trigger, signal) = shutdown_channel();
        Self {
            tasks: Arc::default(),
            next_id: Mutex::new(0),
            trigger,
            signal,
        }
    }

    pub(crate) fn names(&self) -> Vec<&'static str> {
        lock(&self.tasks).values().map(|(name, _)| *name).collect()
    }

    /// Polls every task, in spawn order, until a full pass completes none of them.
    pub(crate) fn run_until_stalled(&self) {
        let mut cx = Context::from_waker(noop_waker_ref());
        loop {
            let ids: Vec<u64> = lock(&self.tasks).keys().copied().collect();
            let mut progressed = false;
            for id in ids {
                let Some((name, mut task)) = lock(&self.tasks).remove(&id) else {
                    continue;
                };
                if task.as_mut().poll(&mut cx).is_pending() {
                    lock(&self.tasks).insert(id, (name, task));
                } else {
                    progressed = true;
                }
            }
            if !progressed {
                return;
            }
        }
    }

    /// Asks the scope to stop.
    pub(crate) fn request_shutdown(&self) {
        self.trigger.trigger();
    }

    /// Drops every task, as a scope shutdown or a simulated crash does.
    pub(crate) fn abort_all(&self) {
        let tasks = std::mem::take(&mut *lock(&self.tasks));
        drop(tasks);
    }
}

#[derive(Debug)]
struct ManualAbort {
    tasks: std::sync::Weak<TaskMap>,
    id: u64,
}

impl TaskAbort for ManualAbort {
    fn abort(&self) {
        if let Some(tasks) = self.tasks.upgrade() {
            let task = lock(&tasks).remove(&self.id);
            drop(task);
        }
    }
}

#[derive(Debug)]
struct NoopAbort;

impl TaskAbort for NoopAbort {
    fn abort(&self) {}
}

impl Spawner for ManualSpawner {
    fn spawn_boxed(&self, name: &'static str, fut: BoxFuture<'static, ()>) -> Box<dyn TaskAbort> {
        let id = {
            let mut next = lock(&self.next_id);
            *next += 1;
            *next
        };
        lock(&self.tasks).insert(id, (name, fut));
        Box::new(ManualAbort {
            tasks: Arc::downgrade(&self.tasks),
            id,
        })
    }

    fn spawn_blocking_boxed(
        &self,
        _name: &'static str,
        f: Box<dyn FnOnce() + Send>,
    ) -> Box<dyn TaskAbort> {
        f();
        Box::new(NoopAbort)
    }

    fn shutdown(&self) -> ShutdownSignal {
        self.signal.clone()
    }
}
