//! What a job declares: its kind, its cadence and its run function.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crucible_control::{Claim, ControlStore, LeaseName, Queue, TxError};
use crucible_env::Env;
use futures::future::BoxFuture;

use crate::cadence::Cadence;
use crate::leader::LeaderCtx;

/// Why a run of a job ended without finishing its work. The runner logs it; a leader job runs again at its
/// next cadence, a claimed row is retried with a backoff.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum JobError {
    /// The lease is no longer valid for this node: its local window ended, or a fenced write found another
    /// fencing token. The job must stop; the runner does not count this as a failure of the job.
    #[error("the lease is no longer held")]
    LeaseLost,
    /// A control-store call failed.
    #[error(transparent)]
    Store(#[from] TxError),
    /// The job's own work failed.
    #[error("{0}")]
    Failed(String),
}

/// What a run returns.
pub type JobResult = Result<(), JobError>;

/// What a per-node job runs with.
#[derive(Clone, Debug)]
pub struct NodeCtx {
    /// The node's environment.
    pub env: Env,
    /// The control store, when Postgres is configured.
    pub control: Option<Arc<dyn ControlStore>>,
}

/// What a claimed-work handler runs with: one claimed row. The handler must be idempotent, because a row is
/// handled at least once: its claim can run out, or the node can stop, after the effect and before the
/// completion is recorded.
#[derive(Clone, Debug)]
pub struct ClaimCtx {
    /// The node's environment.
    pub env: Env,
    /// The control store.
    pub store: Arc<dyn ControlStore>,
    /// The claimed row. `claim.attempt` counts this attempt; the first is 1.
    pub claim: Claim,
}

/// How a queue's rows are claimed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClaimPolicy {
    /// How long a claim hides its row from other claimers. The handler is stopped 2 s before that.
    pub lease_for: Duration,
    /// The poison cap: a row whose attempt counter reaches this without success is parked.
    pub max_attempts: u32,
}

impl Default for ClaimPolicy {
    /// A 60 s claim and 12 attempts, which the backoff spreads over about three and a half hours.
    fn default() -> Self {
        Self {
            lease_for: Duration::from_secs(60),
            max_attempts: 12,
        }
    }
}

pub(crate) type Run<C> = Arc<dyn Fn(C) -> BoxFuture<'static, JobResult> + Send + Sync>;

fn boxed<C, F, Fut>(run: F) -> Run<C>
where
    F: Fn(C) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = JobResult> + Send + 'static,
{
    Arc::new(move |ctx| Box::pin(run(ctx)))
}

pub(crate) enum Kind {
    PerNode(Run<NodeCtx>),
    Leader(LeaseName, Run<LeaderCtx>),
    Claimed(Queue, ClaimPolicy, Run<ClaimCtx>),
}

/// One background job: a name for the logs and the task, a cadence, and what it runs as.
pub struct Job {
    pub(crate) name: &'static str,
    pub(crate) cadence: Cadence,
    pub(crate) kind: Kind,
}

impl Job {
    /// A timer on every node. It runs with or without Postgres. `run` is called at the start and then once
    /// per cadence.
    pub fn per_node<F, Fut>(name: &'static str, cadence: Cadence, run: F) -> Self
    where
        F: Fn(NodeCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = JobResult> + Send + 'static,
    {
        Self {
            name,
            cadence,
            kind: Kind::PerNode(boxed(run)),
        }
    }

    /// A job that only the holder of the lease `lease` runs (contract 11.7). `run` is called when the node
    /// takes the lease and then once per cadence while it holds it; it is dropped at an await point as soon
    /// as the lease is no longer valid locally. Jobs under one lease name run one at a time.
    pub fn leader<F, Fut>(name: &'static str, lease: LeaseName, cadence: Cadence, run: F) -> Self
    where
        F: Fn(LeaderCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = JobResult> + Send + 'static,
    {
        Self {
            name,
            cadence,
            kind: Kind::Leader(lease, boxed(run)),
        }
    }

    /// A consumer of a claimed-work queue (contract 11.7). Once per cadence the node claims the due rows of
    /// `queue` one at a time and calls `run` for each. `run` must be idempotent.
    pub fn claimed<F, Fut>(
        name: &'static str,
        queue: Queue,
        cadence: Cadence,
        policy: ClaimPolicy,
        run: F,
    ) -> Self
    where
        F: Fn(ClaimCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = JobResult> + Send + 'static,
    {
        Self {
            name,
            cadence,
            kind: Kind::Claimed(queue, policy, boxed(run)),
        }
    }

    /// The job's name.
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.kind {
            Kind::PerNode(_) => "per-node timer".to_owned(),
            Kind::Leader(lease, _) => format!("leader under the {lease} lease"),
            Kind::Claimed(queue, policy, _) => format!("consumer of {queue} ({policy:?})"),
        };
        f.debug_struct("Job")
            .field("name", &self.name)
            .field("cadence", &self.cadence)
            .field("kind", &kind)
            .finish()
    }
}
