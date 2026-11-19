//! The simulated Postgres behind `ControlStore` (contract section 2.7, `PgModel`).
//!
//! - Rows live in one multi-version map. A transaction reads the snapshot taken at its first statement and
//!   its own writes; a single statement outside a transaction reads the latest committed rows.
//! - A write takes an exclusive row lock until its transaction ends. A transaction that writes a row changed
//!   since its snapshot, or one that another open transaction has locked, fails with a serialization failure.
//!   Real Postgres would wait for the lock and fail only if the other transaction commits, so the model is
//!   stricter here. A single statement waits for the lock, as read committed does.
//! - Unique and foreign-key constraints carry the names of the production schema and are checked against the
//!   latest committed rows, as Postgres checks them. Deleting a user cascades.
//! - A fenced transaction share-locks its lease row, so an acquisition waits for it (contract 11.7).
//! - Key-feed sequence numbers are drawn when the row is written and survive a rollback, so they become
//!   visible out of order and leave gaps (contract 11.8).
//! - Lease expiry, claim leases and feed timestamps use the database clock, which is the model's clock plus
//!   an offset that a test can step.
//! - Faults, seeded: a call that fails as unavailable, a commit whose outcome is unknown (applied or not),
//!   a spurious serialization failure, a slow commit (locks held, sequence numbers already drawn), pool
//!   exhaustion from the pool sizes, and a failover: every open transaction is lost, every call fails for a
//!   while, and afterwards every acknowledged commit is still there.
//!
//! The model may be stricter than Postgres and is never more permissive. `crucible-conformance` runs the same
//! scripts against this model and against Postgres 15 and 18 and compares the results.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_control::{
    AlertEvent, AlertEventRepo, AlertRule, AlertRuleRepo, AlertSeriesState, AlertStateRepo, ApiKey,
    ApiKeyRepo, AuthFlow, AuthFlowRepo, AuthProvider, AuthProviderRepo, Channel, ChannelRepo,
    Claim, Claims, ControlStore, ControlTx, Credential, CredentialRepo, DeletionRequest,
    ExportJobRepo, Invitation, InvitationRepo, KeyChange, KeyChangeKind, KeyFeed, KeySnapshot,
    Lease, LeaseName, Leases, LedgerEntry, LifecycleAction, LifecycleRepo, MembershipRepo,
    Migrations, NewJob, Node, NodeAck, OauthAccount, OauthAccountRepo, OneTimeToken,
    OneTimeTokenRepo, Org, OrgMembership, OrgRepo, OrgRole, Project, ProjectMembership,
    ProjectRepo, ProjectSaltRepo, Queue, Repos, RetentionFloor, SealedProjectSalt, Secret,
    SecretRepo, Session, SessionRepo, SigningKey, SigningKeyRepo, SigningKeyState, SigningPurpose,
    TRANSACTION_CAP, TokenHash, TxError, User, UserRepo, duration_micros,
};
use crucible_env::{Clock, ClockExt, Instant, Rng};
use crucible_records::{ApiKeyId, ProjectId, Timestamp, UserId, Uuid};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The statement timeout of the production pools (contract 2.7).
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a waiting statement looks at its lock again.
const LOCK_POLL: Duration = Duration::from_millis(1);

/// The seeded faults of the store. Rates are parts per million of the calls they apply to.
#[derive(Clone, Debug)]
pub struct PgFaults {
    /// A call fails as unavailable and changes nothing. Inside a transaction the connection is gone, so the
    /// transaction is lost.
    pub unavailable_ppm: u32,
    /// A commit, or a single writing statement, loses its connection: the caller gets `CommitUnknown` and
    /// the write is applied or not, by a coin.
    pub commit_unknown_ppm: u32,
    /// A statement inside a transaction fails with a serialization failure without a conflict.
    pub spurious_serialization_ppm: u32,
    /// A single writing statement commits late: it holds its locks and its sequence numbers for a time drawn
    /// from `slow_commit_micros`.
    pub slow_commit_ppm: u32,
    /// How long a slow commit takes, drawn uniformly from this range.
    pub slow_commit_micros: (u64, u64),
    /// A call starts a failover that lasts a time drawn from `failover_micros`.
    pub failover_ppm: u32,
    /// How long a failover lasts, drawn uniformly from this range.
    pub failover_micros: (u64, u64),
    /// The answer delay, drawn uniformly from this range.
    pub latency_micros: (u64, u64),
    /// Connections of the main pool. A transaction holds one from `begin` until it ends.
    pub main_pool: usize,
    /// Connections of the reserved pool (the key feed, leases, session lookups, the database clock).
    pub reserved_pool: usize,
    /// How long a call waits for a connection before it fails as unavailable.
    pub acquire_timeout: Duration,
}

impl Default for PgFaults {
    fn default() -> Self {
        Self {
            unavailable_ppm: 0,
            commit_unknown_ppm: 0,
            spurious_serialization_ppm: 0,
            slow_commit_ppm: 0,
            slow_commit_micros: (0, 0),
            failover_ppm: 0,
            failover_micros: (0, 0),
            latency_micros: (0, 0),
            main_pool: 8,
            reserved_pool: 2,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

/// What the model committed about leases and job cursors, in commit order, for the oracles.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LeaseEvent {
    /// A lease row was written: acquired, renewed or released.
    Lease(Lease),
    /// A job cursor was written while the lease row carried `token`.
    Cursor {
        /// The lease the cursor belongs to.
        name: LeaseName,
        /// The stored cursor.
        cursor: String,
        /// The fencing token of the lease row at the commit.
        token: u64,
        /// The holder of the lease row at the commit.
        holder: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Tbl {
    Org,
    Users,
    Credentials,
    Sessions,
    Projects,
    OrgMembers,
    ProjectMembers,
    ApiKeys,
    Feed,
    SigningKeys,
    Secrets,
    ProjectSalts,
    Leases,
    Cursors,
    Jobs,
    Ledger,
    SqlIdentities,
    SavedQueries,
    SqlRuns,
    AuthProviders,
    OauthAccounts,
    AuthFlows,
    OneTimeTokens,
    Invitations,
    Channels,
    AlertRules,
    AlertState,
    AlertEvents,
    Nodes,
    DeletionRequests,
    DeletionAcks,
    Actions,
    Floors,
}

type Key = Vec<u8>;

#[derive(Clone, Debug)]
struct FeedRow {
    seq: u64,
    key_id: ApiKeyId,
    change: KeyChangeKind,
    key_version: u64,
    at: Timestamp,
}

#[derive(Clone, Debug)]
struct JobRow {
    queue: Queue,
    id: Uuid,
    dedup_key: String,
    payload: String,
    attempts: u32,
    next_attempt_at: Timestamp,
    status: JobStatus,
    lease_until: Option<Timestamp>,
    /// The export's own columns, on a row of [`Queue::ExportJobs`].
    export: Option<Box<exports::ExportCols>>,
}

/// The constraint names of a queue's table.
const fn queue_constraints(queue: Queue) -> (&'static str, &'static str) {
    match queue {
        Queue::NotificationOutbox => (
            "notification_outbox_pkey",
            "notification_outbox_dedup_key_key",
        ),
        Queue::ExportJobs => ("export_jobs_pkey", "export_jobs_dedup_key_key"),
        Queue::AlertEvaluations => ("alert_evaluations_pkey", "alert_evaluations_dedup_key_key"),
    }
}

/// The `status` column of a claimed row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobStatus {
    /// Waiting for a claim, or under one.
    Pending,
    /// Completed.
    Done,
    /// Parked after its attempts ran out.
    Parked,
}

/// A claimed row as the model holds it, for the oracles.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JobState {
    /// The queue.
    pub queue: Queue,
    /// The row id.
    pub id: Uuid,
    /// The payload.
    pub payload: String,
    /// The attempt counter.
    pub attempts: u32,
    /// The status.
    pub status: JobStatus,
}

/// A seeded defect of the store, for the simulations that must catch it. `None` in every other use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PgMutant {
    /// `renew` does not compare the holder, the fencing token or the expiry: it writes the caller's lease
    /// back over whatever the row holds.
    RenewWithoutCompareAndSet,
    /// `complete` does not compare the attempt counter, so a claimer whose claim ran out completes the row
    /// of a newer claim.
    CompleteWithoutClaimCheck,
}

#[derive(Clone, Debug)]
enum Row {
    Org(Org),
    User(User),
    Credential(Credential),
    Session(Session),
    Project(Project),
    OrgMember(OrgMembership),
    ProjectMember(ProjectMembership),
    ApiKey(ApiKey),
    Feed(FeedRow),
    SigningKey(SigningKey),
    Secret(Secret),
    ProjectSalt(SealedProjectSalt),
    Lease(Lease),
    Cursor(String),
    Job(JobRow),
    Ledger(LedgerEntry),
    SqlIdentity(crucible_control::SqlIdentity),
    SavedQuery(crucible_control::SavedQuery),
    SqlRun(crucible_control::SqlRun),
    AuthProvider(AuthProvider),
    OauthAccount(OauthAccount),
    AuthFlow(AuthFlow),
    OneTimeToken(OneTimeToken),
    Invitation(Invitation),
    Channel(Channel),
    AlertRule(AlertRule),
    AlertState(AlertSeriesState),
    AlertEvent(AlertEvent),
    Node(Node),
    DeletionRequest(DeletionRequest),
    DeletionAck(NodeAck),
    Action(LifecycleAction),
    Floor((ProjectId, RetentionFloor)),
}

/// The secondary unique constraints of a row, by the names of the production schema.
fn uniques(row: &Row) -> Vec<(&'static str, Key)> {
    match row {
        Row::Org(_) => vec![("org_singleton_key", Vec::new())],
        Row::User(user) => vec![("users_email_key", user.email.as_str().as_bytes().to_vec())],
        Row::Project(project) => vec![("projects_slug_key", project.slug.as_bytes().to_vec())],
        Row::ApiKey(key) => vec![(
            "api_keys_public_key_key",
            key.public_key.as_bytes().to_vec(),
        )],
        // The key is unique within its queue's table.
        Row::Job(job) => vec![(
            queue_constraints(job.queue).1,
            [
                job.queue.as_str().as_bytes(),
                &[0],
                job.dedup_key.as_bytes(),
            ]
            .concat(),
        )],
        Row::AuthProvider(provider) if provider.seeded => vec![(
            "auth_providers_seeded_kind_key",
            provider.kind.as_str().as_bytes().to_vec(),
        )],
        Row::DeletionRequest(request) => lifecycle::idempotency_key(request)
            .map(|key| ("deletion_requests_idempotency_key", key))
            .into_iter()
            .collect(),
        Row::Action(action) => vec![("lifecycle_actions_key_key", action.key.as_bytes().to_vec())],
        _ => Vec::new(),
    }
}

/// The rows a row references, by the foreign-key names of the production schema.
fn parents(row: &Row) -> Vec<(&'static str, Tbl, Key)> {
    let user = |id: UserId| id.to_bytes().to_vec();
    let project = |id: ProjectId| id.to_bytes().to_vec();
    match row {
        Row::Credential(c) => vec![("credentials_user_id_fkey", Tbl::Users, user(c.user_id))],
        Row::Session(s) => vec![("sessions_user_id_fkey", Tbl::Users, user(s.user_id))],
        Row::OrgMember(m) => vec![("org_memberships_user_id_fkey", Tbl::Users, user(m.user_id))],
        Row::ProjectMember(m) => vec![
            (
                "project_memberships_project_id_fkey",
                Tbl::Projects,
                project(m.project_id),
            ),
            (
                "project_memberships_user_id_fkey",
                Tbl::Users,
                user(m.user_id),
            ),
        ],
        Row::ApiKey(k) => vec![(
            "api_keys_project_id_fkey",
            Tbl::Projects,
            project(k.project_id),
        )],
        Row::Channel(c) => vec![(
            "channels_project_id_fkey",
            Tbl::Projects,
            project(c.project_id),
        )],
        Row::AlertRule(r) => vec![(
            "alert_rules_project_id_fkey",
            Tbl::Projects,
            project(r.project_id),
        )],
        Row::AlertState(s) => vec![(
            "alert_state_rule_id_fkey",
            Tbl::AlertRules,
            s.rule_id.to_bytes().to_vec(),
        )],
        Row::AlertEvent(e) => vec![(
            "alert_events_rule_id_fkey",
            Tbl::AlertRules,
            e.rule_id.to_bytes().to_vec(),
        )],
        Row::Job(JobRow {
            export: Some(export),
            ..
        }) => vec![(
            "export_jobs_project_id_fkey",
            Tbl::Projects,
            project(export.new.project_id),
        )],
        Row::SqlIdentity(i) => vec![(
            "ch_sql_identities_project_id_fkey",
            Tbl::Projects,
            project(i.project_id),
        )],
        Row::SavedQuery(q) => vec![(
            "saved_queries_project_id_fkey",
            Tbl::Projects,
            project(q.project_id),
        )],
        Row::SqlRun(r) => vec![(
            "sql_runs_project_id_fkey",
            Tbl::Projects,
            project(r.project_id),
        )],
        Row::OauthAccount(a) => vec![
            (
                "oauth_accounts_provider_id_fkey",
                Tbl::AuthProviders,
                a.provider_id.to_bytes().to_vec(),
            ),
            ("oauth_accounts_user_id_fkey", Tbl::Users, user(a.user_id)),
        ],
        Row::DeletionRequest(r) => vec![(
            "deletion_requests_project_id_fkey",
            Tbl::Projects,
            project(r.project_id),
        )],
        Row::DeletionAck(a) => vec![(
            "deletion_node_acks_request_id_fkey",
            Tbl::DeletionRequests,
            a.request_id.to_bytes().to_vec(),
        )],
        Row::Floor((id, _)) => vec![(
            "retention_floors_project_id_fkey",
            Tbl::Projects,
            project(*id),
        )],
        _ => Vec::new(),
    }
}

/// The tables whose rows are deleted with a row of `table`.
fn cascades(table: Tbl) -> &'static [Tbl] {
    match table {
        Tbl::Users => &[
            Tbl::Credentials,
            Tbl::Sessions,
            Tbl::OrgMembers,
            Tbl::ProjectMembers,
            Tbl::OauthAccounts,
        ],
        Tbl::AuthProviders => &[Tbl::OauthAccounts],
        Tbl::AlertRules => &[Tbl::AlertState, Tbl::AlertEvents],
        _ => &[],
    }
}

#[derive(Debug, Default)]
struct Slot {
    val: Option<Row>,
    /// The commit that last changed the row.
    changed_at: u64,
    excl: Option<u64>,
    shared: BTreeSet<u64>,
}

#[derive(Debug)]
struct Snapshot {
    seq: u64,
    rows: BTreeMap<(Tbl, Key), Row>,
}

/// The state of one transaction, or of one single statement (no snapshot: it reads the latest rows).
#[derive(Debug)]
struct TxState {
    id: u64,
    generation: u64,
    begun: Instant,
    snapshot: Option<Snapshot>,
    writes: BTreeMap<(Tbl, Key), Option<Row>>,
    shares: BTreeSet<(Tbl, Key)>,
    uniques: Vec<(&'static str, Key)>,
    failed: Option<TxError>,
    done: bool,
}

#[derive(Debug)]
struct Db {
    rows: BTreeMap<(Tbl, Key), Slot>,
    pending_unique: BTreeMap<(&'static str, Key), u64>,
    commit_seq: u64,
    next_tx: u64,
    feed_seq: u64,
    deletion_seq: u64,
    /// Increases at every failover; a transaction of an older generation is lost.
    generation: u64,
    /// Monotonic nanoseconds until which every call fails.
    outage_until: u64,
    db_offset_micros: i64,
    faults: PgFaults,
    events: Vec<LeaseEvent>,
    mutant: Option<PgMutant>,
}

/// Why a statement stopped.
#[derive(Debug)]
enum Halt {
    /// A row is locked by another transaction: a single statement waits, a transaction fails.
    Blocked,
    Error(TxError),
}

impl From<TxError> for Halt {
    fn from(error: TxError) -> Self {
        Self::Error(error)
    }
}

type Step<T> = Result<T, Halt>;

impl Db {
    fn new_tx(&mut self, begun: Instant) -> TxState {
        self.next_tx += 1;
        TxState {
            id: self.next_tx,
            generation: self.generation,
            begun,
            snapshot: None,
            writes: BTreeMap::new(),
            shares: BTreeSet::new(),
            uniques: Vec::new(),
            failed: None,
            done: false,
        }
    }

    fn release(&mut self, tx: &mut TxState) {
        for at in tx.writes.keys().chain(&tx.shares) {
            if let Some(slot) = self.rows.get_mut(at) {
                if slot.excl == Some(tx.id) {
                    slot.excl = None;
                }
                slot.shared.remove(&tx.id);
            }
        }
        for unique in tx.uniques.drain(..) {
            if self.pending_unique.get(&unique) == Some(&tx.id) {
                self.pending_unique.remove(&unique);
            }
        }
        tx.writes.clear();
        tx.shares.clear();
        tx.done = true;
    }

    fn commit(&mut self, tx: &mut TxState) {
        if !tx.writes.is_empty() {
            self.commit_seq += 1;
        }
        let writes = std::mem::take(&mut tx.writes);
        for (at, val) in &writes {
            let slot = self.rows.entry(at.clone()).or_default();
            slot.val = val.clone();
            slot.changed_at = self.commit_seq;
        }
        for ((table, key), val) in &writes {
            match (table, val) {
                (Tbl::Leases, Some(Row::Lease(lease))) => {
                    self.events.push(LeaseEvent::Lease(lease.clone()));
                }
                (Tbl::Cursors, Some(Row::Cursor(cursor))) => {
                    if let Some(Row::Lease(lease)) = self
                        .rows
                        .get(&(Tbl::Leases, key.clone()))
                        .and_then(|s| s.val.as_ref())
                    {
                        self.events.push(LeaseEvent::Cursor {
                            name: lease.name,
                            cursor: cursor.clone(),
                            token: lease.fencing_token,
                            holder: lease.holder.clone(),
                        });
                    }
                }
                _ => {}
            }
        }
        tx.writes = writes;
        self.release(tx);
    }

    /// Loses every open transaction and fails every call until `until`.
    fn fail_over(&mut self, until: u64) {
        self.generation += 1;
        self.outage_until = self.outage_until.max(until);
        self.pending_unique.clear();
        for slot in self.rows.values_mut() {
            slot.excl = None;
            slot.shared.clear();
        }
    }
}

/// One statement's access to the rows.
struct Cx<'a> {
    db: &'a mut Db,
    tx: &'a mut TxState,
    /// The database clock at the statement.
    now: Timestamp,
}

impl Cx<'_> {
    /// The row this statement sees: its own writes, then the snapshot or the latest committed row.
    fn get(&self, table: Tbl, key: &[u8]) -> Option<Row> {
        let at = (table, key.to_vec());
        if let Some(own) = self.tx.writes.get(&at) {
            return own.clone();
        }
        match &self.tx.snapshot {
            Some(snapshot) => snapshot.rows.get(&at).cloned(),
            None => self.db.rows.get(&at).and_then(|slot| slot.val.clone()),
        }
    }

    /// The latest committed row, or this statement's own write.
    fn latest(&self, table: Tbl, key: &[u8]) -> Option<Row> {
        let at = (table, key.to_vec());
        if let Some(own) = self.tx.writes.get(&at) {
            return own.clone();
        }
        self.db.rows.get(&at).and_then(|slot| slot.val.clone())
    }

    /// Every row of `table` this statement sees, in key order.
    fn scan(&self, table: Tbl) -> Vec<(Key, Row)> {
        let from = (table, Vec::new());
        let mut rows: BTreeMap<Key, Row> = match &self.tx.snapshot {
            Some(snapshot) => snapshot
                .rows
                .range(from.clone()..)
                .take_while(|((t, _), _)| *t == table)
                .map(|((_, key), row)| (key.clone(), row.clone()))
                .collect(),
            None => self.latest_rows(table),
        };
        for ((_, key), own) in self
            .tx
            .writes
            .range(from..)
            .take_while(|((t, _), _)| *t == table)
        {
            match own {
                Some(row) => rows.insert(key.clone(), row.clone()),
                None => rows.remove(key),
            };
        }
        rows.into_iter().collect()
    }

    fn latest_rows(&self, table: Tbl) -> BTreeMap<Key, Row> {
        self.db
            .rows
            .range((table, Vec::new())..)
            .take_while(|((t, _), _)| *t == table)
            .filter_map(|((_, key), slot)| Some((key.clone(), slot.val.clone()?)))
            .collect()
    }

    fn locked_by_other(&self, table: Tbl, key: &[u8]) -> bool {
        self.db
            .rows
            .get(&(table, key.to_vec()))
            .is_some_and(|slot| {
                slot.excl.is_some_and(|holder| holder != self.tx.id)
                    || slot.shared.iter().any(|holder| *holder != self.tx.id)
            })
    }

    /// Whether the row changed after this transaction's snapshot.
    fn changed_since_snapshot(&self, table: Tbl, key: &[u8]) -> bool {
        let Some(snapshot) = &self.tx.snapshot else {
            return false;
        };
        self.db
            .rows
            .get(&(table, key.to_vec()))
            .is_some_and(|slot| slot.changed_at > snapshot.seq)
    }

    fn check_writable(&self, table: Tbl, key: &[u8]) -> Step<()> {
        if self.locked_by_other(table, key) {
            return Err(Halt::Blocked);
        }
        if self.changed_since_snapshot(table, key) {
            return Err(TxError::SerializationFailure.into());
        }
        Ok(())
    }

    /// A shared lock, as `FOR SHARE` and a foreign-key check take it.
    fn share(&mut self, table: Tbl, key: &[u8]) -> Step<()> {
        let at = (table, key.to_vec());
        let slot = self.db.rows.entry(at.clone()).or_default();
        if slot.excl.is_some_and(|holder| holder != self.tx.id) {
            return Err(Halt::Blocked);
        }
        if slot.excl.is_none() {
            slot.shared.insert(self.tx.id);
            self.tx.shares.insert(at);
        }
        Ok(())
    }

    /// Inserts a row under its primary key.
    fn insert(&mut self, table: Tbl, key: Key, row: Row, pkey: &'static str) -> Step<()> {
        if self.locked_by_other(table, &key) {
            return Err(Halt::Blocked);
        }
        if self.latest(table, &key).is_some() {
            return Err(TxError::UniqueViolation(pkey.to_owned()).into());
        }
        self.put(table, key, Some(row))
    }

    /// Writes or deletes a row, with the lock, snapshot, unique and foreign-key checks.
    fn put(&mut self, table: Tbl, key: Key, val: Option<Row>) -> Step<()> {
        self.check_writable(table, &key)?;
        match &val {
            Some(row) => {
                for (name, unique) in uniques(row) {
                    let pending = (name, unique.clone());
                    if self
                        .db
                        .pending_unique
                        .get(&pending)
                        .is_some_and(|holder| *holder != self.tx.id)
                    {
                        return Err(Halt::Blocked);
                    }
                    let mut others = self.latest_rows(table);
                    for ((_, other), own) in self
                        .tx
                        .writes
                        .range((table, Vec::new())..)
                        .take_while(|((t, _), _)| *t == table)
                    {
                        match own {
                            Some(row) => others.insert(other.clone(), row.clone()),
                            None => others.remove(other),
                        };
                    }
                    let taken = others.iter().any(|(other, row)| {
                        *other != key
                            && uniques(row).iter().any(|(n, u)| *n == name && *u == unique)
                    });
                    if taken {
                        return Err(TxError::UniqueViolation(name.to_owned()).into());
                    }
                    self.db.pending_unique.insert(pending.clone(), self.tx.id);
                    self.tx.uniques.push(pending);
                }
                for (name, parent, parent_key) in parents(row) {
                    let seen = self.get(parent, &parent_key).is_some();
                    let latest = self.latest(parent, &parent_key).is_some();
                    match (seen, latest) {
                        (true, true) => self.share(parent, &parent_key)?,
                        (false, false) => {
                            return Err(TxError::ForeignKeyViolation(name.to_owned()).into());
                        }
                        _ => return Err(TxError::SerializationFailure.into()),
                    }
                }
            }
            None => {
                for child in cascades(table) {
                    let refers = |row: &Row| {
                        parents(row)
                            .iter()
                            .any(|(_, parent, parent_key)| *parent == table && *parent_key == key)
                    };
                    // A referencing row this transaction cannot see is a conflict with its writer.
                    let unseen = self
                        .latest_rows(*child)
                        .iter()
                        .any(|(k, row)| refers(row) && self.changed_since_snapshot(*child, k));
                    if unseen {
                        return Err(TxError::SerializationFailure.into());
                    }
                    for (child_key, row) in self.scan(*child) {
                        if refers(&row) {
                            self.put(*child, child_key, None)?;
                        }
                    }
                }
            }
        }
        let at = (table, key);
        self.db.rows.entry(at.clone()).or_default().excl = Some(self.tx.id);
        self.tx.writes.insert(at, val);
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn ppm(rng: &dyn Rng, rate: u32) -> bool {
    rate > 0 && rng.next_u64() % 1_000_000 < u64::from(rate)
}

fn between(rng: &dyn Rng, (low, high): (u64, u64)) -> u64 {
    low + rng.next_u64() % (high.saturating_sub(low) + 1)
}

fn unavailable(what: &str) -> TxError {
    TxError::Unavailable(what.to_owned())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pool {
    Main,
    Reserved,
}

struct Shared {
    clock: Arc<dyn Clock>,
    db: Mutex<Db>,
    main: Arc<Semaphore>,
    reserved: Arc<Semaphore>,
    fault_rng: Arc<dyn Rng>,
    latency_rng: Arc<dyn Rng>,
}

impl Shared {
    fn db_now(&self, db: &Db) -> Timestamp {
        self.clock
            .wall_now()
            .saturating_add_micros(db.db_offset_micros)
    }

    async fn acquire(&self, pool: Pool) -> Result<OwnedSemaphorePermit, TxError> {
        let (semaphore, name) = match pool {
            Pool::Main => (&self.main, "the main pool is exhausted"),
            Pool::Reserved => (&self.reserved, "the reserved pool is exhausted"),
        };
        let wait = lock(&self.db).faults.acquire_timeout;
        match self
            .clock
            .timeout(wait, Arc::clone(semaphore).acquire_owned())
            .await
        {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(unavailable(name)),
        }
    }

    async fn delay(&self) {
        let range = lock(&self.db).faults.latency_micros;
        let micros = between(self.latency_rng.as_ref(), range);
        if micros > 0 {
            self.clock.sleep(Duration::from_micros(micros)).await;
        }
    }

    /// The faults every call can meet before it runs. `Err` when the call fails without running.
    fn gate(&self, db: &mut Db) -> Result<(), TxError> {
        let now = self.clock.now().nanos();
        if ppm(self.fault_rng.as_ref(), db.faults.failover_ppm) {
            let micros = between(self.fault_rng.as_ref(), db.faults.failover_micros);
            db.fail_over(now.saturating_add(micros.saturating_mul(1_000)));
        }
        if now < db.outage_until {
            return Err(unavailable("postgres is failing over"));
        }
        if ppm(self.fault_rng.as_ref(), db.faults.unavailable_ppm) {
            return Err(unavailable("the connection was lost"));
        }
        Ok(())
    }
}

/// A single statement that has run and not yet committed. Dropping it rolls back.
struct Pending<'a> {
    shared: &'a Shared,
    tx: Option<TxState>,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(mut tx) = self.tx.take() {
            lock(&self.shared.db).release(&mut tx);
        }
    }
}

/// Runs statements: each as its own commit (`tx` is `None`), or inside one transaction.
struct Runner {
    shared: Arc<Shared>,
    tx: Option<Mutex<TxState>>,
}

impl Runner {
    async fn run<T: Send>(
        &self,
        pool: Pool,
        writes: bool,
        op: impl Fn(&mut Cx<'_>) -> Step<T> + Send + Sync,
    ) -> Result<T, TxError> {
        match &self.tx {
            Some(tx) => self.run_in_tx(tx, op).await,
            None => self.run_single(pool, writes, op).await,
        }
    }

    async fn run_single<T: Send>(
        &self,
        pool: Pool,
        writes: bool,
        op: impl Fn(&mut Cx<'_>) -> Step<T> + Send + Sync,
    ) -> Result<T, TxError> {
        let shared = &*self.shared;
        let _connection = shared.acquire(pool).await?;
        shared.delay().await;
        let (unknown, slow) = {
            let mut db = lock(&shared.db);
            shared.gate(&mut db)?;
            let rng = shared.fault_rng.as_ref();
            let unknown = writes && ppm(rng, db.faults.commit_unknown_ppm);
            if unknown && rng.next_u64() % 2 == 0 {
                return Err(TxError::CommitUnknown);
            }
            let slow = (writes && ppm(rng, db.faults.slow_commit_ppm))
                .then(|| between(rng, db.faults.slow_commit_micros));
            (unknown, slow)
        };
        let deadline = shared.clock.now().saturating_add(STATEMENT_TIMEOUT);
        loop {
            let mut pending = Pending { shared, tx: None };
            let step = {
                let mut db = lock(&shared.db);
                let mut tx = db.new_tx(shared.clock.now());
                let now = shared.db_now(&db);
                let step = op(&mut Cx {
                    db: &mut db,
                    tx: &mut tx,
                    now,
                });
                match (&step, slow) {
                    (Ok(_), Some(_)) => pending.tx = Some(tx),
                    (Ok(_), None) => db.commit(&mut tx),
                    (Err(_), _) => db.release(&mut tx),
                }
                step
            };
            match step {
                Ok(value) => {
                    if let Some(micros) = slow {
                        shared.clock.sleep(Duration::from_micros(micros)).await;
                        let mut db = lock(&shared.db);
                        if let Some(mut tx) = pending.tx.take() {
                            if tx.generation != db.generation {
                                db.release(&mut tx);
                                return Err(TxError::CommitUnknown);
                            }
                            db.commit(&mut tx);
                        }
                    }
                    return if unknown {
                        Err(TxError::CommitUnknown)
                    } else {
                        Ok(value)
                    };
                }
                Err(Halt::Error(error)) => return Err(error),
                Err(Halt::Blocked) => {
                    if shared.clock.now() >= deadline {
                        return Err(unavailable(
                            "statement timeout while waiting for a row lock",
                        ));
                    }
                    shared.clock.sleep(LOCK_POLL).await;
                }
            }
        }
    }

    async fn run_in_tx<T: Send>(
        &self,
        tx: &Mutex<TxState>,
        op: impl Fn(&mut Cx<'_>) -> Step<T> + Send + Sync,
    ) -> Result<T, TxError> {
        let shared = &*self.shared;
        shared.delay().await;
        let mut tx = lock(tx);
        let mut db = lock(&shared.db);
        Self::usable(shared, &mut db, &mut tx)?;
        let outcome = if ppm(
            shared.fault_rng.as_ref(),
            db.faults.spurious_serialization_ppm,
        ) {
            Err(TxError::SerializationFailure)
        } else {
            if tx.snapshot.is_none() {
                let rows = db
                    .rows
                    .iter()
                    .filter_map(|(at, slot)| Some((at.clone(), slot.val.clone()?)))
                    .collect();
                tx.snapshot = Some(Snapshot {
                    seq: db.commit_seq,
                    rows,
                });
            }
            let now = shared.db_now(&db);
            match op(&mut Cx {
                db: &mut db,
                tx: &mut tx,
                now,
            }) {
                Ok(value) => Ok(value),
                Err(Halt::Blocked) => Err(TxError::SerializationFailure),
                Err(Halt::Error(error)) => Err(error),
            }
        };
        if let Err(error) = &outcome {
            tx.failed = Some(error.clone());
            db.release(&mut tx);
        }
        outcome
    }

    /// Whether the transaction can still run a statement or commit; fails it when it cannot.
    fn usable(shared: &Shared, db: &mut Db, tx: &mut TxState) -> Result<(), TxError> {
        if let Some(error) = &tx.failed {
            return Err(error.clone());
        }
        let lost = if tx.done {
            Some(unavailable("the transaction has ended"))
        } else if tx.generation != db.generation {
            Some(unavailable("the connection was lost in a failover"))
        } else if shared.clock.now().saturating_duration_since(tx.begun) >= TRANSACTION_CAP {
            Some(unavailable("the transaction exceeded its 10 s cap"))
        } else {
            shared.gate(db).err()
        };
        // A failover that `gate` started has already taken this transaction's locks.
        match lost {
            Some(error) => {
                tx.failed = Some(error.clone());
                db.release(tx);
                Err(error)
            }
            None => Ok(()),
        }
    }
}

/// The simulated control store. Clones share one database.
#[derive(Clone)]
pub struct PgModel {
    runner: Arc<Runner>,
}

impl fmt::Debug for PgModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PgModel")
    }
}

impl PgModel {
    /// An empty database with the production schema's tables. `clock` is the database host's clock; `rng`
    /// seeds the fault streams.
    pub fn new(clock: Arc<dyn Clock>, rng: &dyn Rng, faults: PgFaults) -> Self {
        let shared = Shared {
            clock,
            main: Arc::new(Semaphore::new(faults.main_pool)),
            reserved: Arc::new(Semaphore::new(faults.reserved_pool)),
            fault_rng: rng.fork("pg-model/faults"),
            latency_rng: rng.fork("pg-model/latency"),
            db: Mutex::new(Db {
                rows: BTreeMap::new(),
                pending_unique: BTreeMap::new(),
                commit_seq: 0,
                next_tx: 0,
                feed_seq: 0,
                deletion_seq: 0,
                generation: 0,
                outage_until: 0,
                db_offset_micros: 0,
                faults,
                events: Vec::new(),
                mutant: None,
            }),
        };
        Self {
            runner: Arc::new(Runner {
                shared: Arc::new(shared),
                tx: None,
            }),
        }
    }

    fn shared(&self) -> &Shared {
        &self.runner.shared
    }

    /// Replaces the fault rates. Pool sizes are fixed at construction.
    pub fn set_faults(&self, faults: PgFaults) {
        lock(&self.shared().db).faults = faults;
    }

    /// Steps the database clock against the model's clock, forward or backward.
    pub fn step_db_clock(&self, micros: i64) {
        lock(&self.shared().db).db_offset_micros += micros;
    }

    /// Starts a failover: every open transaction is lost and every call fails for `outage`. Afterwards every
    /// acknowledged commit is still there.
    pub fn fail_over(&self, outage: Duration) {
        let until = self.shared().clock.now().saturating_add(outage).nanos();
        lock(&self.shared().db).fail_over(until);
    }

    /// The lease and job-cursor writes committed so far, in commit order.
    pub fn lease_events(&self) -> Vec<LeaseEvent> {
        lock(&self.shared().db).events.clone()
    }

    /// Switches a seeded defect on or off.
    pub fn set_mutant(&self, mutant: Option<PgMutant>) {
        lock(&self.shared().db).mutant = mutant;
    }

    /// Takes the lease away from whoever holds it, live or not, as an operator's forced expiry followed by an
    /// acquisition would: the row gets `holder`, the next fencing token and an expiry `ttl` from the
    /// database clock. No fault applies. `None` while a statement or a transaction holds a lock on the row,
    /// which the real statement would wait for.
    pub fn steal_lease(&self, name: LeaseName, holder: &str, ttl: Duration) -> Option<Lease> {
        let shared = self.shared();
        let mut db = lock(&shared.db);
        let at = (Tbl::Leases, lease_key(name));
        let slot = db.rows.get(&at);
        if slot.is_some_and(|slot| slot.excl.is_some() || !slot.shared.is_empty()) {
            return None;
        }
        let token = match slot.and_then(|slot| slot.val.as_ref()) {
            Some(Row::Lease(row)) => row.fencing_token,
            _ => 0,
        };
        let lease = Lease {
            name,
            holder: holder.to_owned(),
            fencing_token: token + 1,
            expires_at: after(shared.db_now(&db), ttl),
        };
        db.commit_seq += 1;
        let seq = db.commit_seq;
        let slot = db.rows.entry(at).or_default();
        slot.val = Some(Row::Lease(lease.clone()));
        slot.changed_at = seq;
        db.events.push(LeaseEvent::Lease(lease.clone()));
        Some(lease)
    }

    /// Whether the committed lease row still carries this holder and fencing token and has not expired on
    /// the database clock.
    pub fn lease_is_live(&self, lease: &Lease) -> bool {
        let shared = self.shared();
        let db = lock(&shared.db);
        let now = shared.db_now(&db);
        match db
            .rows
            .get(&(Tbl::Leases, lease_key(lease.name)))
            .and_then(|slot| slot.val.as_ref())
        {
            Some(Row::Lease(row)) => {
                row.holder == lease.holder
                    && row.fencing_token == lease.fencing_token
                    && row.expires_at >= now
            }
            _ => false,
        }
    }

    /// Every committed claimed row, in key order.
    pub fn job_states(&self) -> Vec<JobState> {
        let db = lock(&self.shared().db);
        db.rows
            .values()
            .filter_map(|slot| match &slot.val {
                Some(Row::Job(job)) => Some(JobState {
                    queue: job.queue,
                    id: job.id,
                    payload: job.payload.clone(),
                    attempts: job.attempts,
                    status: job.status,
                }),
                _ => None,
            })
            .collect()
    }

    /// How many key-feed rows are committed.
    pub fn feed_len(&self) -> usize {
        let db = lock(&self.shared().db);
        db.rows
            .values()
            .filter(|slot| matches!(slot.val, Some(Row::Feed(_))))
            .count()
    }
}

/// One open transaction of the model.
pub struct ModelTx {
    runner: Runner,
    _connection: OwnedSemaphorePermit,
}

impl fmt::Debug for ModelTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ModelTx")
    }
}

impl Drop for ModelTx {
    fn drop(&mut self) {
        if let Some(tx) = &self.runner.tx {
            let mut tx = lock(tx);
            if !tx.done {
                lock(&self.runner.shared.db).release(&mut tx);
            }
        }
    }
}

#[async_trait]
impl ControlStore for PgModel {
    async fn begin(&self) -> Result<Box<dyn ControlTx>, TxError> {
        let shared = self.shared();
        let connection = shared.acquire(Pool::Main).await?;
        shared.delay().await;
        let mut db = lock(&shared.db);
        shared.gate(&mut db)?;
        let tx = db.new_tx(shared.clock.now());
        Ok(Box::new(ModelTx {
            runner: Runner {
                shared: Arc::clone(&self.runner.shared),
                tx: Some(Mutex::new(tx)),
            },
            _connection: connection,
        }))
    }

    async fn db_now(&self) -> Result<Timestamp, TxError> {
        self.runner
            .run(Pool::Reserved, false, |cx| Ok(cx.now))
            .await
    }
}

#[async_trait]
impl ControlTx for ModelTx {
    async fn fence(&self, lease: &Lease) -> Result<(), TxError> {
        self.runner
            .run(Pool::Main, false, |cx| fence(cx, lease))
            .await
    }

    async fn commit(self: Box<Self>) -> Result<(), TxError> {
        let shared = Arc::clone(&self.runner.shared);
        shared.delay().await;
        let Some(tx) = &self.runner.tx else {
            return Err(unavailable("the transaction has ended"));
        };
        let mut tx = lock(tx);
        let mut db = lock(&shared.db);
        Runner::usable(&shared, &mut db, &mut tx)?;
        let rng = shared.fault_rng.as_ref();
        if ppm(rng, db.faults.commit_unknown_ppm) {
            if rng.next_u64() % 2 == 0 {
                db.commit(&mut tx);
            } else {
                db.release(&mut tx);
            }
            return Err(TxError::CommitUnknown);
        }
        db.commit(&mut tx);
        Ok(())
    }

    async fn rollback(self: Box<Self>) {}
}

macro_rules! repos {
    ($ty:ty) => {
        impl Repos for $ty {
            fn org(&self) -> &dyn OrgRepo {
                &self.runner
            }
            fn users(&self) -> &dyn UserRepo {
                &self.runner
            }
            fn credentials(&self) -> &dyn CredentialRepo {
                &self.runner
            }
            fn sessions(&self) -> &dyn SessionRepo {
                &self.runner
            }
            fn projects(&self) -> &dyn ProjectRepo {
                &self.runner
            }
            fn memberships(&self) -> &dyn MembershipRepo {
                &self.runner
            }
            fn api_keys(&self) -> &dyn ApiKeyRepo {
                &self.runner
            }
            fn key_feed(&self) -> &dyn KeyFeed {
                &self.runner
            }
            fn signing_keys(&self) -> &dyn SigningKeyRepo {
                &self.runner
            }
            fn secrets(&self) -> &dyn SecretRepo {
                &self.runner
            }
            fn project_salts(&self) -> &dyn ProjectSaltRepo {
                &self.runner
            }
            fn auth_providers(&self) -> &dyn AuthProviderRepo {
                &self.runner
            }
            fn oauth_accounts(&self) -> &dyn OauthAccountRepo {
                &self.runner
            }
            fn auth_flows(&self) -> &dyn AuthFlowRepo {
                &self.runner
            }
            fn one_time_tokens(&self) -> &dyn OneTimeTokenRepo {
                &self.runner
            }
            fn invitations(&self) -> &dyn InvitationRepo {
                &self.runner
            }
            fn channels(&self) -> &dyn ChannelRepo {
                &self.runner
            }
            fn alert_rules(&self) -> &dyn AlertRuleRepo {
                &self.runner
            }
            fn alert_state(&self) -> &dyn AlertStateRepo {
                &self.runner
            }
            fn alert_events(&self) -> &dyn AlertEventRepo {
                &self.runner
            }
            fn leases(&self) -> &dyn Leases {
                &self.runner
            }
            fn claims(&self) -> &dyn Claims {
                &self.runner
            }
            fn export_jobs(&self) -> &dyn ExportJobRepo {
                &self.runner
            }
            fn migrations(&self) -> &dyn Migrations {
                &self.runner
            }
            fn lifecycle(&self) -> &dyn LifecycleRepo {
                &self.runner
            }
            fn sql_identities(&self) -> &dyn crucible_control::SqlIdentityRepo {
                &self.runner
            }
            fn saved_queries(&self) -> &dyn crucible_control::SavedQueryRepo {
                &self.runner
            }
            fn sql_runs(&self) -> &dyn crucible_control::SqlRunRepo {
                &self.runner
            }
        }
    };
}

repos!(ModelTx);

impl Repos for PgModel {
    fn org(&self) -> &dyn OrgRepo {
        &*self.runner
    }
    fn users(&self) -> &dyn UserRepo {
        &*self.runner
    }
    fn credentials(&self) -> &dyn CredentialRepo {
        &*self.runner
    }
    fn sessions(&self) -> &dyn SessionRepo {
        &*self.runner
    }
    fn projects(&self) -> &dyn ProjectRepo {
        &*self.runner
    }
    fn memberships(&self) -> &dyn MembershipRepo {
        &*self.runner
    }
    fn api_keys(&self) -> &dyn ApiKeyRepo {
        &*self.runner
    }
    fn key_feed(&self) -> &dyn KeyFeed {
        &*self.runner
    }
    fn signing_keys(&self) -> &dyn SigningKeyRepo {
        &*self.runner
    }
    fn secrets(&self) -> &dyn SecretRepo {
        &*self.runner
    }
    fn project_salts(&self) -> &dyn ProjectSaltRepo {
        &*self.runner
    }
    fn auth_providers(&self) -> &dyn AuthProviderRepo {
        &*self.runner
    }
    fn oauth_accounts(&self) -> &dyn OauthAccountRepo {
        &*self.runner
    }
    fn auth_flows(&self) -> &dyn AuthFlowRepo {
        &*self.runner
    }
    fn one_time_tokens(&self) -> &dyn OneTimeTokenRepo {
        &*self.runner
    }
    fn invitations(&self) -> &dyn InvitationRepo {
        &*self.runner
    }
    fn channels(&self) -> &dyn ChannelRepo {
        &*self.runner
    }
    fn alert_rules(&self) -> &dyn AlertRuleRepo {
        &*self.runner
    }
    fn alert_state(&self) -> &dyn AlertStateRepo {
        &*self.runner
    }
    fn alert_events(&self) -> &dyn AlertEventRepo {
        &*self.runner
    }
    fn leases(&self) -> &dyn Leases {
        &*self.runner
    }
    fn claims(&self) -> &dyn Claims {
        &*self.runner
    }
    fn export_jobs(&self) -> &dyn ExportJobRepo {
        &*self.runner
    }
    fn migrations(&self) -> &dyn Migrations {
        &*self.runner
    }
    fn lifecycle(&self) -> &dyn LifecycleRepo {
        &*self.runner
    }
    fn sql_identities(&self) -> &dyn crucible_control::SqlIdentityRepo {
        &*self.runner
    }
    fn saved_queries(&self) -> &dyn crucible_control::SavedQueryRepo {
        &*self.runner
    }
    fn sql_runs(&self) -> &dyn crucible_control::SqlRunRepo {
        &*self.runner
    }
}

/// Reads one row of a known variant.
macro_rules! one {
    ($cx:expr, $table:expr, $key:expr, $variant:path) => {
        match $cx.get($table, $key) {
            Some($variant(row)) => Some(row),
            _ => None,
        }
    };
}

/// Reads every row of a table of a known variant, in key order.
macro_rules! all {
    ($cx:expr, $table:expr, $variant:path) => {
        $cx.scan($table)
            .into_iter()
            .filter_map(|(_, row)| match row {
                $variant(row) => Some(row),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
}

mod alerts;
mod exports;
mod identity;
mod lifecycle;
/// The SQL workspace's repositories. Declared after the macros above, which it uses.
mod sqlws;

fn uuid_key(bytes: [u8; 16]) -> Key {
    bytes.to_vec()
}

fn member_key(project: ProjectId, user: UserId) -> Key {
    [project.to_bytes(), user.to_bytes()].concat()
}

fn lease_key(name: LeaseName) -> Key {
    name.as_str().as_bytes().to_vec()
}

fn job_key(queue: Queue, id: Uuid) -> Key {
    [queue.as_str().as_bytes(), &id.to_bytes()].concat()
}

fn after(now: Timestamp, d: Duration) -> Timestamp {
    now.saturating_add_micros(duration_micros(d))
}

fn fence(cx: &mut Cx<'_>, lease: &Lease) -> Step<()> {
    let key = lease_key(lease.name);
    match one!(cx, Tbl::Leases, &key, Row::Lease) {
        Some(row) if row.holder == lease.holder && row.fencing_token == lease.fencing_token => {
            if cx.changed_since_snapshot(Tbl::Leases, &key) {
                return Err(TxError::SerializationFailure.into());
            }
            cx.share(Tbl::Leases, &key)
        }
        _ => Err(TxError::Fenced.into()),
    }
}

#[async_trait]
impl OrgRepo for Runner {
    async fn get(&self) -> Result<Option<Org>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::Org, Row::Org).into_iter().next())
        })
        .await
    }

    async fn insert(&self, org: &Org) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Org,
                uuid_key(org.id.to_bytes()),
                Row::Org(org.clone()),
                "org_pkey",
            )
        })
        .await
    }

    async fn update_if_version(&self, org: &Org, expected: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(org.id.to_bytes());
            match one!(cx, Tbl::Org, &key, Row::Org) {
                Some(stored) if stored.version == expected => {
                    let mut next = org.clone();
                    next.version = expected + 1;
                    cx.put(Tbl::Org, key, Some(Row::Org(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }
}

#[async_trait]
impl UserRepo for Runner {
    async fn get(&self, id: UserId) -> Result<Option<User>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(cx, Tbl::Users, &uuid_key(id.to_bytes()), Row::User))
        })
        .await
    }

    async fn get_by_email(&self, email: &str) -> Result<Option<User>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::Users, Row::User)
                .into_iter()
                .find(|user| user.email.as_str() == email))
        })
        .await
    }

    async fn list(&self) -> Result<Vec<User>, TxError> {
        self.run(Pool::Main, false, |cx| Ok(all!(cx, Tbl::Users, Row::User)))
            .await
    }

    async fn insert(&self, user: &User) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Users,
                uuid_key(user.id.to_bytes()),
                Row::User(user.clone()),
                "users_pkey",
            )
        })
        .await
    }

    async fn update(&self, user: &User) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(user.id.to_bytes());
            if cx.get(Tbl::Users, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::Users, key, Some(Row::User(user.clone())))?;
            Ok(true)
        })
        .await
    }

    async fn delete(&self, id: UserId) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            if cx.get(Tbl::Users, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::Users, key, None)?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl CredentialRepo for Runner {
    async fn get(&self, user_id: UserId) -> Result<Option<Credential>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::Credentials,
                &uuid_key(user_id.to_bytes()),
                Row::Credential
            ))
        })
        .await
    }

    async fn put(&self, credential: &Credential) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.put(
                Tbl::Credentials,
                uuid_key(credential.user_id.to_bytes()),
                Some(Row::Credential(credential.clone())),
            )
        })
        .await
    }

    async fn delete(&self, user_id: UserId) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(user_id.to_bytes());
            if cx.get(Tbl::Credentials, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::Credentials, key, None)?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl SessionRepo for Runner {
    async fn get(&self, token_hash: &TokenHash) -> Result<Option<Session>, TxError> {
        self.run(Pool::Reserved, false, |cx| {
            Ok(one!(cx, Tbl::Sessions, token_hash, Row::Session))
        })
        .await
    }

    async fn insert(&self, session: &Session) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Sessions,
                session.token_hash.to_vec(),
                Row::Session(session.clone()),
                "sessions_pkey",
            )
        })
        .await
    }

    async fn touch(
        &self,
        token_hash: &TokenHash,
        idle_expires_at: Timestamp,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let Some(mut session) = one!(cx, Tbl::Sessions, token_hash, Row::Session) else {
                return Ok(false);
            };
            session.idle_expires_at = idle_expires_at;
            cx.put(
                Tbl::Sessions,
                token_hash.to_vec(),
                Some(Row::Session(session)),
            )?;
            Ok(true)
        })
        .await
    }

    async fn revoke(&self, token_hash: &TokenHash, at: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            match one!(cx, Tbl::Sessions, token_hash, Row::Session) {
                Some(mut session) if session.revoked_at.is_none() => {
                    session.revoked_at = Some(at);
                    cx.put(
                        Tbl::Sessions,
                        token_hash.to_vec(),
                        Some(Row::Session(session)),
                    )?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn revoke_user(
        &self,
        user_id: UserId,
        keep: Option<&TokenHash>,
        at: Timestamp,
    ) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut revoked = 0;
            for mut session in all!(cx, Tbl::Sessions, Row::Session) {
                if session.user_id == user_id
                    && session.revoked_at.is_none()
                    && keep != Some(&session.token_hash)
                {
                    session.revoked_at = Some(at);
                    cx.put(
                        Tbl::Sessions,
                        session.token_hash.to_vec(),
                        Some(Row::Session(session)),
                    )?;
                    revoked += 1;
                }
            }
            Ok(revoked)
        })
        .await
    }

    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut deleted = 0;
            for session in all!(cx, Tbl::Sessions, Row::Session) {
                if session.absolute_expires_at < now {
                    cx.put(Tbl::Sessions, session.token_hash.to_vec(), None)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await
    }
}

#[async_trait]
impl ProjectRepo for Runner {
    async fn get(&self, id: ProjectId) -> Result<Option<Project>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::Projects,
                &uuid_key(id.to_bytes()),
                Row::Project
            ))
        })
        .await
    }

    async fn get_by_slug(&self, slug: &str) -> Result<Option<Project>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::Projects, Row::Project)
                .into_iter()
                .find(|project| project.slug == slug))
        })
        .await
    }

    async fn list(&self) -> Result<Vec<Project>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut projects = all!(cx, Tbl::Projects, Row::Project);
            projects.retain(|project| project.deleted_at.is_none());
            Ok(projects)
        })
        .await
    }

    async fn insert(&self, project: &Project) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Projects,
                uuid_key(project.id.to_bytes()),
                Row::Project(project.clone()),
                "projects_pkey",
            )
        })
        .await
    }

    async fn update_if_version(&self, project: &Project, expected: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(project.id.to_bytes());
            match one!(cx, Tbl::Projects, &key, Row::Project) {
                Some(stored) if stored.version == expected => {
                    let mut next = project.clone();
                    next.version = expected + 1;
                    cx.put(Tbl::Projects, key, Some(Row::Project(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }
}

#[async_trait]
impl MembershipRepo for Runner {
    async fn org_role(&self, user_id: UserId) -> Result<Option<OrgRole>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::OrgMembers,
                &uuid_key(user_id.to_bytes()),
                Row::OrgMember
            )
            .map(|member| member.role))
        })
        .await
    }

    async fn list_org(&self) -> Result<Vec<OrgMembership>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::OrgMembers, Row::OrgMember))
        })
        .await
    }

    async fn put_org(&self, membership: OrgMembership) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.put(
                Tbl::OrgMembers,
                uuid_key(membership.user_id.to_bytes()),
                Some(Row::OrgMember(membership)),
            )
        })
        .await
    }

    async fn delete_org(&self, user_id: UserId) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(user_id.to_bytes());
            if cx.get(Tbl::OrgMembers, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::OrgMembers, key, None)?;
            Ok(true)
        })
        .await
    }

    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<ProjectMembership>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut members = all!(cx, Tbl::ProjectMembers, Row::ProjectMember);
            members.retain(|member| member.user_id == user_id);
            Ok(members)
        })
        .await
    }

    async fn list_project(&self, project_id: ProjectId) -> Result<Vec<ProjectMembership>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut members = all!(cx, Tbl::ProjectMembers, Row::ProjectMember);
            members.retain(|member| member.project_id == project_id);
            Ok(members)
        })
        .await
    }

    async fn put_project(&self, membership: ProjectMembership) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.put(
                Tbl::ProjectMembers,
                member_key(membership.project_id, membership.user_id),
                Some(Row::ProjectMember(membership)),
            )
        })
        .await
    }

    async fn delete_project(
        &self,
        project_id: ProjectId,
        user_id: UserId,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = member_key(project_id, user_id);
            if cx.get(Tbl::ProjectMembers, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::ProjectMembers, key, None)?;
            Ok(true)
        })
        .await
    }
}

/// Writes the feed row of a key change. The sequence number is drawn now and is not given back on rollback.
fn feed(cx: &mut Cx<'_>, key: &ApiKey, change: KeyChangeKind) -> Step<()> {
    cx.db.feed_seq += 1;
    let seq = cx.db.feed_seq;
    let row = FeedRow {
        seq,
        key_id: key.id,
        change,
        key_version: key.version,
        at: cx.now,
    };
    cx.put(Tbl::Feed, seq.to_be_bytes().to_vec(), Some(Row::Feed(row)))
}

#[async_trait]
impl ApiKeyRepo for Runner {
    async fn get(&self, id: ApiKeyId) -> Result<Option<ApiKey>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::ApiKeys,
                &uuid_key(id.to_bytes()),
                Row::ApiKey
            ))
        })
        .await
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<ApiKey>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut keys = all!(cx, Tbl::ApiKeys, Row::ApiKey);
            keys.retain(|key| key.project_id == project_id);
            Ok(keys)
        })
        .await
    }

    async fn insert(&self, key: &ApiKey) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::ApiKeys,
                uuid_key(key.id.to_bytes()),
                Row::ApiKey(key.clone()),
                "api_keys_pkey",
            )?;
            feed(cx, key, KeyChangeKind::Created)
        })
        .await
    }

    async fn update_if_version(
        &self,
        key: &ApiKey,
        expected: u64,
        change: KeyChangeKind,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let at = uuid_key(key.id.to_bytes());
            match one!(cx, Tbl::ApiKeys, &at, Row::ApiKey) {
                Some(stored) if stored.version == expected => {
                    let mut next = key.clone();
                    next.version = expected + 1;
                    next.last_used_at = stored.last_used_at;
                    cx.put(Tbl::ApiKeys, at, Some(Row::ApiKey(next.clone())))?;
                    feed(cx, &next, change)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn set_last_used(&self, id: ApiKeyId, at: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            let Some(mut stored) = one!(cx, Tbl::ApiKeys, &key, Row::ApiKey) else {
                return Ok(false);
            };
            stored.last_used_at = Some(at);
            cx.put(Tbl::ApiKeys, key, Some(Row::ApiKey(stored)))?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl KeyFeed for Runner {
    async fn poll(&self, after_seq: u64, overlap: Duration) -> Result<Vec<KeyChange>, TxError> {
        self.run(Pool::Reserved, false, |cx| {
            let recent = cx.now.saturating_add_micros(-duration_micros(overlap));
            let mut changes = Vec::new();
            for row in all!(cx, Tbl::Feed, Row::Feed) {
                if row.seq <= after_seq && row.at <= recent {
                    continue;
                }
                if let Some(key) = one!(
                    cx,
                    Tbl::ApiKeys,
                    &uuid_key(row.key_id.to_bytes()),
                    Row::ApiKey
                ) {
                    changes.push(KeyChange {
                        seq: row.seq,
                        change: row.change,
                        key_version: row.key_version,
                        at: row.at,
                        key,
                    });
                }
            }
            Ok(changes)
        })
        .await
    }

    async fn snapshot(&self) -> Result<KeySnapshot, TxError> {
        self.run(Pool::Reserved, false, |cx| {
            Ok(KeySnapshot {
                max_seq: all!(cx, Tbl::Feed, Row::Feed)
                    .last()
                    .map_or(0, |row| row.seq),
                keys: all!(cx, Tbl::ApiKeys, Row::ApiKey),
            })
        })
        .await
    }

    async fn prune(&self, older_than: Duration) -> Result<u64, TxError> {
        self.run(Pool::Reserved, true, |cx| {
            let before = cx.now.saturating_add_micros(-duration_micros(older_than));
            let mut pruned = 0;
            for row in all!(cx, Tbl::Feed, Row::Feed) {
                if row.at < before {
                    cx.put(Tbl::Feed, row.seq.to_be_bytes().to_vec(), None)?;
                    pruned += 1;
                }
            }
            Ok(pruned)
        })
        .await
    }
}

#[async_trait]
impl SigningKeyRepo for Runner {
    async fn get(&self, kid: &str) -> Result<Option<SigningKey>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(cx, Tbl::SigningKeys, kid.as_bytes(), Row::SigningKey))
        })
        .await
    }

    async fn list(&self, purpose: SigningPurpose) -> Result<Vec<SigningKey>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut keys = all!(cx, Tbl::SigningKeys, Row::SigningKey);
            keys.retain(|key| key.purpose == purpose);
            keys.sort_by(|a, b| (a.created_at, &a.kid).cmp(&(b.created_at, &b.kid)));
            Ok(keys)
        })
        .await
    }

    async fn insert(&self, key: &SigningKey) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::SigningKeys,
                key.kid.as_bytes().to_vec(),
                Row::SigningKey(key.clone()),
                "signing_keys_pkey",
            )
        })
        .await
    }

    async fn set_state(&self, kid: &str, state: SigningKeyState) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let Some(mut key) = one!(cx, Tbl::SigningKeys, kid.as_bytes(), Row::SigningKey) else {
                return Ok(false);
            };
            key.state = state;
            cx.put(
                Tbl::SigningKeys,
                kid.as_bytes().to_vec(),
                Some(Row::SigningKey(key)),
            )?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl SecretRepo for Runner {
    async fn get(&self, id: Uuid) -> Result<Option<Secret>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::Secrets,
                &uuid_key(id.to_bytes()),
                Row::Secret
            ))
        })
        .await
    }

    async fn insert(&self, secret: &Secret) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Secrets,
                uuid_key(secret.id.to_bytes()),
                Row::Secret(secret.clone()),
                "secrets_pkey",
            )
        })
        .await
    }

    async fn delete(&self, id: Uuid) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            if cx.get(Tbl::Secrets, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::Secrets, key, None)?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl ProjectSaltRepo for Runner {
    async fn get(&self, project_id: ProjectId) -> Result<Option<SealedProjectSalt>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::ProjectSalts,
                &uuid_key(project_id.to_bytes()),
                Row::ProjectSalt
            ))
        })
        .await
    }

    async fn insert_if_absent(&self, salt: &SealedProjectSalt) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(salt.project_id.to_bytes());
            if cx.locked_by_other(Tbl::ProjectSalts, &key) {
                return Err(Halt::Blocked);
            }
            if cx.latest(Tbl::ProjectSalts, &key).is_some() {
                return Ok(false);
            }
            cx.put(Tbl::ProjectSalts, key, Some(Row::ProjectSalt(salt.clone())))?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl Leases for Runner {
    async fn acquire(
        &self,
        name: LeaseName,
        holder: &str,
        ttl: Duration,
    ) -> Result<Option<Lease>, TxError> {
        self.run(Pool::Reserved, true, |cx| {
            let key = lease_key(name);
            let stored = one!(cx, Tbl::Leases, &key, Row::Lease);
            if stored.as_ref().is_some_and(|row| row.expires_at >= cx.now) {
                // The statement still takes the row lock before it finds the lease live.
                cx.check_writable(Tbl::Leases, &key)?;
                return Ok(None);
            }
            let lease = Lease {
                name,
                holder: holder.to_owned(),
                fencing_token: stored.map_or(0, |row| row.fencing_token) + 1,
                expires_at: after(cx.now, ttl),
            };
            cx.put(Tbl::Leases, key, Some(Row::Lease(lease.clone())))?;
            Ok(Some(lease))
        })
        .await
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Option<Lease>, TxError> {
        self.run(Pool::Reserved, true, |cx| {
            let key = lease_key(lease.name);
            if cx.db.mutant == Some(PgMutant::RenewWithoutCompareAndSet) {
                let row = Lease {
                    expires_at: after(cx.now, ttl),
                    ..lease.clone()
                };
                cx.put(Tbl::Leases, key, Some(Row::Lease(row.clone())))?;
                return Ok(Some(row));
            }
            match one!(cx, Tbl::Leases, &key, Row::Lease) {
                Some(mut row)
                    if row.holder == lease.holder
                        && row.fencing_token == lease.fencing_token
                        && row.expires_at >= cx.now =>
                {
                    row.expires_at = after(cx.now, ttl);
                    cx.put(Tbl::Leases, key, Some(Row::Lease(row.clone())))?;
                    Ok(Some(row))
                }
                _ => Ok(None),
            }
        })
        .await
    }

    async fn release(&self, lease: &Lease) -> Result<bool, TxError> {
        self.run(Pool::Reserved, true, |cx| {
            let key = lease_key(lease.name);
            match one!(cx, Tbl::Leases, &key, Row::Lease) {
                Some(mut row)
                    if row.holder == lease.holder && row.fencing_token == lease.fencing_token =>
                {
                    row.expires_at = Timestamp::from_micros(0);
                    cx.put(Tbl::Leases, key, Some(Row::Lease(row)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn get(&self, name: LeaseName) -> Result<Option<Lease>, TxError> {
        self.run(Pool::Reserved, false, |cx| {
            Ok(one!(cx, Tbl::Leases, &lease_key(name), Row::Lease))
        })
        .await
    }

    async fn cursor(&self, name: LeaseName) -> Result<Option<String>, TxError> {
        self.run(Pool::Reserved, false, |cx| {
            Ok(one!(cx, Tbl::Cursors, &lease_key(name), Row::Cursor))
        })
        .await
    }

    async fn set_cursor(&self, lease: &Lease, cursor: &str) -> Result<(), TxError> {
        self.run(Pool::Reserved, true, |cx| {
            fence(cx, lease)?;
            cx.put(
                Tbl::Cursors,
                lease_key(lease.name),
                Some(Row::Cursor(cursor.to_owned())),
            )
        })
        .await
    }
}

#[async_trait]
impl Claims for Runner {
    async fn enqueue(&self, queue: Queue, job: &NewJob) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            let row = JobRow {
                queue,
                id: job.id,
                dedup_key: job.dedup_key.clone(),
                payload: job.payload.clone(),
                attempts: 0,
                next_attempt_at: job.run_at,
                status: JobStatus::Pending,
                lease_until: None,
                export: None,
            };
            cx.insert(
                Tbl::Jobs,
                job_key(queue, job.id),
                Row::Job(row),
                queue_constraints(queue).0,
            )
        })
        .await
    }

    async fn claim(
        &self,
        queue: Queue,
        limit: u32,
        lease_for: Duration,
    ) -> Result<Vec<Claim>, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut due = all!(cx, Tbl::Jobs, Row::Job);
            due.retain(|job| {
                job.queue == queue
                    && job.status == JobStatus::Pending
                    && job.next_attempt_at <= cx.now
                    && job.lease_until.is_none_or(|until| until < cx.now)
                    // Skip locked.
                    && !cx.locked_by_other(Tbl::Jobs, &job_key(queue, job.id))
            });
            due.sort_by_key(|job| (job.next_attempt_at, job.id));
            due.truncate(limit as usize);
            let mut claims = Vec::new();
            for mut job in due {
                job.attempts += 1;
                job.lease_until = Some(after(cx.now, lease_for));
                claims.push(Claim {
                    queue,
                    id: job.id,
                    payload: job.payload.clone(),
                    attempt: job.attempts,
                    lease_until: after(cx.now, lease_for),
                });
                cx.put(Tbl::Jobs, job_key(queue, job.id), Some(Row::Job(job)))?;
            }
            Ok(claims)
        })
        .await
    }

    async fn complete(&self, claim: &Claim) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| settle(cx, claim, Settle::Done))
            .await
    }

    async fn fail(&self, claim: &Claim, retry_at: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            settle(cx, claim, Settle::Retry(retry_at))
        })
        .await
    }

    async fn park(&self, claim: &Claim) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| settle(cx, claim, Settle::Parked))
            .await
    }

    async fn prune_done(&self, queue: Queue, older_than: Duration) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let before = cx.now.saturating_add_micros(-duration_micros(older_than));
            let mut pruned = 0;
            for job in all!(cx, Tbl::Jobs, Row::Job) {
                if job.queue == queue
                    && job.status == JobStatus::Done
                    && job.next_attempt_at < before
                {
                    cx.put(Tbl::Jobs, job_key(queue, job.id), None)?;
                    pruned += 1;
                }
            }
            Ok(pruned)
        })
        .await
    }
}

/// How a claim ends.
#[derive(Clone, Copy)]
enum Settle {
    Done,
    Retry(Timestamp),
    Parked,
}

/// Ends a claim: done, due again at a retry time, or parked.
fn settle(cx: &mut Cx<'_>, claim: &Claim, how: Settle) -> Step<bool> {
    let key = job_key(claim.queue, claim.id);
    let unchecked =
        cx.db.mutant == Some(PgMutant::CompleteWithoutClaimCheck) && matches!(how, Settle::Done);
    match one!(cx, Tbl::Jobs, &key, Row::Job) {
        Some(mut job)
            if (job.attempts == claim.attempt || unchecked) && job.status == JobStatus::Pending =>
        {
            job.lease_until = None;
            match how {
                Settle::Retry(at) => job.next_attempt_at = at,
                Settle::Done => job.status = JobStatus::Done,
                Settle::Parked => job.status = JobStatus::Parked,
            }
            cx.put(Tbl::Jobs, key, Some(Row::Job(job)))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[async_trait]
impl Migrations for Runner {
    async fn applied(&self) -> Result<Vec<LedgerEntry>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::Ledger, Row::Ledger))
        })
        .await
    }

    async fn apply(&self, entry: &LedgerEntry) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = entry.id.as_bytes().to_vec();
            if cx.locked_by_other(Tbl::Ledger, &key) {
                return Err(Halt::Blocked);
            }
            if cx.latest(Tbl::Ledger, &key).is_some() {
                return Ok(false);
            }
            cx.put(Tbl::Ledger, key, Some(Row::Ledger(entry.clone())))?;
            Ok(true)
        })
        .await
    }
}
