//! The registry: the jobs of this node, and the tasks that run them.

use std::collections::BTreeMap;
use std::sync::Arc;

use crucible_control::{ControlStore, LeaseName};
use crucible_env::{Env, JoinHandle};
use futures::{FutureExt, select_biased};

use crate::cadence::Cadence;
use crate::claimed::ClaimRunner;
use crate::job::{Job, Kind, NodeCtx, Run};
use crate::leader::{LeaderJob, LeaderRunner};

/// The jobs of a node. Each job is registered once, before [`Registry::start`].
#[derive(Debug, Default)]
pub struct Registry {
    jobs: Vec<Job>,
    skip_margin: bool,
}

/// The task name of a lease's runner.
const fn leader_task(name: LeaseName) -> &'static str {
    match name {
        LeaseName::Migrations => "jobs-lease-migrations",
        LeaseName::Lifecycle => "jobs-lease-lifecycle",
        LeaseName::SqlIdentities => "jobs-lease-sql-identities",
        LeaseName::MediaGc => "jobs-lease-media-gc",
        LeaseName::SigningKeys => "jobs-lease-signing-keys",
        LeaseName::Housekeeping => "jobs-lease-housekeeping",
    }
}

/// A per-node timer: a run at the start, then one per cadence, until shutdown.
async fn per_node(name: &'static str, cadence: Cadence, run: Run<NodeCtx>, ctx: NodeCtx) {
    let rng = ctx.env.rng.fork(&format!("crucible-jobs/node/{name}"));
    let mut shutdown = ctx.env.shutdown().fuse();
    loop {
        let result = select_biased! {
            () = shutdown => return,
            result = run(ctx.clone()).fuse() => result,
        };
        if let Err(error) = result {
            tracing::warn!(job = name, %error, "the run failed; the job runs again at its next cadence");
        }
        let pause = cadence.next_delay(rng.as_ref());
        select_biased! {
            () = shutdown => return,
            () = ctx.env.clock.sleep(pause).fuse() => {}
        }
    }
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a job.
    pub fn register(&mut self, job: Job) -> &mut Self {
        self.jobs.push(job);
        self
    }

    /// The names of the registered jobs, in registration order.
    pub fn names(&self) -> Vec<&'static str> {
        self.jobs.iter().map(Job::name).collect()
    }

    /// The seeded defect "skip the local validity margin": leaders trust their lease for the whole TTL.
    #[cfg(feature = "test-mutants")]
    #[must_use]
    pub fn skip_validity_margin(mut self) -> Self {
        self.skip_margin = true;
        self
    }

    /// Starts the jobs in the node's shutdown scope: one task per per-node job, one per lease name and one
    /// per queue consumer. `holder` names this process in the lease rows; it must be unique per process
    /// start, so that a restarted node never takes its earlier lease for its own.
    ///
    /// Without a control store only the per-node jobs start, and that is logged once.
    pub fn start(self, env: &Env, control: Option<Arc<dyn ControlStore>>, holder: &str) -> Running {
        let mut tasks = Vec::new();
        let mut leaders: BTreeMap<LeaseName, Vec<LeaderJob>> = BTreeMap::new();
        let mut skipped = Vec::new();
        for job in self.jobs {
            let Job {
                name,
                cadence,
                kind,
            } = job;
            match (kind, &control) {
                (Kind::PerNode(run), _) => {
                    let ctx = NodeCtx {
                        env: env.clone(),
                        control: control.clone(),
                    };
                    tasks.push(env.spawn(name, per_node(name, cadence, run, ctx)));
                }
                (Kind::Leader(lease, run), Some(_)) => {
                    leaders
                        .entry(lease)
                        .or_default()
                        .push(LeaderJob { name, cadence, run });
                }
                (Kind::Claimed(queue, policy, run), Some(store)) => {
                    let runner = ClaimRunner {
                        env: env.clone(),
                        store: Arc::clone(store),
                        name,
                        queue,
                        cadence,
                        policy,
                        run,
                    };
                    tasks.push(env.spawn(name, runner.run()));
                }
                (Kind::Leader(..) | Kind::Claimed(..), None) => skipped.push(name),
            }
        }
        if let Some(store) = &control {
            for (name, jobs) in leaders {
                let runner = LeaderRunner {
                    env: env.clone(),
                    store: Arc::clone(store),
                    name,
                    holder: holder.to_owned(),
                    jobs,
                    skip_margin: self.skip_margin,
                };
                tasks.push(env.spawn(leader_task(name), runner.run()));
            }
        } else if !skipped.is_empty() {
            tracing::info!(
                jobs = ?skipped,
                "Postgres is not configured, so leader and claimed-work jobs do not run; set postgres.url \
                 (CRUCIBLE_POSTGRES_URL) to run them"
            );
        }
        Running { tasks }
    }
}

/// The running jobs. They stop when the node's shutdown scope is asked to stop; leaders release their
/// leases first.
#[derive(Debug)]
pub struct Running {
    tasks: Vec<JoinHandle<()>>,
}

impl Running {
    /// How many tasks were started.
    pub fn tasks(&self) -> usize {
        self.tasks.len()
    }

    /// Waits until every task has ended.
    pub async fn join(&mut self) {
        for task in &mut self.tasks {
            // A cancelled task has ended too.
            let _ended = task.join().await;
        }
    }
}
