//! The control plane's domain types and the `ControlStore` repository traits (contracts 2.7 and 11).
//!
//! This crate is pure: no `Env`, no IO. `crucible-control-pg` implements the traits over Postgres and
//! `crucible-sim` over an in-memory model (`PgModel`); `crucible-env` carries the store in `Stores`.
//!
//! - [`Repos`] and the per-aggregate repository traits; [`ControlStore`], [`ControlTx`] and [`transaction`].
//! - [`TxError`], the one typed error.
//! - [`HeldLease`], the holder's local validity rule for leases.

mod error;
mod lease;
mod repo;
mod types;

pub use error::TxError;
pub use lease::{HeldLease, LEASE_RENEW_EVERY, LEASE_SAFETY_MARGIN, LEASE_TTL};
pub use repo::{
    ApiKeyRepo, AuthFlowRepo, AuthProviderRepo, Claims, ControlStore, ControlTx, CredentialRepo,
    ExportJobRepo, InvitationRepo, KeyFeed, Leases, MembershipRepo, Migrations, OauthAccountRepo,
    OneTimeTokenRepo, OrgRepo, ProjectRepo, Repos, SecretRepo, SessionRepo, SigningKeyRepo,
    TRANSACTION_CAP, UserRepo, transaction,
};
pub use types::{
    ApiKey, AuthFlow, AuthMethod, AuthProvider, Claim, ClaimStatus, Credential, Email, ExportCommit,
    ExportFormat, ExportJob, Invitation, JsonDoc,
    KeyChange, KeyChangeKind, KeyScope, KeySnapshot, Lease, LeaseName, LedgerEntry, MigrationKind,
    NewExport, NewJob, OauthAccount, OneTimeToken, Org, OrgMembership, OrgRole, OrgSettings, Project,
    ProjectAccess, ProjectMembership, ProjectRole, ProviderKind, Queue, Secret, Session,
    SigningKey, SigningKeyState, SigningPurpose, SignupMode, TokenHash, TokenKind, User,
    UserStatus, duration_micros,
};
