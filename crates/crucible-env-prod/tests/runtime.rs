//! The production clock and spawner on a real tokio runtime.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use crucible_env::{
    Cancelled, Clock, ClockExt, Elapsed, Env, Interval, MemoryRecorder, MissedTicks, Rng, Root,
    Spawner,
};
use crucible_env_prod::{PortableDisk, SystemClock, TokioNet, TokioSpawner, os_seeded_rng};

const MS: Duration = Duration::from_millis(1);

#[tokio::test]
async fn sleep_wakes_no_earlier_than_requested_and_in_deadline_order() {
    let clock = SystemClock::new();
    let start = clock.now();
    let (long, short) = (clock.sleep(60 * MS), clock.sleep(20 * MS));
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = |label, sleep: futures::future::BoxFuture<'static, ()>| {
        let order = Arc::clone(&order);
        async move {
            sleep.await;
            order.lock().unwrap().push(label);
        }
    };
    tokio::join!(record("long", long), record("short", short));
    assert_eq!(*order.lock().unwrap(), ["short", "long"]);
    assert!(clock.now().saturating_duration_since(start) >= 60 * MS);

    // A deadline in the past completes at once.
    clock.sleep_until(start).await;
}

#[tokio::test]
async fn clocks_are_monotonic_and_plausible() {
    let clock = SystemClock::new();
    let mut previous = clock.now();
    for _ in 0..1000 {
        let now = clock.now();
        assert!(now >= previous);
        previous = now;
    }
    // After 2025-01-01 and inside the contract's valid range.
    let wall = clock.wall_now();
    assert!(wall.micros() > 1_735_689_600_000_000 && wall.is_valid());
}

#[tokio::test]
async fn timeout_returns_the_value_or_elapses() {
    let clock = SystemClock::new();
    let fast = clock.timeout(500 * MS, async {
        clock.sleep(10 * MS).await;
        "done"
    });
    assert_eq!(fast.await, Ok("done"));

    let start = clock.now();
    let slow = clock.timeout(20 * MS, clock.sleep(10_000 * MS));
    assert_eq!(slow.await, Err(Elapsed));
    let waited = clock.now().saturating_duration_since(start);
    assert!(waited >= 20 * MS && waited < 5_000 * MS, "{waited:?}");
}

#[tokio::test]
async fn interval_ticks_on_its_grid() {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let start = clock.now();
    let mut interval = Interval::new(Arc::clone(&clock), 15 * MS, MissedTicks::Skip);
    let first = interval.tick().await;
    let second = interval.tick().await;
    assert_eq!(second.saturating_duration_since(first), 15 * MS);
    assert!(first.saturating_duration_since(start) >= 15 * MS);
    assert!(clock.now() >= second);
}

fn spawner() -> Arc<TokioSpawner> {
    Arc::new(TokioSpawner::new(tokio::runtime::Handle::current()))
}

fn env(spawner: &Arc<TokioSpawner>) -> Env {
    let clock = Arc::new(SystemClock::new());
    let runtime = tokio::runtime::Handle::current();
    Env::new(
        clock.clone(),
        Arc::new(os_seeded_rng().unwrap()),
        spawner.clone(),
        Arc::new(
            PortableDisk::new(runtime, &std::collections::BTreeMap::<Root, _>::new()).unwrap(),
        ),
        Arc::new(TokioNet::new(clock)),
        Arc::new(MemoryRecorder::new()),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn tasks_are_named_joined_and_aborted() {
    let spawner = spawner();
    let env = env(&spawner);

    let mut sum = env.spawn("sum", async { 1 + 2 });
    assert_eq!(sum.join().await, Ok(3));

    let mut blocking = env.spawn_blocking("hash", || (0..1000u64).sum::<u64>());
    assert_eq!(blocking.join().await, Ok(499_500));

    let mut forever = env.spawn("forever", futures::future::pending::<()>());
    assert_eq!(spawner.task_names(), ["forever"]);
    forever.abort();
    assert_eq!(forever.join().await, Err(Cancelled));

    let mut panics = env.spawn("panics", async {
        panic!("task panic, expected in this test")
    });
    assert_eq!(panics.join().await, Err(Cancelled));
    assert_eq!(spawner.close(1000 * MS).await, Vec::<&str>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn close_lets_tasks_finish_then_aborts_the_rest() {
    let spawner = spawner();
    let env = env(&spawner);
    let signal = env.shutdown();
    assert!(!signal.is_shutdown());

    let mut graceful = env.spawn("graceful", {
        let (signal, clock) = (signal.clone(), env.clock.clone());
        async move {
            signal.await;
            clock.sleep(20 * MS).await;
            "flushed"
        }
    });
    let mut stuck = env.spawn("stuck", futures::future::pending::<()>());
    let mut names = spawner.task_names();
    names.sort_unstable();
    assert_eq!(names, ["graceful", "stuck"]);

    let aborted = spawner.close(300 * MS).await;
    assert_eq!(aborted, ["stuck"]);
    assert!(signal.is_shutdown());
    assert_eq!(graceful.join().await, Ok("flushed"));
    assert_eq!(stuck.join().await, Err(Cancelled));
    assert!(spawner.task_names().is_empty());

    // The scope is closed: nothing new runs.
    let mut late = env.spawn("late", async { 1 });
    assert_eq!(late.join().await, Err(Cancelled));
    let mut late_blocking = env.spawn_blocking("late-blocking", || 1);
    assert_eq!(late_blocking.join().await, Err(Cancelled));
}

#[tokio::test(flavor = "multi_thread")]
async fn close_returns_as_soon_as_the_scope_is_idle() {
    let spawner = spawner();
    let env = env(&spawner);
    let start = env.clock.now();
    for _ in 0..50 {
        let signal = spawner.shutdown();
        drop(env.spawn("worker", signal));
    }
    assert_eq!(spawner.close(30_000 * MS).await, Vec::<&str>::new());
    assert!(env.clock.now().saturating_duration_since(start) < 10_000 * MS);
}

#[tokio::test]
async fn ids_from_the_production_env_increase() {
    let spawner = spawner();
    let env = env(&spawner);
    let mut previous = env.new_id();
    for _ in 0..10_000 {
        let id = env.new_id();
        assert!(id > previous);
        previous = id;
    }
    assert_ne!(
        os_seeded_rng().unwrap().fork("a").next_u64(),
        os_seeded_rng().unwrap().fork("a").next_u64()
    );
}
