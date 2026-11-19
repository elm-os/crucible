//! The repository traits of the control store (contract 2.7): one per aggregate of contract 11.1 that milestone 4
//! needs first.
//!
//! [`Repos`] gives every repository. A [`ControlStore`] implements it with each call as its own statement
//! (read committed); a [`ControlTx`] implements it inside one snapshot transaction. Every method is one
//! round trip and is cancel-safe: dropping its future either leaves the statement unapplied or applied whole.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use crucible_records::{ApiKeyId, ProjectId, Timestamp, UserId, Uuid};
use futures::future::BoxFuture;

use crate::error::TxError;
use crate::types::{
    ApiKey, AuthFlow, AuthProvider, Claim, Credential, Email, Invitation, KeyChange, KeyChangeKind,
    KeySnapshot, Lease, LeaseName, LedgerEntry, NewJob, OauthAccount, OneTimeToken, Org,
    OrgMembership, OrgRole, Project, ProjectMembership, Queue, Secret, Session, SigningKey,
    SigningKeyState, SigningPurpose, TokenHash, TokenKind, User,
};

/// The singleton organization row.
#[async_trait]
pub trait OrgRepo: Send + Sync {
    /// The organization, or `None` before first-owner setup.
    async fn get(&self) -> Result<Option<Org>, TxError>;
    /// Creates the organization. A second insert is `UniqueViolation("org_singleton_key")`.
    async fn insert(&self, org: &Org) -> Result<(), TxError>;
    /// Writes `org` with version `expected + 1` if the stored version is `expected`. `false` when it is not.
    async fn update_if_version(&self, org: &Org, expected: u64) -> Result<bool, TxError>;
}

/// Users.
#[async_trait]
pub trait UserRepo: Send + Sync {
    /// The user with this id.
    async fn get(&self, id: UserId) -> Result<Option<User>, TxError>;
    /// The user with this case-folded email.
    async fn get_by_email(&self, email: &str) -> Result<Option<User>, TxError>;
    /// Every user, ordered by id.
    async fn list(&self) -> Result<Vec<User>, TxError>;
    /// Creates a user. A taken email is `UniqueViolation("users_email_key")`.
    async fn insert(&self, user: &User) -> Result<(), TxError>;
    /// Overwrites the user's columns. `false` when there is no such user.
    async fn update(&self, user: &User) -> Result<bool, TxError>;
    /// Deletes the user with its credential, sessions and memberships. `false` when there is no such user.
    async fn delete(&self, id: UserId) -> Result<bool, TxError>;
}

/// Password credentials, one per user.
#[async_trait]
pub trait CredentialRepo: Send + Sync {
    /// The user's credential.
    async fn get(&self, user_id: UserId) -> Result<Option<Credential>, TxError>;
    /// Creates or replaces the user's credential.
    async fn put(&self, credential: &Credential) -> Result<(), TxError>;
    /// Removes the user's credential. `false` when there was none.
    async fn delete(&self, user_id: UserId) -> Result<bool, TxError>;
}

/// Login sessions. Lookups run on the reserved pool.
#[async_trait]
pub trait SessionRepo: Send + Sync {
    /// The session with this token hash.
    async fn get(&self, token_hash: &TokenHash) -> Result<Option<Session>, TxError>;
    /// Creates a session.
    async fn insert(&self, session: &Session) -> Result<(), TxError>;
    /// Slides the idle expiry. `false` when there is no such session.
    async fn touch(
        &self,
        token_hash: &TokenHash,
        idle_expires_at: Timestamp,
    ) -> Result<bool, TxError>;
    /// Revokes one session. `false` when there is none or it was already revoked.
    async fn revoke(&self, token_hash: &TokenHash, at: Timestamp) -> Result<bool, TxError>;
    /// Revokes every live session of the user except `keep`; returns how many.
    async fn revoke_user(
        &self,
        user_id: UserId,
        keep: Option<&TokenHash>,
        at: Timestamp,
    ) -> Result<u64, TxError>;
    /// Deletes the sessions whose absolute expiry is before `now`; returns how many.
    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError>;
}

/// Projects. Deletion is soft: `update_if_version` sets `deleted_at`.
#[async_trait]
pub trait ProjectRepo: Send + Sync {
    /// The project with this id, deleted or not.
    async fn get(&self, id: ProjectId) -> Result<Option<Project>, TxError>;
    /// The project with this slug.
    async fn get_by_slug(&self, slug: &str) -> Result<Option<Project>, TxError>;
    /// Every project that is not deleted, ordered by id.
    async fn list(&self) -> Result<Vec<Project>, TxError>;
    /// Creates a project. A taken slug is `UniqueViolation("projects_slug_key")`.
    async fn insert(&self, project: &Project) -> Result<(), TxError>;
    /// Writes `project` with version `expected + 1` if the stored version is `expected`.
    async fn update_if_version(&self, project: &Project, expected: u64) -> Result<bool, TxError>;
}

/// Organization and project memberships.
#[async_trait]
pub trait MembershipRepo: Send + Sync {
    /// The user's organization role.
    async fn org_role(&self, user_id: UserId) -> Result<Option<OrgRole>, TxError>;
    /// Every organization membership, ordered by user id.
    async fn list_org(&self) -> Result<Vec<OrgMembership>, TxError>;
    /// Sets the user's organization role.
    async fn put_org(&self, membership: OrgMembership) -> Result<(), TxError>;
    /// Removes the user from the organization. `false` when the user was not a member.
    async fn delete_org(&self, user_id: UserId) -> Result<bool, TxError>;
    /// The user's memberships on projects, ordered by project id.
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<ProjectMembership>, TxError>;
    /// The memberships of a project, ordered by user id.
    async fn list_project(&self, project_id: ProjectId) -> Result<Vec<ProjectMembership>, TxError>;
    /// Sets the user's role on a project.
    async fn put_project(&self, membership: ProjectMembership) -> Result<(), TxError>;
    /// Removes the user from a project. `false` when the user was not a member.
    async fn delete_project(&self, project_id: ProjectId, user_id: UserId)
    -> Result<bool, TxError>;
}

/// API keys. Every change that verification depends on writes a key-feed row in the same statement
/// (contract 11.8).
#[async_trait]
pub trait ApiKeyRepo: Send + Sync {
    /// The key with this id.
    async fn get(&self, id: ApiKeyId) -> Result<Option<ApiKey>, TxError>;
    /// The keys of a project, ordered by id.
    async fn list(&self, project_id: ProjectId) -> Result<Vec<ApiKey>, TxError>;
    /// Creates a key and its `created` feed row. A taken public key is
    /// `UniqueViolation("api_keys_public_key_key")`.
    async fn insert(&self, key: &ApiKey) -> Result<(), TxError>;
    /// Writes `key` with version `expected + 1` and a feed row of kind `change`, if the stored version is
    /// `expected`. `false` when it is not. `last_used_at` is not written here.
    async fn update_if_version(
        &self,
        key: &ApiKey,
        expected: u64,
        change: KeyChangeKind,
    ) -> Result<bool, TxError>;
    /// Records a use. Changes no version and writes no feed row. `false` when there is no such key.
    async fn set_last_used(&self, id: ApiKeyId, at: Timestamp) -> Result<bool, TxError>;
}

/// The key feed (contract 11.8). Runs on the reserved pool.
#[async_trait]
pub trait KeyFeed: Send + Sync {
    /// The feed rows with a sequence number above `after_seq`, and also every row written within `overlap`
    /// of the database clock, ordered by sequence number. Sequence numbers become visible out of order, so
    /// `overlap` must cover the longest transaction plus the poll interval; rows are applied by key version,
    /// so reading one twice is harmless.
    async fn poll(&self, after_seq: u64, overlap: Duration) -> Result<Vec<KeyChange>, TxError>;
    /// Every key, for the full reconcile.
    async fn snapshot(&self) -> Result<KeySnapshot, TxError>;
    /// Deletes the feed rows written more than `older_than` before the database clock; returns how many.
    /// `older_than` must cover the poll overlap window plus the reconcile interval, so that a row is gone
    /// only after every node has reloaded the full snapshot at least once since it was written.
    async fn prune(&self, older_than: Duration) -> Result<u64, TxError>;
}

/// Signing keys.
#[async_trait]
pub trait SigningKeyRepo: Send + Sync {
    /// The key with this id.
    async fn get(&self, kid: &str) -> Result<Option<SigningKey>, TxError>;
    /// The keys of a purpose, ordered by creation time then id.
    async fn list(&self, purpose: SigningPurpose) -> Result<Vec<SigningKey>, TxError>;
    /// Creates a key.
    async fn insert(&self, key: &SigningKey) -> Result<(), TxError>;
    /// Moves a key to another state. `false` when there is no such key.
    async fn set_state(&self, kid: &str, state: SigningKeyState) -> Result<bool, TxError>;
}

/// Sealed secrets.
#[async_trait]
pub trait SecretRepo: Send + Sync {
    /// The secret with this id.
    async fn get(&self, id: Uuid) -> Result<Option<Secret>, TxError>;
    /// Creates a secret.
    async fn insert(&self, secret: &Secret) -> Result<(), TxError>;
    /// Deletes a secret. `false` when there is no such secret.
    async fn delete(&self, id: Uuid) -> Result<bool, TxError>;
}

/// Sign-in providers.
#[async_trait]
pub trait AuthProviderRepo: Send + Sync {
    /// The provider with this id.
    async fn get(&self, id: Uuid) -> Result<Option<AuthProvider>, TxError>;
    /// Every provider, ordered by id.
    async fn list(&self) -> Result<Vec<AuthProvider>, TxError>;
    /// Creates a provider. A second row from configuration of one kind is
    /// `UniqueViolation("auth_providers_seeded_kind_key")`.
    async fn insert(&self, provider: &AuthProvider) -> Result<(), TxError>;
    /// Overwrites the provider's columns. `false` when there is no such provider.
    async fn update(&self, provider: &AuthProvider) -> Result<bool, TxError>;
    /// Deletes a provider with its linked accounts. `false` when there is no such provider.
    async fn delete(&self, id: Uuid) -> Result<bool, TxError>;
}

/// Provider identities linked to users.
#[async_trait]
pub trait OauthAccountRepo: Send + Sync {
    /// The account of this provider subject.
    async fn get(&self, provider_id: Uuid, subject: &str) -> Result<Option<OauthAccount>, TxError>;
    /// The accounts of a user, ordered by provider id then subject.
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<OauthAccount>, TxError>;
    /// Links a provider subject to a user. A subject that is already linked is
    /// `UniqueViolation("oauth_accounts_pkey")`.
    async fn insert(&self, account: &OauthAccount) -> Result<(), TxError>;
}

/// Sign-in attempts through a provider.
#[async_trait]
pub trait AuthFlowRepo: Send + Sync {
    /// Creates a flow.
    async fn insert(&self, flow: &AuthFlow) -> Result<(), TxError>;
    /// Uses the flow, in one statement: marks it consumed at `now` and returns it, if it exists, was not
    /// consumed and has not expired at `now`. `None` otherwise, so a `state` works once.
    async fn consume(
        &self,
        state_hash: &TokenHash,
        now: Timestamp,
    ) -> Result<Option<AuthFlow>, TxError>;
    /// Deletes the flows that expired before `now`; returns how many.
    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError>;
}

/// Single-use tokens.
#[async_trait]
pub trait OneTimeTokenRepo: Send + Sync {
    /// The token with this hash, used or not.
    async fn get(&self, token_hash: &TokenHash) -> Result<Option<OneTimeToken>, TxError>;
    /// Creates a token.
    async fn insert(&self, token: &OneTimeToken) -> Result<(), TxError>;
    /// Uses the token, in one statement: marks it consumed at `now` and returns it, if it exists, is of
    /// `kind`, was not consumed and has not expired at `now`. `None` otherwise, so a token works once.
    async fn consume(
        &self,
        token_hash: &TokenHash,
        kind: TokenKind,
        now: Timestamp,
    ) -> Result<Option<OneTimeToken>, TxError>;
    /// Deletes the unused tokens of `kind` for `subject`; returns how many.
    async fn delete_unused(&self, kind: TokenKind, subject: &str) -> Result<u64, TxError>;
    /// Whether an unused token of `kind` exists that has not expired at `now`.
    async fn any_open(&self, kind: TokenKind, now: Timestamp) -> Result<bool, TxError>;
    /// Deletes the tokens that expired before `now`; returns how many.
    async fn delete_expired(&self, now: Timestamp) -> Result<u64, TxError>;
}

/// Invitations.
#[async_trait]
pub trait InvitationRepo: Send + Sync {
    /// The invitation with this id.
    async fn get(&self, id: Uuid) -> Result<Option<Invitation>, TxError>;
    /// Every invitation, ordered by id.
    async fn list(&self) -> Result<Vec<Invitation>, TxError>;
    /// The invitations of this case-folded email, open or not, ordered by id.
    async fn list_for_email(&self, email: &Email) -> Result<Vec<Invitation>, TxError>;
    /// Creates an invitation.
    async fn insert(&self, invitation: &Invitation) -> Result<(), TxError>;
    /// Marks the invitation accepted at `now`, in one statement, if it is open at `now`. `false` when it is
    /// not, so an invitation is accepted once.
    async fn accept(&self, id: Uuid, now: Timestamp) -> Result<bool, TxError>;
    /// Revokes an invitation that was neither accepted nor revoked. `false` otherwise.
    async fn revoke(&self, id: Uuid, now: Timestamp) -> Result<bool, TxError>;
}

/// Leases with fencing tokens (contract 11.7). Expiry is judged by the database clock. Runs on the reserved
/// pool.
#[async_trait]
pub trait Leases: Send + Sync {
    /// Takes the lease if no row exists or the row has expired: one statement that sets the holder, increments
    /// the fencing token and sets the expiry to the database clock plus `ttl`. `None` while another holder's
    /// lease is live (also the caller's own: a holder renews).
    async fn acquire(
        &self,
        name: LeaseName,
        holder: &str,
        ttl: Duration,
    ) -> Result<Option<Lease>, TxError>;
    /// Extends the lease to the database clock plus `ttl`: a compare-and-set on holder and fencing token that
    /// also requires the lease not to have expired. `None` when the lease is no longer this one.
    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Option<Lease>, TxError>;
    /// Gives the lease up by expiring it. The row and its fencing token stay, so a stale token never matches
    /// again. `false` when the lease is no longer this one.
    async fn release(&self, lease: &Lease) -> Result<bool, TxError>;
    /// The lease row as stored, expired or not.
    async fn get(&self, name: LeaseName) -> Result<Option<Lease>, TxError>;
    /// The job cursor stored under the lease name.
    async fn cursor(&self, name: LeaseName) -> Result<Option<String>, TxError>;
    /// A fenced leader write: stores the job cursor if the lease row still carries this holder and fencing
    /// token, in one statement. [`TxError::Fenced`] when it does not.
    async fn set_cursor(&self, lease: &Lease, cursor: &str) -> Result<(), TxError>;
}

/// Claimed rows (contract 11.7): skip-locked claims with a `lease_until` and an attempt counter.
#[async_trait]
pub trait Claims: Send + Sync {
    /// Adds a pending row. A taken deduplication key is a `UniqueViolation`.
    async fn enqueue(&self, queue: Queue, job: &NewJob) -> Result<(), TxError>;
    /// Claims up to `limit` pending rows that are due and not under a live claim, oldest due first, skipping
    /// rows another transaction has locked. Each claimed row's attempt counter increases and its
    /// `lease_until` becomes the database clock plus `lease_for`.
    async fn claim(
        &self,
        queue: Queue,
        limit: u32,
        lease_for: Duration,
    ) -> Result<Vec<Claim>, TxError>;
    /// Marks the row done. `false` when the row was claimed again since (the attempt counter moved) or is
    /// already done.
    async fn complete(&self, claim: &Claim) -> Result<bool, TxError>;
    /// Gives the row back, due again at `retry_at`. `false` under the same conditions as `complete`.
    async fn fail(&self, claim: &Claim, retry_at: Timestamp) -> Result<bool, TxError>;
    /// Parks the row: it stays in the table and is never claimed again, for an operator to inspect. `false`
    /// under the same conditions as `complete`.
    async fn park(&self, claim: &Claim) -> Result<bool, TxError>;
}

/// The forward-only migrations ledger (contract 6.6): Postgres migrations and ClickHouse schema and access
/// steps.
#[async_trait]
pub trait Migrations: Send + Sync {
    /// Every applied step, ordered by id.
    async fn applied(&self) -> Result<Vec<LedgerEntry>, TxError>;
    /// Records a step as applied. `false` when a step with this id is already recorded; the stored row is not
    /// changed.
    async fn apply(&self, entry: &LedgerEntry) -> Result<bool, TxError>;
}

/// Every repository.
pub trait Repos: Send + Sync {
    /// The organization.
    fn org(&self) -> &dyn OrgRepo;
    /// Users.
    fn users(&self) -> &dyn UserRepo;
    /// Password credentials.
    fn credentials(&self) -> &dyn CredentialRepo;
    /// Login sessions.
    fn sessions(&self) -> &dyn SessionRepo;
    /// Projects.
    fn projects(&self) -> &dyn ProjectRepo;
    /// Memberships.
    fn memberships(&self) -> &dyn MembershipRepo;
    /// API keys.
    fn api_keys(&self) -> &dyn ApiKeyRepo;
    /// The key feed.
    fn key_feed(&self) -> &dyn KeyFeed;
    /// Signing keys.
    fn signing_keys(&self) -> &dyn SigningKeyRepo;
    /// Sealed secrets.
    fn secrets(&self) -> &dyn SecretRepo;
    /// Sign-in providers.
    fn auth_providers(&self) -> &dyn AuthProviderRepo;
    /// Provider identities linked to users.
    fn oauth_accounts(&self) -> &dyn OauthAccountRepo;
    /// Sign-in attempts through a provider.
    fn auth_flows(&self) -> &dyn AuthFlowRepo;
    /// Single-use tokens.
    fn one_time_tokens(&self) -> &dyn OneTimeTokenRepo;
    /// Invitations.
    fn invitations(&self) -> &dyn InvitationRepo;
    /// Leases.
    fn leases(&self) -> &dyn Leases;
    /// Claimed rows.
    fn claims(&self) -> &dyn Claims;
    /// The migrations ledger.
    fn migrations(&self) -> &dyn Migrations;
}

/// Postgres behind repositories (contract 2.7). Through [`Repos`], each call is its own statement.
#[async_trait]
pub trait ControlStore: Repos + fmt::Debug {
    /// Opens a snapshot transaction (repeatable read) on the main pool. It must be committed within
    /// [`TRANSACTION_CAP`] of this call.
    async fn begin(&self) -> Result<Box<dyn ControlTx>, TxError>;
    /// The database clock.
    async fn db_now(&self) -> Result<Timestamp, TxError>;
}

/// The longest a transaction may stay open (contract 2.7 and 11.8): the key-feed overlap window relies on it.
pub const TRANSACTION_CAP: Duration = Duration::from_secs(10);

/// One open snapshot transaction. After any statement fails, every later statement and the commit answer that
/// same error and nothing is applied. Dropping the transaction without committing rolls it back.
#[async_trait]
pub trait ControlTx: Repos + fmt::Debug + Send {
    /// Makes the transaction a fenced leader write (contract 11.7): locks the lease row against a new
    /// acquisition until the transaction ends and checks holder and fencing token. [`TxError::Fenced`] when
    /// they have changed.
    async fn fence(&self, lease: &Lease) -> Result<(), TxError>;
    /// Commits. [`TxError::CommitUnknown`] when the connection was lost while the commit was in flight.
    async fn commit(self: Box<Self>) -> Result<(), TxError>;
    /// Rolls back.
    async fn rollback(self: Box<Self>);
}

/// Runs `f` in one transaction (contract 2.7): commits when `f` returns `Ok`, rolls back when it returns
/// `Err`. The caller decides about retries: [`TxError::is_retryable`].
pub async fn transaction<T, F>(store: &dyn ControlStore, f: F) -> Result<T, TxError>
where
    T: Send,
    F: for<'a> FnOnce(&'a dyn ControlTx) -> BoxFuture<'a, Result<T, TxError>> + Send,
{
    let tx = store.begin().await?;
    match f(tx.as_ref()).await {
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
