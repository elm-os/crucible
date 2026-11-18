//! Leader election with leases under `PgModel` faults (contract 11.7).
//!
//! Several nodes compete for one lease. A leader renews every 5 s, trusts its lease only inside the local
//! window of `HeldLease`, and does two kinds of leader work: a fenced single statement (the job cursor) and a
//! fenced transaction (a ledger row). Nodes stall at the worst moment, after the validity check and before
//! the write, for longer than the lease lives. The store fails over, loses commits with an unknown outcome,
//! aborts transactions and steps its clock against the nodes'.
//!
//! The oracle:
//!
//! - **one leader per token**: a fencing token is acknowledged to one node only, and every leader write that
//!   was acknowledged or that the store committed carries the token and the holder of the lease row at its
//!   commit;
//! - **stale tokens are refused**: no leader write is acknowledged once a higher token has been acknowledged
//!   to anyone before the write began;
//! - **an unknown acquire never acts**: no leader write exists under a token that was never acknowledged;
//! - **acknowledged writes survive a failover**;
//! - **the local window is safe**: without database clock steps, a write that began inside the local window
//!   is never refused as fenced;
//! - **progress**: once the faults stop, some node leads again.
//!
//! `Options::unfenced` is the mutant: the transaction skips its fence, and the oracle must notice.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crucible_control::{
    ControlStore, HeldLease, LEASE_RENEW_EVERY, LEASE_TTL, Lease, LeaseName, LedgerEntry,
    MigrationKind, Repos, TxError,
};
use crucible_env::{Clock, Rng};

use crate::pg_model::{LeaseEvent, PgFaults, PgModel};
use crate::{Sim, run};

const NAME: LeaseName = LeaseName::Lifecycle;

/// What one run does.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Competing nodes.
    pub nodes: usize,
    /// How long the faulty phase lasts, in virtual time. A fault-free tail of two minutes follows.
    pub faulty_for: Duration,
    /// Whether the database clock is stepped against the node clocks.
    pub clock_steps: bool,
    /// The mutant: leader transactions skip their fence.
    pub unfenced: bool,
}

impl Options {
    /// Failovers, unknown commits, aborted transactions, stalls and clock steps.
    pub const FAULTY: Self = Self {
        nodes: 3,
        faulty_for: Duration::from_secs(600),
        clock_steps: true,
        unfenced: false,
    };
    /// The same without clock steps, where the local window must be safe.
    pub const STEADY_CLOCK: Self = Self {
        clock_steps: false,
        ..Self::FAULTY
    };
}

/// What happened in a run, so that a test can require that the interesting cases occurred.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Stats {
    /// Acquisitions acknowledged to a node.
    pub acquired: u64,
    /// Acquisitions acknowledged to a node other than the previous holder.
    pub takeovers: u64,
    /// Renewals acknowledged.
    pub renewed: u64,
    /// Leases given up on purpose.
    pub released: u64,
    /// Fenced cursor writes acknowledged.
    pub cursor_writes: u64,
    /// Leader transactions acknowledged.
    pub transactions: u64,
    /// Leader writes attempted after the local window had ended (the node stalled).
    pub stale_attempts: u64,
    /// Leader writes refused as fenced.
    pub fenced: u64,
    /// Calls whose outcome was unknown.
    pub commit_unknown: u64,
    /// Acquisitions whose outcome was unknown.
    pub acquire_unknown: u64,
    /// Calls that failed as unavailable.
    pub unavailable: u64,
    /// Transactions aborted by a serialization failure.
    pub serialization_failures: u64,
    /// Failovers the harness started (the fault stream starts more).
    pub failovers: u64,
    /// Steps of the database clock.
    pub clock_steps: u64,
    /// Leader writes acknowledged in the fault-free tail.
    pub tail_writes: u64,
}

impl Stats {
    /// Adds another run's counts.
    pub fn add(&mut self, other: &Self) {
        self.acquired += other.acquired;
        self.takeovers += other.takeovers;
        self.renewed += other.renewed;
        self.released += other.released;
        self.cursor_writes += other.cursor_writes;
        self.transactions += other.transactions;
        self.stale_attempts += other.stale_attempts;
        self.fenced += other.fenced;
        self.commit_unknown += other.commit_unknown;
        self.acquire_unknown += other.acquire_unknown;
        self.unavailable += other.unavailable;
        self.serialization_failures += other.serialization_failures;
        self.failovers += other.failovers;
        self.clock_steps += other.clock_steps;
        self.tail_writes += other.tail_writes;
    }
}

/// The result of one run.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The run seed.
    pub seed: u64,
    /// What happened.
    pub stats: Stats,
    /// Every invariant the run broke; empty when the lease protocol held.
    pub violations: Vec<String>,
}

/// What the nodes were told, shared by all of them.
#[derive(Default)]
struct Ledger {
    stats: Stats,
    violations: Vec<String>,
    /// The highest fencing token acknowledged to any node.
    max_acked: u64,
    /// Who each acknowledged token belongs to.
    owner: BTreeMap<u64, String>,
    last_holder: Option<String>,
    /// The ledger ids of the acknowledged leader transactions.
    acked_rows: Vec<String>,
    /// The acknowledged cursor values.
    acked_cursors: Vec<String>,
    tail: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Ledger {
    fn failed(&mut self, error: &TxError) {
        match error {
            TxError::CommitUnknown => self.stats.commit_unknown += 1,
            TxError::Unavailable(_) => self.stats.unavailable += 1,
            TxError::SerializationFailure => self.stats.serialization_failures += 1,
            TxError::Fenced => self.stats.fenced += 1,
            other => self
                .violations
                .push(format!("an unexpected error: {other:?}")),
        }
    }

    fn acquired(&mut self, lease: &Lease) {
        self.stats.acquired += 1;
        if self
            .last_holder
            .as_ref()
            .is_some_and(|last| *last != lease.holder)
        {
            self.stats.takeovers += 1;
        }
        self.last_holder = Some(lease.holder.clone());
        if lease.fencing_token <= self.max_acked {
            self.violations.push(format!(
                "token {} was acknowledged to {} after token {}",
                lease.fencing_token, lease.holder, self.max_acked
            ));
        }
        self.max_acked = self.max_acked.max(lease.fencing_token);
        if let Some(other) = self
            .owner
            .insert(lease.fencing_token, lease.holder.clone())
        {
            self.violations.push(format!(
                "token {} was acknowledged to {other} and to {}",
                lease.fencing_token, lease.holder
            ));
        }
    }

    /// An acknowledged leader write under `lease`, begun when the highest acknowledged token was `before`.
    fn wrote(&mut self, lease: &Lease, before: u64, what: &str) {
        if before > lease.fencing_token {
            self.violations.push(format!(
                "{} wrote {what} under token {} after token {before} had been acknowledged",
                lease.holder, lease.fencing_token
            ));
        }
        if self.tail {
            self.stats.tail_writes += 1;
        }
    }
}

struct Node {
    name: String,
    store: PgModel,
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
    ledger: Arc<Mutex<Ledger>>,
    options: Options,
    writes: u64,
}

fn below(rng: &dyn Rng, bound: u64) -> u64 {
    rng.next_u64() % bound.max(1)
}

fn millis(rng: &dyn Rng, from: u64, to: u64) -> Duration {
    Duration::from_millis(from + below(rng, to - from + 1))
}

/// A leader write's value: the token, the holder and a per-node counter.
fn value(lease: &Lease, n: u64) -> String {
    format!("{:010}/{}/{n:06}", lease.fencing_token, lease.holder)
}

/// The token and the holder of a leader write's value.
fn parse(value: &str) -> Option<(u64, &str)> {
    let mut parts = value.split('/');
    let token = parts.next()?.parse().ok()?;
    Some((token, parts.next()?))
}

impl Node {
    async fn run(mut self, until_nanos: u64) {
        let mut held: Option<(HeldLease, u64)> = None;
        while self.clock.now().nanos() < until_nanos {
            match held.take() {
                None => {
                    held = self.acquire().await;
                    if held.is_none() {
                        self.clock.sleep(millis(self.rng.as_ref(), 500, 4_000)).await;
                    }
                }
                Some((lease, renewed_at)) => held = self.lead(lease, renewed_at).await,
            }
        }
    }

    async fn acquire(&self) -> Option<(HeldLease, u64)> {
        let asked = self.clock.now().nanos();
        match self
            .store
            .leases()
            .acquire(NAME, &self.name, LEASE_TTL)
            .await
        {
            Ok(Some(lease)) => {
                lock(&self.ledger).acquired(&lease);
                Some((HeldLease::new(lease, asked, LEASE_TTL), asked))
            }
            Ok(None) => None,
            Err(error) => {
                let mut ledger = lock(&self.ledger);
                if error == TxError::CommitUnknown {
                    ledger.stats.acquire_unknown += 1;
                }
                ledger.failed(&error);
                None
            }
        }
    }

    /// One round as the leader. Returns the lease while the node still holds it.
    async fn lead(&mut self, held: HeldLease, renewed_at: u64) -> Option<(HeldLease, u64)> {
        let rng = Arc::clone(&self.rng);
        if !held.is_valid_at(self.clock.now().nanos()) {
            return None;
        }
        // The node passed its validity check. One time in eight it now stalls, up to longer than the lease
        // lives, and then writes anyway: the store must refuse what the local window can no longer protect.
        let in_window = if below(rng.as_ref(), 8) == 0 {
            self.clock.sleep(millis(rng.as_ref(), 0, 25_000)).await;
            held.is_valid_at(self.clock.now().nanos())
        } else {
            true
        };
        if !in_window {
            lock(&self.ledger).stats.stale_attempts += 1;
        }
        let lease = &held.lease;
        let before = lock(&self.ledger).max_acked;
        self.writes += 1;
        let what = value(lease, self.writes);
        let result = if below(rng.as_ref(), 2) == 0 && !self.options.unfenced {
            let result = self.store.leases().set_cursor(lease, &what).await;
            if result.is_ok() {
                let mut ledger = lock(&self.ledger);
                ledger.stats.cursor_writes += 1;
                ledger.acked_cursors.push(what.clone());
            }
            result
        } else {
            let result = self.transaction(lease, &what).await;
            if result.is_ok() {
                let mut ledger = lock(&self.ledger);
                ledger.stats.transactions += 1;
                ledger.acked_rows.push(what.clone());
            }
            result
        };
        match result {
            Ok(()) => lock(&self.ledger).wrote(lease, before, &what),
            Err(TxError::Fenced) => {
                let mut ledger = lock(&self.ledger);
                ledger.stats.fenced += 1;
                if in_window && !self.options.clock_steps {
                    ledger.violations.push(format!(
                        "{} was fenced inside its local window under token {}",
                        lease.holder, lease.fencing_token
                    ));
                }
                return None;
            }
            Err(error) => lock(&self.ledger).failed(&error),
        }
        if below(rng.as_ref(), 40) == 0 {
            match self.store.leases().release(lease).await {
                Ok(_) => lock(&self.ledger).stats.released += 1,
                Err(error) => lock(&self.ledger).failed(&error),
            }
            return None;
        }
        let now = self.clock.now().nanos();
        let due = renewed_at.saturating_add(nanos(LEASE_RENEW_EVERY));
        if now < due {
            self.clock.sleep(millis(rng.as_ref(), 100, 900)).await;
            return Some((held, renewed_at));
        }
        let asked = self.clock.now().nanos();
        match self.store.leases().renew(lease, LEASE_TTL).await {
            Ok(Some(lease)) => {
                lock(&self.ledger).stats.renewed += 1;
                Some((HeldLease::new(lease, asked, LEASE_TTL), asked))
            }
            Ok(None) => None,
            Err(error) => {
                lock(&self.ledger).failed(&error);
                // The outcome is unknown or nothing was applied: the old local window still bounds the node.
                Some((held, renewed_at))
            }
        }
    }

    /// A fenced leader transaction: one ledger row under the lease.
    async fn transaction(&self, lease: &Lease, id: &str) -> Result<(), TxError> {
        let tx = self.store.begin().await?;
        if !self.options.unfenced {
            tx.fence(lease).await?;
        }
        let entry = LedgerEntry {
            id: id.to_owned(),
            kind: MigrationKind::ClickhouseSchema,
            applied_at: self.clock.wall_now(),
            checksum: String::new(),
        };
        tx.migrations().apply(&entry).await?;
        tx.commit().await
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn faults(on: bool) -> PgFaults {
    if on {
        PgFaults {
            unavailable_ppm: 20_000,
            commit_unknown_ppm: 30_000,
            spurious_serialization_ppm: 20_000,
            slow_commit_ppm: 30_000,
            slow_commit_micros: (1_000, 2_000_000),
            failover_ppm: 1_000,
            failover_micros: (200_000, 6_000_000),
            latency_micros: (100, 40_000),
            ..PgFaults::default()
        }
    } else {
        PgFaults::default()
    }
}

/// Fails the store over and steps its clock at random moments until `until_nanos`.
async fn chaos(
    store: PgModel,
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
    ledger: Arc<Mutex<Ledger>>,
    options: Options,
    until_nanos: u64,
) {
    loop {
        clock.sleep(millis(rng.as_ref(), 2_000, 15_000)).await;
        if clock.now().nanos() >= until_nanos {
            return;
        }
        if below(rng.as_ref(), 2) == 0 {
            store.fail_over(millis(rng.as_ref(), 200, 8_000));
            lock(&ledger).stats.failovers += 1;
        } else if options.clock_steps {
            // Up to 20 s either way: more than the lease lives.
            let step = i64::try_from(below(rng.as_ref(), 40_000_001)).unwrap_or(0) - 20_000_000;
            store.step_db_clock(step);
            lock(&ledger).stats.clock_steps += 1;
        }
    }
}

/// What the store committed, checked against what the nodes were told.
async fn audit(store: &PgModel, ledger: &mut Ledger) {
    let mut row: Option<Lease> = None;
    let mut cursors = Vec::new();
    for event in store.lease_events() {
        match event {
            LeaseEvent::Lease(lease) => {
                if let Some(old) = &row {
                    let same = old.holder == lease.holder;
                    let ok = if same {
                        lease.fencing_token == old.fencing_token
                            || lease.fencing_token == old.fencing_token + 1
                    } else {
                        lease.fencing_token == old.fencing_token + 1
                    };
                    if !ok {
                        ledger.violations.push(format!(
                            "the lease row went from {old:?} to {lease:?}"
                        ));
                    }
                }
                row = Some(lease);
            }
            LeaseEvent::Cursor {
                cursor,
                token,
                holder,
                ..
            } => {
                if parse(&cursor) != Some((token, holder.as_str())) {
                    ledger.violations.push(format!(
                        "cursor {cursor} was committed while the lease row carried token {token} of {holder}"
                    ));
                }
                cursors.push(cursor);
            }
        }
    }
    for cursor in &ledger.acked_cursors {
        if !cursors.contains(cursor) {
            ledger
                .violations
                .push(format!("the acknowledged cursor {cursor} was never committed"));
        }
    }
    let rows = match store.migrations().applied().await {
        Ok(rows) => rows,
        Err(error) => {
            ledger
                .violations
                .push(format!("reading the ledger after the faults: {error}"));
            return;
        }
    };
    let rows: Vec<String> = rows.into_iter().map(|row| row.id).collect();
    for id in &ledger.acked_rows {
        if !rows.contains(id) {
            ledger
                .violations
                .push(format!("the acknowledged row {id} is gone"));
        }
    }
    for written in rows.iter().chain(&cursors) {
        match parse(written) {
            Some((token, holder)) if ledger.owner.get(&token).map(String::as_str) == Some(holder) => {}
            _ => ledger.violations.push(format!(
                "{written} was committed under a token that was never acknowledged to its writer"
            )),
        }
    }
}

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let database = sim.host("postgres", &sim.disk("postgres"), 0);
    let store = PgModel::new(
        database.clock.clone(),
        sim.rng("lease-sim/pg").as_ref(),
        faults(true),
    );
    let ledger = Arc::new(Mutex::new(Ledger::default()));
    let faulty_until = nanos(options.faulty_for);
    let end = faulty_until + nanos(Duration::from_secs(120));
    let rng = sim.rng("lease-sim/skew");
    let nodes: Vec<_> = (0..options.nodes)
        .map(|n| {
            let name = format!("node-{n}");
            // Each node's wall clock is up to a minute off the database's.
            let skew = i64::try_from(below(rng.as_ref(), 120_000_001)).unwrap_or(0) - 60_000_000;
            let host = sim.host(&name, &sim.disk(&name), skew);
            Node {
                rng: sim.rng(&format!("lease-sim/{name}")),
                name,
                store: store.clone(),
                clock: host.clock.clone(),
                ledger: Arc::clone(&ledger),
                options,
                writes: 0,
            }
            .run(end)
        })
        .collect();
    let calm = async {
        database
            .clock
            .sleep_until(crucible_env::Instant::from_nanos(faulty_until))
            .await;
        store.set_faults(faults(false));
        lock(&ledger).tail = true;
    };
    futures::join!(
        futures::future::join_all(nodes),
        chaos(
            store.clone(),
            database.clock.clone(),
            sim.rng("lease-sim/chaos"),
            Arc::clone(&ledger),
            options,
            faulty_until,
        ),
        calm,
    );
    let mut ledger = std::mem::take(&mut *lock(&ledger));
    audit(&store, &mut ledger).await;
    if ledger.stats.tail_writes == 0 {
        ledger
            .violations
            .push("no node led in the two fault-free minutes at the end".to_owned());
    }
    Outcome {
        seed: sim.seed(),
        stats: ledger.stats,
        violations: ledger.violations,
    }
}

/// Runs the lease simulation for one seed.
pub fn run_lease_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}
