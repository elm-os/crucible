//! Tasks on the tokio multi-thread runtime, tracked in one shutdown scope.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crucible_env::{ShutdownSignal, ShutdownTrigger, Spawner, TaskAbort, shutdown_channel};
use futures::future::BoxFuture;
use tokio::task::AbortHandle;

#[derive(Debug, Default)]
struct ScopeState {
    next_id: u64,
    closed: bool,
    /// The handle is `None` for the instant between registration and the runtime returning it.
    tasks: BTreeMap<u64, (&'static str, Option<AbortHandle>)>,
}

#[derive(Debug, Default)]
struct Scope {
    state: Mutex<ScopeState>,
    idle: tokio::sync::Notify,
}

impl Scope {
    fn lock(&self) -> MutexGuard<'_, ScopeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Lives inside the task; removes it from the scope when the task finishes, is aborted or panics.
struct Registration {
    scope: Arc<Scope>,
    id: u64,
    name: &'static str,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut state = self.scope.lock();
        state.tasks.remove(&self.id);
        let idle = state.tasks.is_empty();
        drop(state);
        tracing::trace!(task = self.name, "task ended");
        if idle {
            self.scope.idle.notify_waiters();
        }
    }
}

/// The production [`Spawner`]: named tasks on a tokio runtime inside one shutdown scope.
///
/// Task names are kept in the scope (see [`TokioSpawner::task_names`]) and attached to trace events; tokio's
/// own task naming is an unstable feature and is not used. Shutdown is two steps, both driven by `main`:
/// [`TokioSpawner::request_shutdown`] resolves every [`ShutdownSignal`], and [`TokioSpawner::close`] waits
/// for tasks to finish and aborts the stragglers.
#[derive(Debug)]
pub struct TokioSpawner {
    runtime: tokio::runtime::Handle,
    scope: Arc<Scope>,
    trigger: ShutdownTrigger,
    signal: ShutdownSignal,
}

#[derive(Debug)]
struct TokioAbort(Option<AbortHandle>);

impl TaskAbort for TokioAbort {
    fn abort(&self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

impl TokioSpawner {
    /// Creates an open scope on `runtime`.
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        let (trigger, signal) = shutdown_channel();
        Self {
            runtime,
            scope: Arc::default(),
            trigger,
            signal,
        }
    }

    /// Asks the scope to stop: every [`ShutdownSignal`] resolves. Tasks keep running until they return or
    /// [`TokioSpawner::close`] aborts them.
    pub fn request_shutdown(&self) {
        self.trigger.trigger();
    }

    /// The names of the tasks that are still running, in spawn order.
    pub fn task_names(&self) -> Vec<&'static str> {
        self.scope
            .lock()
            .tasks
            .values()
            .map(|(name, _)| *name)
            .collect()
    }

    /// Requests shutdown, waits up to `grace` for every task to finish, then closes the scope: tasks still
    /// running are aborted (their names are returned) and later spawns are dropped unrun, so their handles
    /// report `Cancelled`. Must not be called from a task of this scope.
    pub async fn close(&self, grace: Duration) -> Vec<&'static str> {
        self.request_shutdown();
        let drained = async {
            loop {
                let notified = self.scope.idle.notified();
                tokio::pin!(notified);
                // Register before checking, so a task that ends in between is not missed.
                notified.as_mut().enable();
                if self.scope.lock().tasks.is_empty() {
                    return;
                }
                notified.await;
            }
        };
        let _timed_out = tokio::time::timeout(grace, drained).await;

        let leftover = {
            let mut state = self.scope.lock();
            state.closed = true;
            std::mem::take(&mut state.tasks)
        };
        leftover
            .into_values()
            .map(|(name, handle)| {
                tracing::warn!(
                    task = name,
                    "task did not stop within the grace period; aborting"
                );
                if let Some(handle) = handle {
                    handle.abort();
                }
                name
            })
            .collect()
    }

    /// Registers a task and starts it with `start`, which receives the registration to move into the task.
    fn track(
        &self,
        name: &'static str,
        start: impl FnOnce(Registration) -> AbortHandle,
    ) -> Box<dyn TaskAbort> {
        let id = {
            let mut state = self.scope.lock();
            if state.closed {
                return Box::new(TokioAbort(None));
            }
            state.next_id += 1;
            let id = state.next_id;
            state.tasks.insert(id, (name, None));
            id
        };
        // The lock is not held here: a runtime that is shutting down drops the task, and with it the
        // registration, inside the spawn call.
        let handle = start(Registration {
            scope: Arc::clone(&self.scope),
            id,
            name,
        });
        {
            let mut state = self.scope.lock();
            let closed = state.closed;
            match state.tasks.get_mut(&id) {
                Some(entry) => entry.1 = Some(handle.clone()),
                // Closed in between: the scope could not abort a task it had no handle for.
                None if closed => handle.abort(),
                // Already finished.
                None => {}
            }
        }
        tracing::trace!(task = name, "task spawned");
        Box::new(TokioAbort(Some(handle)))
    }
}

impl Spawner for TokioSpawner {
    fn spawn_boxed(&self, name: &'static str, fut: BoxFuture<'static, ()>) -> Box<dyn TaskAbort> {
        self.track(name, |registration| {
            self.runtime
                .spawn(async move {
                    let _registration = registration;
                    fut.await;
                })
                .abort_handle()
        })
    }

    fn spawn_blocking_boxed(
        &self,
        name: &'static str,
        f: Box<dyn FnOnce() + Send>,
    ) -> Box<dyn TaskAbort> {
        self.track(name, |registration| {
            self.runtime
                .spawn_blocking(move || {
                    let _registration = registration;
                    f();
                })
                .abort_handle()
        })
    }

    fn shutdown(&self) -> ShutdownSignal {
        self.signal.clone()
    }
}
