//! The control plane's domain types and the `ControlStore` repository traits (contracts 2.7 and 11).
//!
//! This crate is pure: no `Env`, no IO. `crucible-control-pg` implements the traits over Postgres and
//! `crucible-sim` over an in-memory model (`PgModel`); `crucible-env` carries the store in `Stores`.
//!
//! - [`Repos`] and the per-aggregate repository traits; [`ControlStore`], [`ControlTx`] and [`transaction`].
//! - [`TxError`], the one typed error.
//! - [`HeldLease`], the holder's local validity rule for leases.
//! - [`LifecycleRepo`] and its rows: the node registry, deletion requests, leader actions and retention floors.
//! - [`DashboardRepo`]: dashboards and their widgets.
//! - [`MediaRepo`] and its rows: media objects and their references.

mod alerts;
mod dashboards;
mod error;
mod lease;
mod sqlws;
// Declared before `lifecycle`, which uses its `text_enum!`.
#[macro_use]
mod types;
mod lifecycle;
mod media;
mod repo;

pub use alerts::{
    AlertEvent, AlertEventKind, AlertEventRepo, AlertRule, AlertRuleRepo, AlertSeriesState,
    AlertStateRepo, AlertStatus, Channel, ChannelKind, ChannelRepo,
};
pub use dashboards::{Dashboard, DashboardEntry, DashboardRepo, DashboardRows, Widget};
pub use error::TxError;
pub use lease::{HeldLease, LEASE_RENEW_EVERY, LEASE_SAFETY_MARGIN, LEASE_TTL};
pub use lifecycle::{
    ActionKind, ActionState, DeletionRequest, DeletionSelector, LIVE_WITHIN, LifecycleAction,
    LifecycleRepo, MAX_DELETION_TRACES, Node, NodeAck, PurgedRows, RetentionFloor,
};
pub use media::{MediaObject, MediaRef, MediaRepo, MediaState, MediaUse};
pub use repo::{
    ApiKeyRepo, AuthFlowRepo, AuthProviderRepo, Claims, ControlStore, ControlTx, CredentialRepo,
    ExportJobRepo, InvitationRepo, KeyFeed, Leases, MembershipRepo, Migrations, OauthAccountRepo,
    OneTimeTokenRepo, OrgRepo, ProjectRepo, ProjectSaltRepo, Repos, SecretRepo, SessionRepo,
    SigningKeyRepo, TRANSACTION_CAP, UserRepo, transaction,
};
pub use sqlws::{
    AppliedRetention, SavedParam, SavedQuery, SavedQueryRepo, SqlActor, SqlIdentity,
    SqlIdentityRepo, SqlIdentityState, SqlParamType, SqlRun, SqlRunEnd, SqlRunKind, SqlRunOutcome,
    SqlRunRepo, SqlVisibility,
};
pub use types::{
    ApiKey, AuthFlow, AuthMethod, AuthProvider, Claim, ClaimStatus, Credential, Email,
    ExportCommit, ExportFormat, ExportJob, Invitation, JsonDoc, KeyChange, KeyChangeKind, KeyScope,
    KeySnapshot, Lease, LeaseName, LedgerEntry, MigrationKind, NewExport, NewJob, OauthAccount,
    OneTimeToken, Org, OrgMembership, OrgRole, OrgSettings, Project, ProjectAccess,
    ProjectMembership, ProjectRole, ProviderKind, Queue, SealedProjectSalt, Secret, Session,
    SigningKey, SigningKeyState, SigningPurpose, SignupMode, TokenHash, TokenKind, User,
    UserStatus, duration_micros,
};
