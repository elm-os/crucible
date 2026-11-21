//! The shipper's blocking jobs under the CPU budget (contract 2.14): stage A of the frames read ahead of stage B,
//! and the encoding of the blocks of a sealed batch, each stage under its own share of `cpu.permits`.
//!
//! A job takes a permit of its stage before it is spawned. One that finds none waits without a thread and runs
//! on the thread of the job that passes its permit on, first come, first served. Each job's wait (from the offer
//! to its start) and hold (its run) are recorded by stage. While ingest takes every permit of its own share, its
//! requests wait for the CPUs, and the work runs in place on the shipper's task instead, as it did before it moved
//! off the task: a burst of ingest keeps the CPUs it had. The simulation's spawner runs a blocking job in place,
//! so there every job has ended when its offer returns, at the point of the shipper's run where the work was done
//! before it moved off the task.

use std::sync::Arc;
use std::time::Duration;

use crucible_admission::{Job, Offer, PermitPool, Waiting};
use crucible_env::metrics::registry;
use crucible_env::{Cancelled, Env, Histogram, Metrics};
use futures::channel::oneshot;

/// The `stage` label of stage A's jobs in the CPU budget's metrics.
pub(crate) const STAGE_A: &str = "stage_a";

/// The `stage` label of the encoding jobs.
pub(crate) const ENCODE: &str = "encode";

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// One stage's share of the CPU budget, with the histograms of its jobs' waits and holds.
#[derive(Debug)]
pub(crate) struct CpuStage {
    /// The name of its jobs' blocking tasks.
    task: &'static str,
    permits: Arc<PermitPool>,
    /// Ingest's share, when the node has one.
    ingest: Option<Arc<PermitPool>>,
    wait: Histogram,
    hold: Histogram,
}

impl CpuStage {
    /// The stage labelled `stage`, with `permits` permits (at least 1), whose jobs run as blocking tasks named
    /// `task` while `ingest`, ingest's share, has a free permit.
    pub(crate) fn new(
        metrics: &dyn Metrics,
        stage: &'static str,
        task: &'static str,
        permits: u32,
        ingest: Option<Arc<PermitPool>>,
    ) -> Self {
        Self {
            task,
            permits: Arc::new(PermitPool::new(permits.max(1))),
            ingest,
            wait: metrics.histogram(&registry::CPU_PERMIT_WAIT, &[stage]),
            hold: metrics.histogram(&registry::CPU_PERMIT_HOLD, &[stage]),
        }
    }

    /// Runs `work` as a blocking job under a permit of the stage and returns its result to come; or, while
    /// ingest takes every permit of its share, runs it here.
    pub(crate) fn run<T, F>(&self, env: &Env, work: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (sender, result) = oneshot::channel();
        if self
            .ingest
            .as_ref()
            .is_some_and(|pool| pool.available() == 0)
        {
            // The receiver is held below.
            let _held = sender.send(work());
            return Pending {
                result,
                _waiting: None,
            };
        }
        let clock = Arc::clone(&env.clock);
        let offered = clock.now();
        let (wait, hold) = (self.wait.clone(), self.hold.clone());
        let job: Job = Box::new(move || {
            let started = clock.now();
            let value = work();
            let ended = clock.now();
            wait.record(micros(started.saturating_duration_since(offered)));
            hold.record(micros(ended.saturating_duration_since(started)));
            // The shipper may have dropped the result meanwhile, at a restart of its read or at shutdown.
            let _unwanted = sender.send(value);
        });
        let waiting = match self.permits.offer(job) {
            Offer::Run(permit) => {
                // The result comes back on its own channel, so the task's handle is not needed.
                drop(env.spawn_blocking(self.task, move || permit.run()));
                None
            }
            Offer::Queued(waiting) => Some(waiting),
        };
        Pending {
            result,
            _waiting: waiting,
        }
    }
}

/// The result of a job, once it has ended. Dropping it withdraws a job that still waits for its permit.
#[derive(Debug)]
pub(crate) struct Pending<T> {
    result: oneshot::Receiver<T>,
    _waiting: Option<Waiting>,
}

impl<T> Pending<T> {
    /// The job's result if it has ended, or `Err(Cancelled)` when it ended without one: it panicked, or the
    /// runtime dropped it while shutting down. `None` while it runs or waits; call it no more once it answered.
    pub(crate) fn try_take(&mut self) -> Option<Result<T, Cancelled>> {
        match self.result.try_recv() {
            Ok(Some(value)) => Some(Ok(value)),
            Ok(None) => None,
            Err(oneshot::Canceled) => Some(Err(Cancelled)),
        }
    }

    /// Waits for the job's result, as [`Pending::try_take`] gives it. Cancel-safe.
    pub(crate) async fn take(&mut self) -> Result<T, Cancelled> {
        (&mut self.result)
            .await
            .map_err(|oneshot::Canceled| Cancelled)
    }
}
