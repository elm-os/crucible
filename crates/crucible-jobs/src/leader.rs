//! The leader runner (contract 11.7): one per lease name. It takes the lease, renews it every 5 s, trusts it
//! only inside the local window, runs the jobs registered under it one at a time, and gives it up on
//! shutdown.

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crucible_control::{
    ControlStore, ControlTx, HeldLease, LEASE_RENEW_EVERY, LEASE_SAFETY_MARGIN, LEASE_TTL, Lease,
    LeaseName, TxError,
};
use crucible_env::{Clock, ClockExt, Env, Instant, Rng, ShutdownSignal};
use futures::future::{BoxFuture, Fuse};
use futures::{FutureExt, pin_mut, select_biased};

use crate::cadence::{Cadence, uniform};
use crate::job::{JobError, Run};

/// The pause between attempts to take a lease another node holds; a jitter of up to 1 s is added.
pub const ACQUIRE_EVERY: Duration = Duration::from_secs(5);
/// The pause before a renewal is tried again after it failed without an answer.
pub const RENEW_RETRY: Duration = Duration::from_secs(1);
/// How long a shutdown waits for the run in flight to end before it cuts it.
pub const DRAIN_LIMIT: Duration = Duration::from_secs(5);
/// How long a shutdown waits for the release statement.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// The lease as this node holds it. Shared by the renewal loop, which extends the local window, and the
/// running job, which reads it before every leader action.
pub(crate) struct Leadership {
    clock: Arc<dyn Clock>,
    held: Mutex<Option<HeldLease>>,
}

impl Leadership {
    fn new(clock: Arc<dyn Clock>, held: HeldLease) -> Self {
        Self {
            clock,
            held: Mutex::new(Some(held)),
        }
    }

    fn held(&self) -> Option<HeldLease> {
        self.held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The lease, while its local window is open.
    fn current(&self) -> Option<Lease> {
        let held = self.held()?;
        held.is_valid_at(self.clock.now().nanos())
            .then_some(held.lease)
    }

    /// The end of the local window, while the lease has not been given up.
    fn valid_until(&self) -> Option<Instant> {
        Some(Instant::from_nanos(self.held()?.valid_until_nanos()))
    }

    fn set(&self, held: HeldLease) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(held);
    }

    /// Gives the lease up locally: nothing acts under it any more.
    fn lose(&self) {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl fmt::Debug for Leadership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Leadership")
            .field("lease", &self.current())
            .finish()
    }
}

/// What a leader job runs with. Reads go to [`LeaderCtx::store`]; every write to Postgres goes through
/// [`LeaderCtx::fenced`] or [`LeaderCtx::set_cursor`], and every action on ClickHouse through
/// [`LeaderCtx::issue`], so that a node that has lost the lease changes nothing.
#[derive(Clone, Debug)]
pub struct LeaderCtx {
    /// The node's environment.
    pub env: Env,
    store: Arc<dyn ControlStore>,
    lead: Arc<Leadership>,
}

impl LeaderCtx {
    /// The control store, for reads. A write made through it directly is not fenced.
    pub fn store(&self) -> &dyn ControlStore {
        self.store.as_ref()
    }

    /// The lease, if this node may still act under it: the local window (the acquire or renew time plus the
    /// TTL minus 2 s) is open and no fenced write has been refused.
    pub fn lease(&self) -> Result<Lease, JobError> {
        self.lead.current().ok_or(JobError::LeaseLost)
    }

    fn checked<T>(&self, result: Result<T, TxError>) -> Result<T, JobError> {
        result.map_err(|error| {
            if error == TxError::Fenced {
                self.lead.lose();
                JobError::LeaseLost
            } else {
                JobError::Store(error)
            }
        })
    }

    /// A fenced leader write: runs `f` in one transaction that first locks the lease row and checks the
    /// fencing token, and commits when `f` returns `Ok`. Nothing is retried here; a job that fails runs again
    /// at its next cadence. Keep the transaction to a few statements: the renewal waits for its lock on the
    /// lease row.
    pub async fn fenced<T, F>(&self, f: F) -> Result<T, JobError>
    where
        T: Send,
        F: for<'a> FnOnce(&'a dyn ControlTx) -> BoxFuture<'a, Result<T, TxError>> + Send,
    {
        let lease = self.lease()?;
        let result = async {
            let tx = self.store.begin().await?;
            let ran = match tx.fence(&lease).await {
                Ok(()) => f(tx.as_ref()).await,
                Err(error) => Err(error),
            };
            match ran {
                Ok(value) => {
                    tx.commit().await?;
                    Ok(value)
                }
                Err(error) => {
                    tx.rollback().await;
                    Err(error)
                }
            }
        }
        .await;
        self.checked(result)
    }

    /// The job cursor stored under the lease name. Jobs that share a lease name share the cursor.
    pub async fn cursor(&self) -> Result<Option<String>, JobError> {
        let name = self.lease()?.name;
        self.checked(self.store.leases().cursor(name).await)
    }

    /// Stores the job cursor in one fenced statement.
    pub async fn set_cursor(&self, cursor: &str) -> Result<(), JobError> {
        let lease = self.lease()?;
        self.checked(self.store.leases().set_cursor(&lease, cursor).await)
    }

    /// A leader action on ClickHouse (contract 11.7): `record` writes what is about to be done in a fenced
    /// transaction, and only when that has committed and the local window is still open is `action` started.
    /// The action must be idempotent: the record can commit and the node stop before the action runs, so
    /// the next leader finds the record and issues the action again.
    pub async fn issue<T, R, A, Fut>(&self, record: R, action: A) -> Result<T, JobError>
    where
        R: for<'a> FnOnce(&'a dyn ControlTx) -> BoxFuture<'a, Result<(), TxError>> + Send,
        A: FnOnce(&Lease) -> Fut,
        Fut: Future<Output = T>,
    {
        self.fenced(record).await?;
        // No await point between this check and the start of the action.
        let lease = self.lease()?;
        Ok(action(&lease).await)
    }
}

pub(crate) struct LeaderJob {
    pub(crate) name: &'static str,
    pub(crate) cadence: Cadence,
    pub(crate) run: Run<LeaderCtx>,
}

/// The runner of one lease name.
pub(crate) struct LeaderRunner {
    pub(crate) env: Env,
    pub(crate) store: Arc<dyn ControlStore>,
    pub(crate) name: LeaseName,
    pub(crate) holder: String,
    pub(crate) jobs: Vec<LeaderJob>,
    /// The seeded defect of the feature `test-mutants`: the local window is the whole TTL.
    pub(crate) skip_margin: bool,
}

/// Logs a failure that keeps repeating as one warning, at its `warn_at`-th occurrence in a row, and at debug
/// level otherwise: a store that is still starting, migrating or failing over does not fill the log.
struct Quiet {
    warn_at: u32,
    failures: u32,
}

impl Quiet {
    const fn new(warn_at: u32) -> Self {
        Self {
            warn_at,
            failures: 0,
        }
    }

    fn failed(&mut self, lease: LeaseName, what: &str, error: &TxError) {
        self.failures = self.failures.saturating_add(1);
        if self.failures == self.warn_at {
            tracing::warn!(
                %lease, %error, failures = self.failures,
                "{what} repeatedly and is tried again every few seconds. If the error names a missing \
                 relation, apply the migrations: `crucible migrate`, or `crucible serve --migrate`"
            );
        } else {
            tracing::debug!(%lease, %error, "{what}");
        }
    }

    fn worked(&mut self) {
        self.failures = 0;
    }
}

impl LeaderRunner {
    /// The lease TTL as the local window sees it.
    fn window_ttl(&self) -> Duration {
        if self.skip_margin {
            LEASE_TTL.saturating_add(LEASE_SAFETY_MARGIN)
        } else {
            LEASE_TTL
        }
    }

    /// Runs until shutdown.
    pub(crate) async fn run(self) {
        let rng = self
            .env
            .rng
            .fork(&format!("crucible-jobs/leader/{}", self.name));
        let mut shutdown = self.env.shutdown().fuse();
        // About half a minute of failed attempts: longer than a first start takes to apply the migrations.
        let mut quiet = Quiet::new(6);
        loop {
            // The clock is read before the call, so the local window ends before the database's.
            let asked = self.env.clock.now();
            let acquire = self
                .store
                .leases()
                .acquire(self.name, &self.holder, LEASE_TTL)
                .fuse();
            pin_mut!(acquire);
            let attempt = select_biased! {
                () = shutdown => {
                    // The statement is on its way: if it takes the lease, the lease is given back.
                    let landed = self.env.timeout(RELEASE_TIMEOUT, acquire).await;
                    if let Ok(Ok(Some(lease))) = landed {
                        self.release(&lease).await;
                    }
                    return;
                }
                attempt = acquire => attempt,
            };
            match attempt {
                Ok(Some(lease)) => {
                    quiet.worked();
                    tracing::info!(lease = %self.name, token = lease.fencing_token, "lease acquired");
                    let held = HeldLease::new(lease, asked.nanos(), self.window_ttl());
                    if self.lead(held, asked, rng.as_ref(), &mut shutdown).await {
                        return;
                    }
                }
                Ok(None) => quiet.worked(),
                // An acquisition with an unknown outcome is never acted on: the lease, if it was taken,
                // expires by itself.
                Err(error) => quiet.failed(self.name, "taking the lease failed", &error),
            }
            let pause = ACQUIRE_EVERY.saturating_add(uniform(rng.as_ref(), Duration::from_secs(1)));
            select_biased! {
                () = shutdown => return,
                () = self.env.clock.sleep(pause).fuse() => {}
            }
        }
    }

    /// Leads until the lease is lost (`false`) or shutdown is requested (`true`).
    async fn lead(
        &self,
        held: HeldLease,
        asked: Instant,
        rng: &dyn Rng,
        mut shutdown: &mut Fuse<ShutdownSignal>,
    ) -> bool {
        let lead = Arc::new(Leadership::new(Arc::clone(&self.env.clock), held));
        let stopping = AtomicBool::new(false);
        let busy = AtomicBool::new(false);
        let renewing = self.renew(&lead, asked).fuse();
        let working = self.work(&lead, rng, &stopping, &busy).fuse();
        pin_mut!(renewing, working);
        // Validity comes first: when it lapses the job is not polled again, and returning drops it.
        let stop = select_biased! {
            () = shutdown => true,
            () = renewing => false,
            () = working => false,
        };
        if !stop {
            lead.lose();
            tracing::info!(lease = %self.name, "lease no longer held; work under it has stopped");
            return false;
        }
        // Shutdown (contract 11.7): no new run starts; the run in flight may finish while the lease stays
        // valid, and then the lease is released. A run that has to be cut may have an action in flight, so
        // its lease is left to expire instead.
        stopping.store(true, Ordering::SeqCst);
        let clean = if busy.load(Ordering::SeqCst) {
            select_biased! {
                () = working => true,
                () = renewing => false,
                () = self.env.clock.sleep(DRAIN_LIMIT).fuse() => false,
            }
        } else {
            true
        };
        let lease = lead.held().map(|held| held.lease);
        lead.lose();
        if let (true, Some(lease)) = (clean, lease) {
            self.release(&lease).await;
        }
        true
    }

    /// Gives the lease back, within [`RELEASE_TIMEOUT`]. A release that fails only delays the next leader by
    /// what is left of the TTL.
    async fn release(&self, lease: &Lease) {
        let release = self.store.leases().release(lease);
        match self.env.timeout(RELEASE_TIMEOUT, release).await {
            Ok(Ok(_)) => tracing::info!(lease = %self.name, "lease released"),
            Ok(Err(error)) => {
                tracing::debug!(lease = %self.name, %error, "releasing the lease failed; it expires");
            }
            Err(_) => {
                tracing::debug!(lease = %self.name, "releasing the lease timed out; it expires");
            }
        }
    }

    /// Renews every 5 s. Returns when the lease is lost: the database says so, or the local window ends
    /// before a renewal got through.
    async fn renew(&self, lead: &Leadership, asked: Instant) {
        let clock = &self.env.clock;
        let mut next = asked.saturating_add(LEASE_RENEW_EVERY);
        let mut quiet = Quiet::new(3);
        loop {
            let Some(until) = lead.valid_until() else {
                return;
            };
            if clock
                .timeout_at(until, clock.sleep_until(next))
                .await
                .is_err()
            {
                return;
            }
            let Some(lease) = lead.current() else {
                return;
            };
            let asked = clock.now();
            let renewal = self.store.leases().renew(&lease, LEASE_TTL);
            match clock.timeout_at(until, renewal).await {
                // The window ended with the renewal in flight. Whatever its outcome, nothing acts on it.
                Err(_) => return,
                Ok(Ok(Some(renewed))) => {
                    quiet.worked();
                    // A fenced write may have given the lease up meanwhile; that stays.
                    if lead.held().is_some() {
                        lead.set(HeldLease::new(renewed, asked.nanos(), self.window_ttl()));
                    }
                    next = asked.saturating_add(LEASE_RENEW_EVERY);
                }
                Ok(Ok(None)) => return,
                // Nothing was applied, or the outcome is unknown: the old window still bounds this node.
                Ok(Err(error)) => {
                    quiet.failed(self.name, "renewing the lease failed", &error);
                    next = clock.now().saturating_add(RENEW_RETRY);
                }
            }
        }
    }

    /// Runs the jobs of this lease, one at a time, each when it is due. Returns when the lease is no longer
    /// valid, or between two runs once `stopping` is set.
    async fn work(
        &self,
        lead: &Arc<Leadership>,
        rng: &dyn Rng,
        stopping: &AtomicBool,
        busy: &AtomicBool,
    ) {
        let clock = &self.env.clock;
        // A new leader runs every job once, then each on its cadence.
        let mut due = vec![clock.now(); self.jobs.len()];
        loop {
            let Some((job, at)) = self.jobs.iter().zip(&mut due).min_by_key(|(_, at)| **at) else {
                return std::future::pending().await;
            };
            clock.sleep_until(*at).await;
            if stopping.load(Ordering::SeqCst) || lead.current().is_none() {
                return;
            }
            let ctx = LeaderCtx {
                env: self.env.clone(),
                store: Arc::clone(&self.store),
                lead: Arc::clone(lead),
            };
            busy.store(true, Ordering::SeqCst);
            let result = (job.run)(ctx).await;
            busy.store(false, Ordering::SeqCst);
            match result {
                Ok(()) => {}
                Err(JobError::LeaseLost) => {
                    tracing::debug!(
                        job = job.name,
                        "the run stopped: the lease is no longer held"
                    );
                }
                Err(error) => {
                    tracing::warn!(job = job.name, %error, "the run failed; the job runs again at its next cadence");
                }
            }
            *at = clock.now().saturating_add(job.cadence.next_delay(rng));
            if stopping.load(Ordering::SeqCst) || lead.current().is_none() {
                return;
            }
        }
    }
}
