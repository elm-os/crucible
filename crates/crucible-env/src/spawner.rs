//! Tasks and the shutdown scope (contract 2.3).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::{BoxFuture, Shared};

/// Spawns tasks inside the host's shutdown scope. When the scope shuts down (or, in simulation, the host
/// crashes) every task still running is dropped at an await point.
///
/// Product code calls the typed wrappers [`crate::Env::spawn`] and [`crate::Env::spawn_blocking`]; the boxed
/// methods exist to keep the trait object-safe.
pub trait Spawner: fmt::Debug + Send + Sync {
    /// Spawns a named task and returns a handle that can abort it.
    fn spawn_boxed(&self, name: &'static str, fut: BoxFuture<'static, ()>) -> Box<dyn TaskAbort>;

    /// Runs a named blocking or CPU-bound closure off the async threads. Simulation runs it inline.
    fn spawn_blocking_boxed(
        &self,
        name: &'static str,
        f: Box<dyn FnOnce() + Send>,
    ) -> Box<dyn TaskAbort>;

    /// A signal that resolves when the scope is asked to stop.
    fn shutdown(&self) -> ShutdownSignal;
}

/// Aborts one spawned task.
pub trait TaskAbort: fmt::Debug + Send + Sync {
    /// Drops the task at its next await point. A blocking closure that has already started runs to the end.
    /// Aborting a finished task does nothing.
    fn abort(&self);
}

/// The task was aborted, its scope shut down, or it panicked, before it produced a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("task cancelled")]
pub struct Cancelled;

/// A handle to a spawned task. Dropping it detaches the task; it does not abort it.
#[derive(Debug)]
pub struct JoinHandle<T> {
    result: oneshot::Receiver<T>,
    abort: Box<dyn TaskAbort>,
}

impl<T> JoinHandle<T> {
    /// Waits for the task's value. Returns `Err(Cancelled)` after [`JoinHandle::abort`] or scope shutdown
    /// dropped the task (a panic in the task surfaces the same way). Cancel-safe; once it has returned, later
    /// calls return `Err(Cancelled)`.
    pub async fn join(&mut self) -> Result<T, Cancelled> {
        (&mut self.result)
            .await
            .map_err(|oneshot::Canceled| Cancelled)
    }

    /// Asks for the task to be dropped at its next await point.
    pub fn abort(&self) {
        self.abort.abort();
    }
}

pub(crate) fn spawn<T, F>(spawner: &dyn Spawner, name: &'static str, fut: F) -> JoinHandle<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, result) = oneshot::channel();
    let abort = spawner.spawn_boxed(
        name,
        Box::pin(async move {
            // The receiver is gone when the handle was dropped; the value is then discarded.
            let _detached = tx.send(fut.await);
        }),
    );
    JoinHandle { result, abort }
}

pub(crate) fn spawn_blocking<T, F>(spawner: &dyn Spawner, name: &'static str, f: F) -> JoinHandle<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, result) = oneshot::channel();
    let abort = spawner.spawn_blocking_boxed(
        name,
        Box::new(move || {
            let _detached = tx.send(f());
        }),
    );
    JoinHandle { result, abort }
}

/// Creates a shutdown scope's trigger and its signal. Spawner implementations own the trigger.
pub fn shutdown_channel() -> (ShutdownTrigger, ShutdownSignal) {
    let (tx, rx) = oneshot::channel::<()>();
    let fut: BoxFuture<'static, ()> = Box::pin(rx.map(|_| ()));
    (
        ShutdownTrigger {
            tx: std::sync::Mutex::new(Some(tx)),
        },
        ShutdownSignal {
            inner: fut.shared(),
        },
    )
}

/// Asks a shutdown scope to stop. Dropping the trigger also resolves the signal.
#[derive(Debug)]
pub struct ShutdownTrigger {
    tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl ShutdownTrigger {
    /// Resolves the signal for every holder. Idempotent.
    pub fn trigger(&self) {
        let tx = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(tx);
    }
}

/// A cloneable future that resolves when the scope is asked to stop, and stays resolved.
#[derive(Clone)]
pub struct ShutdownSignal {
    inner: Shared<BoxFuture<'static, ()>>,
}

impl ShutdownSignal {
    /// Whether shutdown has been requested and observed through any handle to this signal, or is observable now.
    pub fn is_shutdown(&self) -> bool {
        self.inner.clone().now_or_never().is_some()
    }
}

impl fmt::Debug for ShutdownSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownSignal")
            .field("is_shutdown", &self.is_shutdown())
            .finish()
    }
}

impl Future for ShutdownSignal {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.inner.poll_unpin(cx)
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;
    use crate::testing::ManualSpawner;

    #[test]
    fn join_returns_the_task_value() {
        let spawner = ManualSpawner::new();
        let mut handle = spawn(&spawner, "adder", async { 1 + 2 });
        assert_eq!(spawner.names(), ["adder"]);
        assert!(handle.join().now_or_never().is_none());
        spawner.run_until_stalled();
        assert_eq!(block_on(handle.join()), Ok(3));
        assert_eq!(block_on(handle.join()), Err(Cancelled));
    }

    #[test]
    fn join_is_cancelled_after_abort() {
        let spawner = ManualSpawner::new();
        let mut handle = spawn(&spawner, "forever", futures::future::pending::<u8>());
        spawner.run_until_stalled();
        handle.abort();
        assert_eq!(block_on(handle.join()), Err(Cancelled));
        assert!(spawner.names().is_empty());
        handle.abort();
    }

    #[test]
    fn join_is_cancelled_after_scope_shutdown() {
        let spawner = ManualSpawner::new();
        let signal = spawner.shutdown();
        let mut graceful = spawn(&spawner, "graceful", {
            let signal = signal.clone();
            async move {
                signal.await;
                "stopped"
            }
        });
        let mut stuck = spawn(&spawner, "stuck", futures::future::pending::<u8>());
        spawner.run_until_stalled();
        assert!(!signal.is_shutdown());

        spawner.request_shutdown();
        assert!(signal.is_shutdown());
        spawner.run_until_stalled();
        spawner.abort_all();
        assert_eq!(block_on(graceful.join()), Ok("stopped"));
        assert_eq!(block_on(stuck.join()), Err(Cancelled));
    }

    #[test]
    fn dropping_the_handle_detaches_the_task() {
        let spawner = ManualSpawner::new();
        let (tx, rx) = oneshot::channel();
        drop(spawn(&spawner, "detached", async move {
            let _sent = tx.send(5);
        }));
        spawner.run_until_stalled();
        assert_eq!(block_on(rx), Ok(5));
    }

    #[test]
    fn spawn_blocking_returns_the_closure_value() {
        let spawner = ManualSpawner::new();
        let mut handle = spawn_blocking(&spawner, "hash", || 21 * 2);
        assert_eq!(block_on(handle.join()), Ok(42));
    }

    #[test]
    fn shutdown_signal_resolves_for_every_clone_and_when_the_trigger_is_dropped() {
        let (trigger, signal) = shutdown_channel();
        let other = signal.clone();
        assert!(!signal.is_shutdown());
        trigger.trigger();
        trigger.trigger();
        block_on(async {
            signal.clone().await;
            other.await;
        });
        assert!(signal.is_shutdown());

        let (trigger, signal) = shutdown_channel();
        drop(trigger);
        block_on(signal);
    }
}
