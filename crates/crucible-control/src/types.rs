//! The domain types of the control plane (contract 11.1): one struct per row of the aggregates that milestone
//! 4 needs first. Ids and timestamps are made in Rust by the caller; no type here reads a clock.

use std::fmt;
use std::time::Duration;

use crucible_records::{ApiKeyId, ExportJobId, OrgId, ProjectId, Timestamp, UserId, Uuid};
use serde::{Deserialize, Serialize};

use crate::error::TxError;

/// A closed set of values stored as lowercase text.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
        pub enum $name {
            $($(#[$vmeta])* #[serde(rename = $text)] $variant,)+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The stored text.
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)+
                }
            }

            /// The variant with this stored text.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    /// How people join the organization (contract 11.5). There is no open sign-up.
    SignupMode {
        /// By invitation, first-owner setup or the allowed-domain rule.
        InviteOnly => "invite_only",
    }
}

text_enum! {
    /// How org members and viewers reach projects (contract 11.6).
    ProjectAccess {
        /// Their org role applies on every project.
        All => "all",
        /// Only through a project membership.
        Explicit => "explicit",
    }
}

text_enum! {
    /// Whether a user may sign in.
    UserStatus {
        /// May sign in.
        Active => "active",
        /// May not sign in; sessions are revoked by the caller.
        Disabled => "disabled",
    }
}

text_enum! {
    /// How a session was authenticated.
    AuthMethod {
        /// Email and password.
        Password => "password",
        /// The GitHub provider.
        Github => "github",
        /// The Google provider.
        Google => "google",
        /// A generic OIDC provider.
        Oidc => "oidc",
    }
}

text_enum! {
    /// Organization roles (contract 11.6).
    OrgRole {
        /// Owner.
        Owner => "owner",
        /// Admin.
        Admin => "admin",
        /// Member.
        Member => "member",
        /// Viewer.
        Viewer => "viewer",
    }
}

text_enum! {
    /// Project roles (contract 11.6).
    ProjectRole {
        /// Admin.
        Admin => "admin",
        /// Member.
        Member => "member",
        /// Viewer.
        Viewer => "viewer",
    }
}

text_enum! {
    /// API key scopes (contract 11.3).
    KeyScope {
        /// OTLP and compat ingest; media upload and confirm.
        Ingest => "ingest",
        /// Every telemetry and analytics read.
        Read => "read",
        /// Feedback, tags, flags, corrections, comments.
        Write => "write",
        /// Deletion requests.
        Delete => "delete",
        /// SQL workspace runs.
        Sql => "sql",
    }
}

text_enum! {
    /// What a key-feed row announces (contract 11.8).
    KeyChangeKind {
        /// The key was created.
        Created => "created",
        /// Scopes, note or another attribute changed.
        Changed => "changed",
        /// The key was revoked.
        Revoked => "revoked",
        /// The expiry was edited.
        ExpiryEdited => "expiry_edited",
    }
}

text_enum! {
    /// What a signing key signs.
    SigningPurpose {
        /// Pagination cursors.
        Cursor => "cursor",
        /// Export download links.
        Download => "download",
        /// Media tokens.
        MediaToken => "media_token",
    }
}

text_enum! {
    /// The life cycle of a signing key (contract 11.2).
    SigningKeyState {
        /// Signs and verifies.
        Active => "active",
        /// Verifies only.
        VerifyOnly => "verify_only",
        /// No longer used.
        Retired => "retired",
    }
}

text_enum! {
    /// The lease names of contract 11.7.
    LeaseName {
        /// Schema and access migrations.
        Migrations => "migrations",
        /// Retention, deletion leader steps, the mask scheduler, marker purge and rollup rebuild.
        Lifecycle => "lifecycle",
        /// SQL workspace identities.
        SqlIdentities => "sql_identities",
        /// Media garbage collection.
        MediaGc => "media_gc",
        /// Signing-key rotation.
        SigningKeys => "signing_keys",
        /// Housekeeping.
        Housekeeping => "housekeeping",
    }
}

text_enum! {
    /// The queues whose rows are claimed (contract 11.7). Each names one table.
    Queue {
        /// `notification_outbox`.
        NotificationOutbox => "notification_outbox",
        /// `alert_evaluations`: one row per rule and evaluation tick.
        AlertEvaluations => "alert_evaluations",
        /// `export_jobs` (contract 12.8).
        ExportJobs => "export_jobs",
    }
}

text_enum! {
    /// The `status` column of a claimed row (contract 11.7).
    ClaimStatus {
        /// Waiting for a claim, or under one.
        Pending => "pending",
        /// Completed.
        Done => "done",
        /// Parked at the attempt cap; never claimed again.
        Parked => "parked",
    }
}

text_enum! {
    /// The formats of an export (contract 12.8).
    ExportFormat {
        /// One JSON object per span with the keys `schema`, `span` and `facts`.
        CrucibleJsonl => "crucible-jsonl",
        /// One OTLP JSON document per span.
        OtlpJson => "otlp-json",
    }
}

pub(crate) use text_enum;

text_enum! {
    /// The kind of a ledger entry (contract 6.6 and 11.1).
    MigrationKind {
        /// A Postgres migration.
        Postgres => "postgres",
        /// A ClickHouse schema step.
        ClickhouseSchema => "clickhouse_schema",
        /// A ClickHouse access step.
        ClickhouseAccess => "clickhouse_access",
    }
}

text_enum! {
    /// The kind of a sign-in provider (contract 11.1).
    ProviderKind {
        /// GitHub, over OAuth 2.0.
        Github => "github",
        /// Google, over OpenID Connect.
        Google => "google",
        /// Any OpenID Connect provider.
        Oidc => "oidc",
    }
}

impl ProviderKind {
    /// The auth method of a session signed in through a provider of this kind.
    pub const fn auth_method(self) -> AuthMethod {
        match self {
            Self::Github => AuthMethod::Github,
            Self::Google => AuthMethod::Google,
            Self::Oidc => AuthMethod::Oidc,
        }
    }
}

text_enum! {
    /// What a one-time token is for (contract 11.1).
    TokenKind {
        /// A password reset; the subject is the user id.
        PasswordReset => "password_reset",
        /// An invitation; the subject is the invitation id.
        Invite => "invite",
        /// First-owner setup; the subject is empty.
        Setup => "setup",
    }
}

/// An email address, case-folded. Uniqueness of users is on this folded form.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Email(String);

impl Email {
    /// Folds `text` to lowercase. Refuses text without exactly one `@` between two non-empty parts, or with
    /// whitespace or a control character; real validation is the verification mail.
    pub fn new(text: &str) -> Option<Self> {
        let folded = text.to_lowercase();
        let (local, domain) = folded.split_once('@')?;
        let plain = !folded.chars().any(|c| c.is_whitespace() || c.is_control());
        (plain && !local.is_empty() && !domain.is_empty() && !domain.contains('@'))
            .then_some(Self(folded))
    }

    /// The folded address.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Email {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A JSON document stored in a `jsonb` column. Equality is structural, so key order and spacing do not matter.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsonDoc(serde_json::Value);

impl JsonDoc {
    /// The empty object.
    pub fn empty() -> Self {
        Self(serde_json::Value::Object(serde_json::Map::new()))
    }

    /// Parses JSON text.
    pub fn parse(text: &str) -> Result<Self, TxError> {
        serde_json::from_str(text)
            .map(Self)
            .map_err(|error| TxError::Invalid(format!("json: {error}")))
    }

    /// The document as compact JSON text.
    pub fn to_text(&self) -> String {
        self.0.to_string()
    }
}

/// The organization's settings (contract 11.1), stored as one JSON document.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgSettings {
    /// How people join.
    pub signup_mode: SignupMode,
    /// Email domains whose verified members may join without an invitation.
    pub allowed_email_domains: Vec<String>,
    /// Whether email-and-password login is on.
    pub password_login_enabled: bool,
    /// How org members and viewers reach projects.
    pub project_access: ProjectAccess,
}

impl Default for OrgSettings {
    fn default() -> Self {
        Self {
            signup_mode: SignupMode::InviteOnly,
            allowed_email_domains: Vec::new(),
            password_login_enabled: true,
            project_access: ProjectAccess::All,
        }
    }
}

impl OrgSettings {
    /// The stored JSON text.
    pub fn to_text(&self) -> String {
        // A struct of enums, strings and a bool always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parses the stored JSON text.
    pub fn parse(text: &str) -> Result<Self, TxError> {
        serde_json::from_str(text)
            .map_err(|error| TxError::Invalid(format!("org settings: {error}")))
    }
}

/// The singleton organization row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Org {
    /// The id.
    pub id: OrgId,
    /// The display name.
    pub name: String,
    /// The settings.
    pub settings: OrgSettings,
    /// The optimistic-concurrency version; starts at 1.
    pub version: u64,
}

/// A user.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct User {
    /// The id.
    pub id: UserId,
    /// The case-folded email; unique.
    pub email: Email,
    /// The display name.
    pub name: String,
    /// Whether the user may sign in.
    pub status: UserStatus,
    /// When the email was verified.
    pub email_verified_at: Option<Timestamp>,
}

/// A user's password credential.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// The user.
    pub user_id: UserId,
    /// The Argon2id PHC string.
    pub password_hash: String,
    /// The version of the hashing parameters.
    pub params_version: u32,
    /// Consecutive failed logins.
    pub failed_count: u32,
    /// Login is refused until then.
    pub locked_until: Option<Timestamp>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("user_id", &self.user_id)
            .field("params_version", &self.params_version)
            .field("failed_count", &self.failed_count)
            .field("locked_until", &self.locked_until)
            .finish_non_exhaustive()
    }
}

/// SHA-256 of a session token. The raw token is never stored.
pub type TokenHash = [u8; 32];

/// A login session (contract 11.4).
#[derive(Clone, PartialEq, Eq)]
pub struct Session {
    /// SHA-256 of the 256-bit token; the primary key.
    pub token_hash: TokenHash,
    /// The user.
    pub user_id: UserId,
    /// The CSRF token of the session.
    pub csrf_token: String,
    /// Sliding idle expiry.
    pub idle_expires_at: Timestamp,
    /// Absolute expiry.
    pub absolute_expires_at: Timestamp,
    /// How the session was authenticated.
    pub auth_method: AuthMethod,
    /// When it was revoked.
    pub revoked_at: Option<Timestamp>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("idle_expires_at", &self.idle_expires_at)
            .field("absolute_expires_at", &self.absolute_expires_at)
            .field("auth_method", &self.auth_method)
            .field("revoked_at", &self.revoked_at)
            .finish_non_exhaustive()
    }
}

/// A project.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Project {
    /// The id.
    pub id: ProjectId,
    /// The display name.
    pub name: String,
    /// The URL slug; unique.
    pub slug: String,
    /// The settings document.
    pub settings: JsonDoc,
    /// Days telemetry is kept; `None` is the deployment default.
    pub retention_days: Option<u32>,
    /// Days payloads are kept.
    pub payload_retention_days: Option<u32>,
    /// Days MCP content is kept.
    pub mcp_content_retention_days: Option<u32>,
    /// Increases on every change that the normalizer snapshots.
    pub config_version: u64,
    /// The optimistic-concurrency version; starts at 1.
    pub version: u64,
    /// When the project was deleted (a soft delete).
    pub deleted_at: Option<Timestamp>,
}

/// A user's organization role.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OrgMembership {
    /// The user.
    pub user_id: UserId,
    /// The role.
    pub role: OrgRole,
}

/// A user's role on one project.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProjectMembership {
    /// The project.
    pub project_id: ProjectId,
    /// The user.
    pub user_id: UserId,
    /// The role.
    pub role: ProjectRole,
}

/// An API key row (contract 11.3). The secret itself is never stored.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey {
    /// The id.
    pub id: ApiKeyId,
    /// The project the key is a principal of.
    pub project_id: ProjectId,
    /// The public key string, `cpk_...`; unique.
    pub public_key: String,
    /// 16 random bytes.
    pub secret_salt: [u8; 16],
    /// `SHA-256(secret_salt || secret key string)`.
    pub secret_hash: [u8; 32],
    /// The first 8 bytes of the lookup HMAC of the secret key string.
    pub lookup_tag: [u8; 8],
    /// The scopes, sorted without duplicates.
    pub scopes: Vec<KeyScope>,
    /// Whether the key may bypass the event-age limit.
    pub import: bool,
    /// The operator's note.
    pub note: String,
    /// When the key stops working.
    pub expires_at: Option<Timestamp>,
    /// When the key was last used, in 60 s batches.
    pub last_used_at: Option<Timestamp>,
    /// When the key was revoked.
    pub revoked_at: Option<Timestamp>,
    /// Increases on every change that the key feed announces; starts at 1.
    pub version: u64,
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKey")
            .field("id", &self.id)
            .field("project_id", &self.project_id)
            .field("public_key", &self.public_key)
            .field("scopes", &self.scopes)
            .field("import", &self.import)
            .field("note", &self.note)
            .field("expires_at", &self.expires_at)
            .field("last_used_at", &self.last_used_at)
            .field("revoked_at", &self.revoked_at)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// One row of the key feed, joined with the key's current row (contract 11.8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyChange {
    /// The feed sequence number. Numbers can become visible out of order.
    pub seq: u64,
    /// What happened.
    pub change: KeyChangeKind,
    /// The key's version after the change this row announces.
    pub key_version: u64,
    /// The database clock when the row was written.
    pub at: Timestamp,
    /// The key as it is now; its `version` is at least `key_version`.
    pub key: ApiKey,
}

/// Every key, for the full reconcile.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeySnapshot {
    /// The highest feed sequence number visible before the keys were read; polling resumes after it.
    pub max_seq: u64,
    /// Every key row, ordered by id.
    pub keys: Vec<ApiKey>,
}

/// A signing key (contract 11.2).
#[derive(Clone, PartialEq, Eq)]
pub struct SigningKey {
    /// The key id.
    pub kid: String,
    /// What it signs.
    pub purpose: SigningPurpose,
    /// The key, sealed under the master key.
    pub sealed_key: Vec<u8>,
    /// Its state.
    pub state: SigningKeyState,
    /// When it was created.
    pub created_at: Timestamp,
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .field("purpose", &self.purpose)
            .field("state", &self.state)
            .field("created_at", &self.created_at)
            .finish_non_exhaustive()
    }
}

/// A sealed secret (contract 11.2). The associated data of the seal binds `id`, `kind` and `scope`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    /// The id.
    pub id: Uuid,
    /// What the secret belongs to, for example a project or the organization.
    pub scope: String,
    /// What the secret is, for example a channel webhook.
    pub kind: String,
    /// Which master key of the ring sealed it.
    pub key_id: String,
    /// The nonce.
    pub nonce: Vec<u8>,
    /// The ciphertext with its tag.
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret")
            .field("id", &self.id)
            .field("scope", &self.scope)
            .field("kind", &self.kind)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// The salt of a project, sealed (contracts 9.3, 11.1 and 11.2). It is written once and never changes. The
/// associated data of the seal binds the project.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedProjectSalt {
    /// The project.
    pub project_id: ProjectId,
    /// Which master key of the ring sealed it.
    pub key_id: String,
    /// The nonce.
    pub nonce: Vec<u8>,
    /// The ciphertext with its tag.
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for SealedProjectSalt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedProjectSalt")
            .field("project_id", &self.project_id)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// A sign-in provider (contract 11.1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AuthProvider {
    /// The id; it appears in the provider's redirect URI, so it never changes.
    pub id: Uuid,
    /// The kind.
    pub kind: ProviderKind,
    /// The name on the sign-in button.
    pub display_name: String,
    /// The OpenID Connect issuer; empty for GitHub.
    pub issuer: String,
    /// The OAuth client id.
    pub client_id: String,
    /// The sealed client secret, when it is stored; a provider from configuration keeps its secret there.
    pub secret_id: Option<Uuid>,
    /// The scopes to request.
    pub scopes: Vec<String>,
    /// Email domains that may sign in through this provider; empty allows every domain.
    pub allowed_domains: Vec<String>,
    /// Whether the provider is offered.
    pub enabled: bool,
    /// Whether the row comes from configuration: at most one such row per kind, read-only in the UI.
    pub seeded: bool,
}

/// A provider identity linked to a user (contract 11.1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OauthAccount {
    /// The provider.
    pub provider_id: Uuid,
    /// The provider's stable subject for the person.
    pub subject: String,
    /// The user.
    pub user_id: UserId,
}

/// One sign-in attempt through a provider, between the redirect and the callback (contract 11.1).
#[derive(Clone, PartialEq, Eq)]
pub struct AuthFlow {
    /// SHA-256 of the `state` parameter; the primary key.
    pub state_hash: TokenHash,
    /// The provider.
    pub provider_id: Uuid,
    /// The PKCE verifier, sealed under the master key.
    pub sealed_verifier: Vec<u8>,
    /// The OpenID Connect nonce; empty for GitHub.
    pub nonce: String,
    /// Where to go after sign-in.
    pub next_path: String,
    /// The flow is refused after this time.
    pub expires_at: Timestamp,
    /// When the callback used it.
    pub consumed_at: Option<Timestamp>,
}

impl fmt::Debug for AuthFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthFlow")
            .field("provider_id", &self.provider_id)
            .field("next_path", &self.next_path)
            .field("expires_at", &self.expires_at)
            .field("consumed_at", &self.consumed_at)
            .finish_non_exhaustive()
    }
}

/// A single-use token (contract 11.1). Only the hash of the token is stored.
#[derive(Clone, PartialEq, Eq)]
pub struct OneTimeToken {
    /// SHA-256 of the 256-bit token; the primary key.
    pub token_hash: TokenHash,
    /// What the token is for.
    pub kind: TokenKind,
    /// What the token acts on; see [`TokenKind`].
    pub subject: String,
    /// The token is refused after this time.
    pub expires_at: Timestamp,
    /// When it was used.
    pub consumed_at: Option<Timestamp>,
}

impl fmt::Debug for OneTimeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OneTimeToken")
            .field("kind", &self.kind)
            .field("subject", &self.subject)
            .field("expires_at", &self.expires_at)
            .field("consumed_at", &self.consumed_at)
            .finish_non_exhaustive()
    }
}

/// An invitation to join the organization (contract 11.1 and 11.5).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Invitation {
    /// The id.
    pub id: Uuid,
    /// The invited address, case-folded.
    pub email: Email,
    /// The organization role the person gets.
    pub org_role: OrgRole,
    /// The project memberships the person gets, ordered by project id.
    pub project_roles: Vec<(ProjectId, ProjectRole)>,
    /// The invitation is refused after this time.
    pub expires_at: Timestamp,
    /// When it was accepted.
    pub accepted_at: Option<Timestamp>,
    /// When it was revoked.
    pub revoked_at: Option<Timestamp>,
}

impl Invitation {
    /// Whether the invitation can still be accepted at `now`.
    pub fn is_open(&self, now: Timestamp) -> bool {
        self.accepted_at.is_none() && self.revoked_at.is_none() && now < self.expires_at
    }
}

/// A held lease (contract 11.7).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lease {
    /// The lease.
    pub name: LeaseName,
    /// Who holds it, for example a node's `wal_epoch`.
    pub holder: String,
    /// Increases at every acquisition; fenced writes compare it.
    pub fencing_token: u64,
    /// The database clock at which the lease expires unless renewed.
    pub expires_at: Timestamp,
}

/// A row to enqueue for claiming.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewJob {
    /// The id.
    pub id: Uuid,
    /// The deduplication key; unique within the queue.
    pub dedup_key: String,
    /// The payload.
    pub payload: String,
    /// The row is not claimed before this time (compared with the database clock).
    pub run_at: Timestamp,
}

/// A claimed row (contract 11.7).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Claim {
    /// The queue.
    pub queue: Queue,
    /// The row id.
    pub id: Uuid,
    /// The payload.
    pub payload: String,
    /// The attempt counter after this claim; the first claim is 1. `complete` and `fail` compare it, so a
    /// claimer whose lease ran out and whose row was claimed again cannot finish the newer attempt.
    pub attempt: u32,
    /// The database clock until which other claimers skip the row.
    pub lease_until: Timestamp,
}

/// A new export job (contract 11.1 and 12.8): what the request that creates it fixes. The row is also the queue
/// row its job claims: it starts pending, due at `created_at`, with no attempt.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewExport {
    /// The id.
    pub id: ExportJobId,
    /// The project whose data the export holds.
    pub project_id: ProjectId,
    /// The format.
    pub format: ExportFormat,
    /// What to export: JSON text, opaque to the store.
    pub selector: String,
    /// How to write it (gzip, truncated spans): JSON text, opaque to the store; the claim's payload.
    pub options: String,
    /// The queue's deduplication key: the job id, or the principal and its `Idempotency-Key`.
    pub dedup_key: String,
    /// Who created it, for example `user:<id>` or `api_key:<id>`.
    pub created_by: String,
    /// When it was created.
    pub created_at: Timestamp,
    /// The arrival bound of every read of the export: only spans received at or before it are exported.
    pub as_of: Timestamp,
}

/// An export job as stored (contract 11.1): the request's values, its claim state and its outcome.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExportJob {
    /// What the request fixed.
    pub new: NewExport,
    /// The claim status.
    pub status: ClaimStatus,
    /// The attempt counter; 0 before the first claim.
    pub attempts: u32,
    /// The database clock until which the current claim hides the row.
    pub lease_until: Option<Timestamp>,
    /// Spans written so far by the current attempt, or in all once the file is committed.
    pub spans: u64,
    /// Bytes written so far by the current attempt, or the file's length once it is committed.
    pub bytes: u64,
    /// Spans the format could not hold and left out (an `otlp-json` export skips truncated spans).
    pub skipped: u64,
    /// The committed file, relative to the exports root; `None` until the export is committed.
    pub file: Option<String>,
    /// SHA-256 of the committed file, lowercase hex.
    pub checksum: Option<String>,
    /// When the file stops being downloadable. Set at the commit, and earlier by a deletion request that
    /// the export predates.
    pub expires_at: Option<Timestamp>,
    /// Why the export ended without a file, when it did.
    pub error: Option<String>,
}

/// What a committed export holds (contract 12.8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExportCommit {
    /// The file, relative to the exports root.
    pub file: String,
    /// SHA-256 of the file, lowercase hex.
    pub checksum: String,
    /// Spans written.
    pub spans: u64,
    /// The file's length in bytes.
    pub bytes: u64,
    /// Spans left out.
    pub skipped: u64,
    /// When the file stops being downloadable.
    pub expires_at: Timestamp,
}

/// One applied step of the migrations ledger (contract 6.6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LedgerEntry {
    /// The step id, unique across kinds.
    pub id: String,
    /// The kind of step.
    pub kind: MigrationKind,
    /// When it was applied.
    pub applied_at: Timestamp,
    /// A checksum of what was applied.
    pub checksum: String,
}

/// Whole microseconds of a duration, for database intervals. Saturates.
pub fn duration_micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_enums_round_trip() {
        for role in OrgRole::ALL {
            assert_eq!(OrgRole::parse(role.as_str()), Some(*role));
        }
        for name in LeaseName::ALL {
            assert_eq!(LeaseName::parse(name.as_str()), Some(*name));
        }
        assert_eq!(KeyScope::parse("admin"), None);
        assert_eq!(
            MigrationKind::ClickhouseAccess.to_string(),
            "clickhouse_access"
        );
    }

    #[test]
    fn email_is_case_folded_and_shaped() {
        assert_eq!(
            Email::new("Ada@Example.COM").unwrap().as_str(),
            "ada@example.com"
        );
        for bad in [
            "",
            "ada",
            "@example.com",
            "ada@",
            "a b@example.com",
            "a@b@c",
        ] {
            assert!(Email::new(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn org_settings_round_trip_and_refuse_unknown_fields() {
        let settings = OrgSettings {
            allowed_email_domains: vec!["example.com".to_owned()],
            project_access: ProjectAccess::Explicit,
            ..OrgSettings::default()
        };
        assert_eq!(OrgSettings::parse(&settings.to_text()).unwrap(), settings);
        assert!(OrgSettings::parse(r#"{"signup_mode":"open"}"#).is_err());
        let extra = settings.to_text().replacen('{', r#"{"extra":1,"#, 1);
        assert!(OrgSettings::parse(&extra).is_err());
    }

    #[test]
    fn json_documents_compare_structurally() {
        let a = JsonDoc::parse(r#"{"a": 1, "b": [true]}"#).unwrap();
        let b = JsonDoc::parse(r#"{"b":[true],"a":1}"#).unwrap();
        assert_eq!(a, b);
        assert_eq!(JsonDoc::parse(&a.to_text()).unwrap(), a);
        assert!(JsonDoc::parse("{").is_err());
    }

    #[test]
    fn secrets_do_not_print() {
        let credential = Credential {
            user_id: UserId::default(),
            password_hash: "$argon2id$secret".to_owned(),
            params_version: 1,
            failed_count: 0,
            locked_until: None,
        };
        assert!(!format!("{credential:?}").contains("argon2id"));
    }
}
