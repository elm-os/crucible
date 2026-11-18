//! The job runners of `crucible-jobs` over `PgModel` under faults (contract 11.7).
//!
//! Several nodes each run the real registry: a leader job under the `lifecycle` lease that writes fenced
//! cursors and issues actions, the real housekeeping job under the `housekeeping` lease, a consumer of the
//! notification outbox and a per-node timer. The harness reaches a node only through its `Env` and its store
//! handle:
//!
//! - **crashes**: every task of the node is aborted at its await point; the node starts again as a new
//!   process with a new holder name;
//! - **freezes**: the process stops for up to 25 s, longer than a lease lives. No sleep of the node wakes and
//!   no store call of the node starts or returns until the thaw;
//! - **clock skew**: each node's wall clock is up to 5 min off the database's;
//! - **connection loss**: the store fails over, loses commits with an unknown outcome, aborts transactions
//!   and delays commits, and a node's reserved pool (the lease statements) goes away for up to 20 s while its
//!   transactions still get through;
//! - **lease steals**: the lease row is rewritten for another holder while its holder's local window is open;
//! - **graceful restarts**: the shutdown path, with its release.
//!
//! A fault-free tail follows. The oracle:
//!
//! - **J1**: fenced leader writes never interleave between two holders. Every committed cursor carries the
//!   fencing token and the holder of the lease row at its commit, and a lease row's token never goes back. A
//!   leader action lands only while its lease row still carries its token and has not expired, unless the
//!   lease was stolen.
//! - **J2**: a claimed row's effect is applied at least once before the row is done, and its completion is
//!   recorded at most once, by the claim that holds the row.
//! - **J3**: after the faults heal every enqueued row is done, both leases are held and their jobs run; on
//!   shutdown both leases are released.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_control::{
    ApiKey, ApiKeyRepo, AuthMethod, Claim, Claims, ControlStore, ControlTx, CredentialRepo, Email,
    JsonDoc, KeyFeed, KeyScope, LEASE_TTL, Lease, LeaseName, Leases, MembershipRepo, Migrations,
    NewJob, OrgRepo, Project, ProjectRepo, Queue, Repos, SecretRepo, Session, SessionRepo,
    SigningKeyRepo, TokenHash, TxError, User, UserRepo, UserStatus,
};
use crucible_env::{Clock, ClockExt, Env, Instant, Rng};
use crucible_jobs::housekeeping::{self, KEY_FEED_KEEP};
use crucible_jobs::{
    Cadence, ClaimCtx, ClaimPolicy, Job, JobError, JobResult, LeaderCtx, NodeCtx, Registry,
};
use crucible_records::{ApiKeyId, ProjectId, Timestamp, UserId, Uuid};
use futures::future::BoxFuture;

use crate::pg_model::{JobStatus, LeaseEvent, PgFaults, PgModel, PgMutant};
use crate::{SIM_EPOCH_MICROS, Sim, run};

const QUEUE: Queue = Queue::NotificationOutbox;
const LEASES: [LeaseName; 2] = [LeaseName::Lifecycle, LeaseName::Housekeeping];
/// How long a claim of the simulated consumer lasts: shorter than the longest freeze.
const CLAIM_FOR: Duration = Duration::from_secs(12);
/// The longest a leader action takes to land: under the 2 s the local window ends early.
const LANDING_MAX: Duration = Duration::from_millis(1_500);

/// A seeded defect the oracle must catch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// The runner trusts its lease for the whole TTL instead of the TTL minus 2 s.
    SkipValidityMargin,
    /// The store renews without comparing holder and fencing token.
    RenewWithoutCompareAndSet,
    /// The store completes a row without comparing the claim.
    CompleteWithoutClaimCheck,
}

/// What one run does.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Nodes that run the registry.
    pub nodes: usize,
    /// How long the faulty phase lasts, in virtual time.
    pub faulty_for: Duration,
    /// How long the fault-free tail lasts.
    pub tail: Duration,
    /// Whether leases are stolen.
    pub steals: bool,
    /// The seeded defect, if any.
    pub mutant: Option<Mutant>,
}

impl Options {
    /// Every fault, lease steals included.
    pub const FAULTY: Self = Self {
        nodes: 3,
        faulty_for: Duration::from_secs(600),
        tail: Duration::from_secs(420),
        steals: true,
        mutant: None,
    };
    /// The same without lease steals: every lease ends by expiry or release only.
    pub const NO_STEALS: Self = Self {
        steals: false,
        ..Self::FAULTY
    };
}

/// What happened in a run, so that a test can require that the interesting cases occurred.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Times a node started.
    pub boots: u64,
    /// Crashes.
    pub crashes: u64,
    /// Graceful restarts.
    pub restarts: u64,
    /// Freezes.
    pub freezes: u64,
    /// Freezes longer than the lease TTL.
    pub long_freezes: u64,
    /// Outages of one node's reserved pool.
    pub reserved_outages: u64,
    /// Failovers the harness started (the fault stream starts more).
    pub failovers: u64,
    /// Leases stolen.
    pub steals: u64,
    /// Lease rows committed for a holder other than the previous one.
    pub takeovers: u64,
    /// Fenced cursor statements acknowledged.
    pub cursor_writes: u64,
    /// Fenced transactions acknowledged.
    pub transactions: u64,
    /// Leader actions issued.
    pub actions: u64,
    /// Leader actions that landed.
    pub landed: u64,
    /// Leader runs that stopped because the lease was lost: a fenced refusal or the end of the window.
    pub lease_lost: u64,
    /// Leader runs that failed with an unknown commit.
    pub commit_unknown: u64,
    /// Leader runs that failed in another way.
    pub failed_runs: u64,
    /// Rows enqueued.
    pub enqueued: u64,
    /// Handler runs.
    pub handled: u64,
    /// Handler runs of an attempt after the first.
    pub retries: u64,
    /// Handler runs that failed on purpose.
    pub handler_failures: u64,
    /// Completions acknowledged.
    pub completions: u64,
    /// Completions refused because the row had been claimed again.
    pub stale_completions: u64,
    /// Ticks of the per-node timer.
    pub ticks: u64,
    /// Leader writes acknowledged in the fault-free tail.
    pub tail_writes: u64,
    /// Leases found released after the final shutdown.
    pub released: u64,
}

impl Stats {
    /// Adds another run's counts.
    pub fn add(&mut self, other: &Self) {
        self.boots += other.boots;
        self.crashes += other.crashes;
        self.restarts += other.restarts;
        self.freezes += other.freezes;
        self.long_freezes += other.long_freezes;
        self.reserved_outages += other.reserved_outages;
        self.failovers += other.failovers;
        self.steals += other.steals;
        self.takeovers += other.takeovers;
        self.cursor_writes += other.cursor_writes;
        self.transactions += other.transactions;
        self.actions += other.actions;
        self.landed += other.landed;
        self.lease_lost += other.lease_lost;
        self.commit_unknown += other.commit_unknown;
        self.failed_runs += other.failed_runs;
        self.enqueued += other.enqueued;
        self.handled += other.handled;
        self.retries += other.retries;
        self.handler_failures += other.handler_failures;
        self.completions += other.completions;
        self.stale_completions += other.stale_completions;
        self.ticks += other.ticks;
        self.tail_writes += other.tail_writes;
        self.released += other.released;
    }
}

/// The result of one run.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The run seed.
    pub seed: u64,
    /// What happened.
    pub stats: Stats,
    /// Every invariant the run broke; empty when the runners held.
    pub violations: Vec<String>,
}

/// What the harness saw, shared by every node.
#[derive(Default)]
struct Ledger {
    stats: Stats,
    violations: Vec<String>,
    /// The acknowledged cursor values.
    acked_cursors: Vec<String>,
    /// The `(lease, token)` pairs whose lease was stolen from under them.
    stolen: BTreeSet<(LeaseName, u64)>,
    /// How often each row's effect was applied.
    effects: BTreeMap<Uuid, u32>,
    /// How often each row's completion was acknowledged.
    completions: BTreeMap<Uuid, u32>,
    tail: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn below(rng: &dyn Rng, bound: u64) -> u64 {
    rng.next_u64() % bound.max(1)
}

fn millis(rng: &dyn Rng, from: u64, to: u64) -> Duration {
    Duration::from_millis(from + below(rng, to - from + 1))
}

/// A leader write's value: the token, the holder and a counter.
fn value(lease: &Lease, n: u64) -> String {
    format!("{:010}/{}/{n:06}", lease.fencing_token, lease.holder)
}

/// The token and the holder of a leader write's value.
fn parse(value: &str) -> Option<(u64, &str)> {
    let mut parts = value.split('/');
    let token = parts.next()?.parse().ok()?;
    Some((token, parts.next()?))
}

/// The process state of one node that the harness controls: frozen, and cut off from its reserved pool.
struct Gate {
    clock: Arc<dyn Clock>,
    frozen_until: AtomicU64,
    reserved_down_until: AtomicU64,
}

impl Gate {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            frozen_until: AtomicU64::new(0),
            reserved_down_until: AtomicU64::new(0),
        }
    }

    /// Returns once the process runs.
    async fn thaw(&self) {
        loop {
            let until = self.frozen_until.load(Ordering::SeqCst);
            if self.clock.now().nanos() >= until {
                return;
            }
            self.clock.sleep_until(Instant::from_nanos(until)).await;
        }
    }

    fn reserved(&self) -> Result<(), TxError> {
        if self.clock.now().nanos() < self.reserved_down_until.load(Ordering::SeqCst) {
            Err(TxError::Unavailable(
                "the reserved pool is exhausted".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

/// A node's clock: no sleep wakes while the process is frozen.
struct NodeClock {
    inner: Arc<dyn Clock>,
    gate: Arc<Gate>,
}

impl fmt::Debug for NodeClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeClock")
    }
}

impl Clock for NodeClock {
    fn now(&self) -> Instant {
        self.inner.now()
    }

    fn wall_now(&self) -> Timestamp {
        self.inner.wall_now()
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'static, ()> {
        let sleep = self.inner.sleep_until(deadline);
        let gate = Arc::clone(&self.gate);
        Box::pin(async move {
            sleep.await;
            gate.thaw().await;
        })
    }
}

/// The lease statements of one node: they run on its reserved pool, and no call starts or returns while the
/// process is frozen.
struct LeaseCalls {
    model: PgModel,
    gate: Arc<Gate>,
}

#[async_trait]
impl Leases for LeaseCalls {
    async fn acquire(
        &self,
        name: LeaseName,
        holder: &str,
        ttl: Duration,
    ) -> Result<Option<Lease>, TxError> {
        self.gate.thaw().await;
        self.gate.reserved()?;
        let result = self.model.leases().acquire(name, holder, ttl).await;
        self.gate.thaw().await;
        result
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Option<Lease>, TxError> {
        self.gate.thaw().await;
        self.gate.reserved()?;
        let result = self.model.leases().renew(lease, ttl).await;
        self.gate.thaw().await;
        result
    }

    async fn release(&self, lease: &Lease) -> Result<bool, TxError> {
        self.gate.thaw().await;
        self.gate.reserved()?;
        let result = self.model.leases().release(lease).await;
        self.gate.thaw().await;
        result
    }

    async fn get(&self, name: LeaseName) -> Result<Option<Lease>, TxError> {
        self.gate.thaw().await;
        self.gate.reserved()?;
        let result = self.model.leases().get(name).await;
        self.gate.thaw().await;
        result
    }

    async fn cursor(&self, name: LeaseName) -> Result<Option<String>, TxError> {
        self.gate.thaw().await;
        self.gate.reserved()?;
        let result = self.model.leases().cursor(name).await;
        self.gate.thaw().await;
        result
    }

    async fn set_cursor(&self, lease: &Lease, cursor: &str) -> Result<(), TxError> {
        self.gate.thaw().await;
        self.gate.reserved()?;
        let result = self.model.leases().set_cursor(lease, cursor).await;
        self.gate.thaw().await;
        result
    }
}

/// The claim statements of one node, with the completion oracle of J2.
struct ClaimCalls {
    model: PgModel,
    gate: Arc<Gate>,
    ledger: Arc<Mutex<Ledger>>,
}

#[async_trait]
impl Claims for ClaimCalls {
    async fn enqueue(&self, queue: Queue, job: &NewJob) -> Result<(), TxError> {
        self.gate.thaw().await;
        let result = self.model.claims().enqueue(queue, job).await;
        self.gate.thaw().await;
        result
    }

    async fn claim(
        &self,
        queue: Queue,
        limit: u32,
        lease_for: Duration,
    ) -> Result<Vec<Claim>, TxError> {
        self.gate.thaw().await;
        let result = self.model.claims().claim(queue, limit, lease_for).await;
        self.gate.thaw().await;
        result
    }

    async fn complete(&self, claim: &Claim) -> Result<bool, TxError> {
        self.gate.thaw().await;
        let result = self.model.claims().complete(claim).await;
        {
            let mut ledger = lock(&self.ledger);
            match &result {
                Ok(true) => {
                    ledger.stats.completions += 1;
                    let count = ledger.completions.entry(claim.id).or_default();
                    *count += 1;
                    let count = *count;
                    if count > 1 {
                        ledger.violations.push(format!(
                            "J2: the completion of row {} was recorded {count} times",
                            claim.id
                        ));
                    }
                    // A done row is never claimed again, so its counter is still the completing claim's.
                    let attempts = self
                        .model
                        .job_states()
                        .into_iter()
                        .find(|job| job.id == claim.id)
                        .map(|job| job.attempts);
                    if attempts != Some(claim.attempt) {
                        ledger.violations.push(format!(
                            "J2: attempt {} completed row {} whose attempt counter is {attempts:?}",
                            claim.attempt, claim.id
                        ));
                    }
                    if !ledger.effects.contains_key(&claim.id) {
                        ledger.violations.push(format!(
                            "J2: row {} was completed before its effect was applied",
                            claim.id
                        ));
                    }
                }
                Ok(false) => ledger.stats.stale_completions += 1,
                Err(_) => {}
            }
        }
        self.gate.thaw().await;
        result
    }

    async fn fail(&self, claim: &Claim, retry_at: Timestamp) -> Result<bool, TxError> {
        self.gate.thaw().await;
        let result = self.model.claims().fail(claim, retry_at).await;
        self.gate.thaw().await;
        result
    }

    async fn park(&self, claim: &Claim) -> Result<bool, TxError> {
        self.gate.thaw().await;
        let result = self.model.claims().park(claim).await;
        self.gate.thaw().await;
        result
    }
}

/// The control store as one node reaches it.
struct NodeStore {
    model: PgModel,
    gate: Arc<Gate>,
    leases: LeaseCalls,
    claims: ClaimCalls,
}

impl fmt::Debug for NodeStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeStore")
    }
}

impl NodeStore {
    fn new(model: &PgModel, gate: &Arc<Gate>, ledger: &Arc<Mutex<Ledger>>) -> Self {
        Self {
            model: model.clone(),
            gate: Arc::clone(gate),
            leases: LeaseCalls {
                model: model.clone(),
                gate: Arc::clone(gate),
            },
            claims: ClaimCalls {
                model: model.clone(),
                gate: Arc::clone(gate),
                ledger: Arc::clone(ledger),
            },
        }
    }
}

impl Repos for NodeStore {
    fn org(&self) -> &dyn OrgRepo {
        self.model.org()
    }
    fn users(&self) -> &dyn UserRepo {
        self.model.users()
    }
    fn credentials(&self) -> &dyn CredentialRepo {
        self.model.credentials()
    }
    fn sessions(&self) -> &dyn SessionRepo {
        self.model.sessions()
    }
    fn projects(&self) -> &dyn ProjectRepo {
        self.model.projects()
    }
    fn memberships(&self) -> &dyn MembershipRepo {
        self.model.memberships()
    }
    fn api_keys(&self) -> &dyn ApiKeyRepo {
        self.model.api_keys()
    }
    fn key_feed(&self) -> &dyn KeyFeed {
        self.model.key_feed()
    }
    fn signing_keys(&self) -> &dyn SigningKeyRepo {
        self.model.signing_keys()
    }
    fn secrets(&self) -> &dyn SecretRepo {
        self.model.secrets()
    }
    fn leases(&self) -> &dyn Leases {
        &self.leases
    }
    fn claims(&self) -> &dyn Claims {
        &self.claims
    }
    fn migrations(&self) -> &dyn Migrations {
        self.model.migrations()
    }
}

#[async_trait]
impl ControlStore for NodeStore {
    async fn begin(&self) -> Result<Box<dyn ControlTx>, TxError> {
        self.gate.thaw().await;
        let tx = self.model.begin().await;
        self.gate.thaw().await;
        Ok(Box::new(NodeTx {
            inner: tx?,
            gate: Arc::clone(&self.gate),
        }))
    }

    async fn db_now(&self) -> Result<Timestamp, TxError> {
        self.gate.thaw().await;
        self.gate.reserved()?;
        let result = self.model.db_now().await;
        self.gate.thaw().await;
        result
    }
}

/// A transaction of one node: its fence and its commit wait for the thaw.
struct NodeTx {
    inner: Box<dyn ControlTx>,
    gate: Arc<Gate>,
}

impl fmt::Debug for NodeTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeTx")
    }
}

impl Repos for NodeTx {
    fn org(&self) -> &dyn OrgRepo {
        self.inner.org()
    }
    fn users(&self) -> &dyn UserRepo {
        self.inner.users()
    }
    fn credentials(&self) -> &dyn CredentialRepo {
        self.inner.credentials()
    }
    fn sessions(&self) -> &dyn SessionRepo {
        self.inner.sessions()
    }
    fn projects(&self) -> &dyn ProjectRepo {
        self.inner.projects()
    }
    fn memberships(&self) -> &dyn MembershipRepo {
        self.inner.memberships()
    }
    fn api_keys(&self) -> &dyn ApiKeyRepo {
        self.inner.api_keys()
    }
    fn key_feed(&self) -> &dyn KeyFeed {
        self.inner.key_feed()
    }
    fn signing_keys(&self) -> &dyn SigningKeyRepo {
        self.inner.signing_keys()
    }
    fn secrets(&self) -> &dyn SecretRepo {
        self.inner.secrets()
    }
    fn leases(&self) -> &dyn Leases {
        self.inner.leases()
    }
    fn claims(&self) -> &dyn Claims {
        self.inner.claims()
    }
    fn migrations(&self) -> &dyn Migrations {
        self.inner.migrations()
    }
}

#[async_trait]
impl ControlTx for NodeTx {
    async fn fence(&self, lease: &Lease) -> Result<(), TxError> {
        self.gate.thaw().await;
        let result = self.inner.fence(lease).await;
        self.gate.thaw().await;
        result
    }

    async fn commit(self: Box<Self>) -> Result<(), TxError> {
        let Self { inner, gate } = *self;
        gate.thaw().await;
        let result = inner.commit().await;
        gate.thaw().await;
        result
    }

    async fn rollback(self: Box<Self>) {
        self.inner.rollback().await;
    }
}

/// What every node and the harness share.
#[derive(Clone)]
struct World {
    model: PgModel,
    /// The database host: its clock is never frozen, and its task scope stands for the network.
    database: Env,
    ledger: Arc<Mutex<Ledger>>,
    landing_rng: Arc<dyn Rng>,
    options: Options,
}

impl World {
    /// A leader action on its way: it lands after the network's delay, whatever happens to its sender.
    fn land(&self, lease: Lease, what: String) -> impl Future<Output = ()> + use<> {
        let delay = Duration::from_nanos(below(
            self.landing_rng.as_ref(),
            u64::try_from(LANDING_MAX.as_nanos()).unwrap_or(u64::MAX) + 1,
        ));
        let world = self.clone();
        lock(&self.ledger).stats.actions += 1;
        let mut landing = self.database.spawn("landing", async move {
            world.database.clock.sleep(delay).await;
            let live = world.model.lease_is_live(&lease);
            let mut ledger = lock(&world.ledger);
            ledger.stats.landed += 1;
            let stolen = ledger
                .stolen
                .contains(&(lease.name, lease.fencing_token));
            if !live && !stolen {
                ledger.violations.push(format!(
                    "J1: the action {what} landed {delay:?} after it was issued, when the {} lease no \
                     longer carried its token or had expired",
                    lease.name
                ));
            }
        });
        async move {
            let _landed = landing.join().await;
        }
    }

    /// The leader job of the `lifecycle` lease: one fenced write per run, in one of the three forms.
    fn lifecycle(&self, rng: Arc<dyn Rng>) -> Job {
        let world = self.clone();
        let writes = Arc::new(AtomicU64::new(0));
        let cadence =
            Cadence::every(Duration::from_millis(400)).with_jitter(Duration::from_millis(400));
        Job::leader(
            "sim-lifecycle",
            LeaseName::Lifecycle,
            cadence,
            move |ctx: LeaderCtx| {
                let world = world.clone();
                let rng = Arc::clone(&rng);
                let writes = Arc::clone(&writes);
                async move {
                    let result = world.lead(&ctx, rng.as_ref(), &writes).await;
                    let mut ledger = lock(&world.ledger);
                    match &result {
                        Ok(()) => {
                            if ledger.tail {
                                ledger.stats.tail_writes += 1;
                            }
                        }
                        Err(JobError::LeaseLost) => ledger.stats.lease_lost += 1,
                        Err(JobError::Store(TxError::CommitUnknown)) => {
                            ledger.stats.commit_unknown += 1;
                        }
                        Err(JobError::Store(
                            TxError::Unavailable(_) | TxError::SerializationFailure,
                        )) => ledger.stats.failed_runs += 1,
                        Err(other) => ledger
                            .violations
                            .push(format!("an unexpected error of a leader run: {other:?}")),
                    }
                    result
                }
            },
        )
    }

    async fn lead(&self, ctx: &LeaderCtx, rng: &dyn Rng, writes: &AtomicU64) -> JobResult {
        let lease = ctx.lease()?;
        let what = value(&lease, writes.fetch_add(1, Ordering::SeqCst));
        let record = record(lease, what.clone());
        match below(rng, 3) {
            0 => {
                ctx.set_cursor(&what).await?;
                lock(&self.ledger).stats.cursor_writes += 1;
            }
            1 => {
                ctx.fenced(record).await?;
                lock(&self.ledger).stats.transactions += 1;
            }
            _ => {
                let action = |lease: &Lease| self.land(lease.clone(), what.clone());
                ctx.issue(record, action).await?;
                lock(&self.ledger).stats.transactions += 1;
            }
        }
        lock(&self.ledger).acked_cursors.push(what);
        ctx.env.clock.sleep(millis(rng, 0, 300)).await;
        Ok(())
    }

    /// The consumer of the outbox: works for a while, fails the first two attempts of a flaky row, then
    /// applies the row's effect.
    fn consumer(&self, rng: Arc<dyn Rng>) -> Job {
        let world = self.clone();
        let policy = ClaimPolicy {
            lease_for: CLAIM_FOR,
            ..ClaimPolicy::default()
        };
        Job::claimed(
            "sim-outbox",
            QUEUE,
            Cadence::every(Duration::from_secs(1)),
            policy,
            move |ctx: ClaimCtx| {
                let world = world.clone();
                let rng = Arc::clone(&rng);
                async move {
                    ctx.env.clock.sleep(millis(rng.as_ref(), 0, 2_500)).await;
                    let mut ledger = lock(&world.ledger);
                    ledger.stats.handled += 1;
                    if ctx.claim.attempt > 1 {
                        ledger.stats.retries += 1;
                    }
                    if ctx.claim.payload.starts_with("flaky") && ctx.claim.attempt <= 2 {
                        ledger.stats.handler_failures += 1;
                        return Err(JobError::Failed("the flaky row failed".to_owned()));
                    }
                    *ledger.effects.entry(ctx.claim.id).or_default() += 1;
                    Ok(())
                }
            },
        )
    }

    fn ticker(&self) -> Job {
        let ledger = Arc::clone(&self.ledger);
        Job::per_node(
            "sim-tick",
            Cadence::every(Duration::from_secs(5)),
            move |_ctx: NodeCtx| {
                lock(&ledger).stats.ticks += 1;
                async { Ok(()) }
            },
        )
    }

    fn registry(&self, rng: &Arc<dyn Rng>) -> Registry {
        let mut registry = Registry::new();
        registry
            .register(self.lifecycle(rng.fork("lifecycle")))
            .register(housekeeping::job())
            .register(self.consumer(rng.fork("consumer")))
            .register(self.ticker());
        if self.options.mutant == Some(Mutant::SkipValidityMargin) {
            registry.skip_validity_margin()
        } else {
            registry
        }
    }
}

/// The fenced record of a leader write: the cursor, stored in the transaction.
fn record(
    lease: Lease,
    what: String,
) -> impl for<'a> FnOnce(&'a dyn ControlTx) -> BoxFuture<'a, Result<(), TxError>> + Send {
    move |tx| Box::pin(async move { tx.leases().set_cursor(&lease, &what).await })
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// One node across its boots: starts the registry, then crashes, restarts, freezes and cuts the node off its
/// reserved pool at random until the faulty phase ends, and stops it at `end`.
async fn node(sim: &Sim, world: World, n: usize, faulty_until: u64, end: u64) {
    let rng = sim.rng(&format!("jobs-sim/node-{n}"));
    // Up to 5 min either way: the realistic tier's skew.
    let skew = i64::try_from(below(rng.as_ref(), 600_000_001)).unwrap_or(0) - 300_000_000;
    let disk = sim.disk(&format!("node-{n}"));
    let clock = Arc::clone(&world.database.clock);
    let mut boot = 0;
    loop {
        boot += 1;
        let name = format!("node-{n}#{boot}");
        let host = sim.host(&name, &disk, skew);
        let gate = Arc::new(Gate::new(Arc::clone(&clock)));
        let env = Env::new(
            Arc::new(NodeClock {
                inner: Arc::clone(&host.env.clock),
                gate: Arc::clone(&gate),
            }),
            Arc::clone(&host.env.rng),
            Arc::clone(&host.env.spawner),
            Arc::clone(&host.env.disk),
            Arc::clone(&host.env.net),
            Arc::clone(&host.env.metrics),
        );
        let store = Arc::new(NodeStore::new(&world.model, &gate, &world.ledger));
        let mut running = world
            .registry(&host.env.rng)
            .start(&env, Some(store), &name);
        lock(&world.ledger).stats.boots += 1;
        loop {
            let pause = millis(rng.as_ref(), 5_000, 40_000);
            let wake = clock.now().saturating_add(pause).nanos();
            if wake >= faulty_until {
                // The tail: the node runs undisturbed, then stops as an operator would stop it.
                clock.sleep_until(Instant::from_nanos(end)).await;
                host.spawner.request_shutdown();
                if clock
                    .timeout(Duration::from_secs(60), running.join())
                    .await
                    .is_err()
                {
                    lock(&world.ledger)
                        .violations
                        .push(format!("{name} did not stop within 60 s of its shutdown"));
                }
                host.stop().await;
                return;
            }
            clock.sleep(pause).await;
            match below(rng.as_ref(), 6) {
                0 => {
                    lock(&world.ledger).stats.crashes += 1;
                    host.crash().await;
                    clock.sleep(millis(rng.as_ref(), 0, 10_000)).await;
                    break;
                }
                1 => {
                    lock(&world.ledger).stats.restarts += 1;
                    host.spawner.request_shutdown();
                    if clock
                        .timeout(Duration::from_secs(90), running.join())
                        .await
                        .is_err()
                    {
                        lock(&world.ledger)
                            .violations
                            .push(format!("{name} did not stop within 90 s of its shutdown"));
                    }
                    host.stop().await;
                    break;
                }
                2 | 3 => {
                    let freeze = millis(rng.as_ref(), 1_000, 25_000);
                    let until = clock.now().saturating_add(freeze).nanos();
                    gate.frozen_until.fetch_max(until, Ordering::SeqCst);
                    let mut ledger = lock(&world.ledger);
                    ledger.stats.freezes += 1;
                    if freeze > LEASE_TTL {
                        ledger.stats.long_freezes += 1;
                    }
                }
                4 => {
                    let outage = millis(rng.as_ref(), 5_000, 20_000);
                    let until = clock.now().saturating_add(outage).nanos();
                    gate.reserved_down_until.fetch_max(until, Ordering::SeqCst);
                    lock(&world.ledger).stats.reserved_outages += 1;
                }
                _ => {}
            }
        }
    }
}

fn faults(on: bool) -> PgFaults {
    if on {
        PgFaults {
            unavailable_ppm: 10_000,
            commit_unknown_ppm: 20_000,
            spurious_serialization_ppm: 10_000,
            slow_commit_ppm: 20_000,
            slow_commit_micros: (1_000, 1_000_000),
            failover_ppm: 300,
            failover_micros: (200_000, 6_000_000),
            latency_micros: (100, 40_000),
            ..PgFaults::default()
        }
    } else {
        PgFaults::default()
    }
}

/// Fails the store over and steals leases at random moments until `until`.
async fn chaos(world: World, rng: Arc<dyn Rng>, until: u64) {
    let clock = &world.database.clock;
    let mut thieves = 0;
    loop {
        clock.sleep(millis(rng.as_ref(), 5_000, 30_000)).await;
        if clock.now().nanos() >= until {
            return;
        }
        match below(rng.as_ref(), 3) {
            0 => {
                world.model.fail_over(millis(rng.as_ref(), 200, 8_000));
                lock(&world.ledger).stats.failovers += 1;
            }
            1 if world.options.steals => {
                let name = LEASES[usize::from(below(rng.as_ref(), 2) == 1)];
                thieves += 1;
                let Some(lease) =
                    world
                        .model
                        .steal_lease(name, &format!("thief#{thieves}"), LEASE_TTL)
                else {
                    continue;
                };
                {
                    let mut ledger = lock(&world.ledger);
                    ledger.stats.steals += 1;
                    ledger.stolen.insert((name, lease.fencing_token - 1));
                }
                // The thief writes as the leader it now is, then gives the lease up or lets it expire.
                let what = value(&lease, 0);
                if name == LeaseName::Lifecycle
                    && world.model.leases().set_cursor(&lease, &what).await.is_ok()
                {
                    lock(&world.ledger).acked_cursors.push(what);
                }
                if below(rng.as_ref(), 2) == 0 {
                    let _released = world.model.leases().release(&lease).await;
                }
            }
            _ => {}
        }
    }
}

fn id(n: u64) -> Uuid {
    Uuid::from_v7_parts(1_800_000_000_000 + n, u128::from(n))
}

fn db_time(clock: &dyn Clock) -> Timestamp {
    let elapsed = i64::try_from(clock.now().nanos() / 1_000).unwrap_or(i64::MAX);
    Timestamp::from_micros(SIM_EPOCH_MICROS.saturating_add(elapsed))
}

fn session(n: u8, user: UserId, expires: Timestamp) -> Session {
    let mut token_hash: TokenHash = [0; 32];
    token_hash[0] = n;
    Session {
        token_hash,
        user_id: user,
        csrf_token: format!("csrf-{n}"),
        idle_expires_at: expires,
        absolute_expires_at: expires,
        auth_method: AuthMethod::Password,
        revoked_at: None,
    }
}

const OLD_SESSIONS: u8 = 4;
const FRESH_SESSIONS: u8 = 4;

/// The rows the housekeeping job will meet: a user with sessions that expired an hour ago and sessions that
/// expire in a day, and a project for the API keys.
async fn seed(world: &World) -> Result<ProjectId, TxError> {
    let store = &world.model;
    let user = User {
        id: UserId::from_uuid(id(9_000)),
        email: Email::new("ada@example.com")
            .ok_or_else(|| TxError::Invalid("the fixture email".to_owned()))?,
        name: "Ada".to_owned(),
        status: UserStatus::Active,
        email_verified_at: None,
    };
    store.users().insert(&user).await?;
    let now = db_time(world.database.clock.as_ref());
    for n in 0..OLD_SESSIONS + FRESH_SESSIONS {
        let expires = if n < OLD_SESSIONS {
            now.saturating_add_micros(-3_600_000_000)
        } else {
            now.saturating_add_micros(86_400_000_000)
        };
        store
            .sessions()
            .insert(&session(n, user.id, expires))
            .await?;
    }
    let project = Project {
        id: ProjectId::from_uuid(id(9_001)),
        name: "Web".to_owned(),
        slug: "web".to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    };
    store.projects().insert(&project).await?;
    Ok(project.id)
}

fn api_key(n: u64, project: ProjectId) -> ApiKey {
    let byte = u8::try_from(n % 251).unwrap_or(0);
    ApiKey {
        id: ApiKeyId::from_uuid(id(10_000 + n)),
        project_id: project,
        public_key: format!("cpk_{n:043}"),
        secret_salt: [byte; 16],
        secret_hash: [byte; 32],
        lookup_tag: [byte; 8],
        scopes: vec![KeyScope::Ingest],
        import: false,
        note: String::new(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        version: 1,
    }
}

/// The workload until `until`: enqueues outbox rows and creates API keys, which write feed rows.
async fn workload(world: World, rng: Arc<dyn Rng>, project: ProjectId, until: u64) {
    let clock = &world.database.clock;
    let mut n = 0;
    loop {
        clock.sleep(millis(rng.as_ref(), 500, 4_000)).await;
        if clock.now().nanos() >= until {
            return;
        }
        n += 1;
        let kind = if n % 4 == 1 { "flaky" } else { "plain" };
        // Due up to 5 s ago or up to 10 s from now.
        let due = i64::try_from(below(rng.as_ref(), 15_000_001)).unwrap_or(0) - 5_000_000;
        let job = NewJob {
            id: id(n),
            dedup_key: format!("row-{n}"),
            payload: format!("{kind}-{n}"),
            run_at: db_time(clock.as_ref()).saturating_add_micros(due),
        };
        // An attempt that failed without an answer is made again: the same row, so at most one exists.
        for _ in 0..20 {
            match world.model.claims().enqueue(QUEUE, &job).await {
                Ok(()) | Err(TxError::UniqueViolation(_)) => {
                    lock(&world.ledger).stats.enqueued += 1;
                    break;
                }
                Err(_) => clock.sleep(Duration::from_millis(500)).await,
            }
        }
        if n % 3 == 0 {
            let _created = world.model.api_keys().insert(&api_key(n, project)).await;
        }
    }
}

/// J1 over what the store committed.
fn audit_leases(world: &World, ledger: &mut Ledger) {
    let mut rows: BTreeMap<LeaseName, Lease> = BTreeMap::new();
    let mut cursors = BTreeSet::new();
    for event in world.model.lease_events() {
        match event {
            LeaseEvent::Lease(lease) => {
                if let Some(old) = rows.get(&lease.name) {
                    let same = old.holder == lease.holder;
                    let ok = if same {
                        lease.fencing_token == old.fencing_token
                            || lease.fencing_token == old.fencing_token + 1
                    } else {
                        lease.fencing_token == old.fencing_token + 1
                    };
                    if !ok {
                        ledger
                            .violations
                            .push(format!("J1: the lease row went from {old:?} to {lease:?}"));
                    }
                    if !same {
                        ledger.stats.takeovers += 1;
                    }
                }
                rows.insert(lease.name, lease);
            }
            LeaseEvent::Cursor {
                cursor,
                token,
                holder,
                ..
            } => {
                if parse(&cursor) != Some((token, holder.as_str())) {
                    ledger.violations.push(format!(
                        "J1: cursor {cursor} was committed while the lease row carried token {token} of \
                         {holder}"
                    ));
                }
                cursors.insert(cursor);
            }
        }
    }
    for cursor in &ledger.acked_cursors {
        if !cursors.contains(cursor) {
            ledger.violations.push(format!(
                "J1: the acknowledged cursor {cursor} was never committed"
            ));
        }
    }
}

/// J2 and J3 over the rows of the queue.
fn audit_rows(world: &World, ledger: &mut Ledger) {
    let rows = world.model.job_states();
    if (rows.len() as u64) < ledger.stats.enqueued {
        ledger.violations.push(format!(
            "J3: {} rows were enqueued and {} exist",
            ledger.stats.enqueued,
            rows.len()
        ));
    }
    for row in rows {
        if row.status != JobStatus::Done {
            ledger.violations.push(format!(
                "J3: row {} ({}) is {:?} after {} attempts",
                row.id, row.payload, row.status, row.attempts
            ));
        } else if !ledger.effects.contains_key(&row.id) {
            ledger.violations.push(format!(
                "J2: row {} ({}) is done and its effect was never applied",
                row.id, row.payload
            ));
        }
    }
}

/// J3 for the leases, at the end of the tail and before the nodes stop: each is held, unexpired, by a node.
async fn audit_held(world: &World, ledger: &Mutex<Ledger>) {
    let now = db_time(world.database.clock.as_ref());
    for name in LEASES {
        match world.model.leases().get(name).await {
            Ok(Some(lease)) if lease.expires_at >= now && lease.holder.starts_with("node-") => {}
            other => lock(ledger).violations.push(format!(
                "J3: the {name} lease is not held at the end of the tail: {other:?}"
            )),
        }
    }
}

/// What the housekeeping job must have done by the end: the feed rows are older than its horizon and gone,
/// the expired sessions are gone and the live ones are not.
async fn audit_housekeeping(world: &World, ledger: &mut Ledger) {
    if world.options.tail > KEY_FEED_KEEP + housekeeping::EVERY * 3 && world.model.feed_len() > 0 {
        ledger.violations.push(format!(
            "housekeeping: {} key-feed rows older than the horizon are left",
            world.model.feed_len()
        ));
    }
    for n in 0..OLD_SESSIONS + FRESH_SESSIONS {
        let mut hash: TokenHash = [0; 32];
        hash[0] = n;
        let found = matches!(world.model.sessions().get(&hash).await, Ok(Some(_)));
        if found == (n < OLD_SESSIONS) {
            ledger.violations.push(format!(
                "housekeeping: session {n} is {} after the run",
                if found { "still there" } else { "gone" }
            ));
        }
    }
}

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let database = sim.host("postgres", &sim.disk("postgres"), 0);
    let model = PgModel::new(
        database.clock.clone(),
        sim.rng("jobs-sim/pg").as_ref(),
        PgFaults::default(),
    );
    model.set_mutant(match options.mutant {
        Some(Mutant::RenewWithoutCompareAndSet) => Some(PgMutant::RenewWithoutCompareAndSet),
        Some(Mutant::CompleteWithoutClaimCheck) => Some(PgMutant::CompleteWithoutClaimCheck),
        Some(Mutant::SkipValidityMargin) | None => None,
    });
    let ledger = Arc::new(Mutex::new(Ledger::default()));
    let world = World {
        model: model.clone(),
        database: database.env.clone(),
        ledger: Arc::clone(&ledger),
        landing_rng: sim.rng("jobs-sim/landing"),
        options,
    };
    let project = match seed(&world).await {
        Ok(project) => project,
        Err(error) => {
            return Outcome {
                seed: sim.seed(),
                stats: Stats::default(),
                violations: vec![format!("seeding the store: {error}")],
            };
        }
    };
    model.set_faults(faults(true));
    let clock = &world.database.clock;
    let faulty_until = clock.now().saturating_add(options.faulty_for).nanos();
    let end = faulty_until + nanos(options.tail);
    let nodes: Vec<_> = (0..options.nodes)
        .map(|n| node(&sim, world.clone(), n, faulty_until, end))
        .collect();
    let calm = async {
        clock.sleep_until(Instant::from_nanos(faulty_until)).await;
        model.set_faults(faults(false));
        lock(&ledger).tail = true;
        // Just before the nodes are stopped.
        clock
            .sleep_until(Instant::from_nanos(end - nanos(Duration::from_millis(1))))
            .await;
        audit_held(&world, &ledger).await;
    };
    futures::join!(
        futures::future::join_all(nodes),
        chaos(world.clone(), sim.rng("jobs-sim/chaos"), faulty_until),
        workload(
            world.clone(),
            sim.rng("jobs-sim/workload"),
            project,
            faulty_until
        ),
        calm,
    );
    // Actions still on their way land.
    clock.sleep(LANDING_MAX * 2).await;
    let mut ledger = std::mem::take(&mut *lock(&ledger));
    audit_leases(&world, &mut ledger);
    audit_rows(&world, &mut ledger);
    audit_housekeeping(&world, &mut ledger).await;
    if ledger.stats.tail_writes == 0 {
        ledger
            .violations
            .push("J3: no leader write was acknowledged in the fault-free tail".to_owned());
    }
    // Every node stopped at the same instant with nothing in its way, so both leases were released.
    for name in LEASES {
        match model.leases().get(name).await {
            Ok(Some(lease)) if lease.expires_at == Timestamp::from_micros(0) => {
                ledger.stats.released += 1;
            }
            other => ledger.violations.push(format!(
                "J3: the {name} lease was not released on shutdown: {other:?}"
            )),
        }
    }
    database.stop().await;
    Outcome {
        seed: sim.seed(),
        stats: ledger.stats,
        violations: ledger.violations,
    }
}

/// Runs the jobs simulation for one seed.
pub fn run_jobs_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}
