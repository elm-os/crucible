//! The control-plane side of identity (contract 11): sessions with their cache, password sign-in, first-owner
//! setup, invitations, password changes and resets, provider sign-in, the organization, projects, members and
//! API keys. The API layer calls [`Identity`] and maps [`AuthError`] to problem documents.

mod account;
mod admin;
mod provider_flow;
mod sessions;

use std::sync::Arc;
use std::time::Duration;

use crucible_control::{ControlStore, Org, TxError};
use crucible_env::{Env, TlsRoots};

pub use account::{NewSession, SetupLink};
pub use admin::{
    ApiKeyChange, InvitationCreated, KeyCreated, OrgChange, ProviderChange, ProviderInput, Seeded,
};
pub use provider_flow::{CallbackQuery, FlowFailure, FlowStart, SignInView, SignedIn};
pub use sessions::{SessionPrincipal, SessionVerdict};

use crate::csrf::Origins;
use crate::db_keys::DbKeys;
use crate::http::NetHttp;
use crate::mint::LookupKey;
use crate::password::{HashCost, Passwords};
use crate::seal::Sealer;

/// Idle session expiry (contract 11.4).
pub const SESSION_IDLE: Duration = Duration::from_secs(7 * 86_400);
/// Absolute session expiry.
pub const SESSION_ABSOLUTE: Duration = Duration::from_secs(30 * 86_400);
/// Sliding the idle expiry is written at most this often.
pub const TOUCH_EVERY: Duration = Duration::from_secs(60);
/// How long a node trusts its cached sessions and memberships while Postgres answers.
pub const CACHE_FOR: Duration = Duration::from_secs(5);
/// `auth.session_outage_grace` by default.
pub const OUTAGE_GRACE: Duration = Duration::from_secs(600);
/// Invitations expire after this long.
pub const INVITATION_TTL: Duration = Duration::from_secs(7 * 86_400);
/// Password-reset links expire after this long.
pub const RESET_TTL: Duration = Duration::from_secs(30 * 60);
/// The first-owner setup token expires after this long; a restart issues a new one.
pub const SETUP_TTL: Duration = Duration::from_secs(86_400);
/// A sign-in flow expires after this long.
pub const FLOW_TTL: Duration = Duration::from_secs(600);

/// The settings identity runs with.
#[derive(Clone, Debug)]
pub struct IdentityConfig {
    /// The origins a cookie-authenticated request may come from.
    pub origins: Origins,
    /// `server.public_url`: provider sign-in needs it for its callback URL.
    pub public_url: Option<String>,
    /// `auth.session_outage_grace`: how long cached sessions stay valid while Postgres is unreachable.
    pub session_outage_grace: Duration,
    /// `security.require_https`: refuse sign-in unless the public URL is HTTPS.
    pub require_https: bool,
    /// Argon2id costs.
    pub hash_cost: HashCost,
    /// Concurrent password hashes: min(cores, 4).
    pub hash_permits: usize,
    /// The trust anchors of provider calls.
    pub tls_roots: TlsRoots,
    /// The sign-in providers of the server configuration, at most one per kind (contract 11.1: seeded rows, which
    /// the API cannot change). Their client secrets stay here and are never stored.
    pub providers: Vec<ProviderInput>,
}

impl IdentityConfig {
    /// The defaults of the contract, with `origins` and `public_url`.
    pub fn new(origins: Origins, public_url: Option<String>) -> Self {
        Self {
            origins,
            public_url,
            session_outage_grace: OUTAGE_GRACE,
            require_https: false,
            hash_cost: HashCost::CONTRACT,
            hash_permits: 4,
            tls_roots: TlsRoots::WebPki,
            providers: Vec::new(),
        }
    }

    /// Whether cookies are for an HTTPS origin.
    pub fn cookies(&self) -> crate::cookie::Cookies {
        crate::cookie::Cookies {
            secure: self.origins.public_is_https(),
        }
    }
}

/// Why an identity call failed. The API maps each to a problem code (contract 12.3); every message says what to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No or invalid session (`unauthenticated`).
    #[error("sign in again: the session is missing, expired or revoked")]
    Unauthenticated,
    /// Wrong email or password (`invalid_credentials`).
    #[error("the email or password is wrong")]
    InvalidCredentials,
    /// The caller may not do this (`forbidden`).
    #[error("{0}")]
    Forbidden(String),
    /// No such entity, or one the caller cannot see (`not_found`).
    #[error("no resource exists at this path")]
    NotFound,
    /// A unique or state conflict (`conflict`).
    #[error("{0}")]
    Conflict(String),
    /// A field is invalid (`validation_failed`): the field and why.
    #[error("{field}: {message}")]
    Invalid {
        /// The JSON field.
        field: &'static str,
        /// What is wrong and what is accepted.
        message: String,
    },
    /// A reset, invitation or setup token is invalid, expired or used (`token_invalid`).
    #[error("the link is invalid, expired or already used; ask for a new one")]
    TokenInvalid,
    /// `If-Match` names an older version (`precondition_failed`).
    #[error("the resource changed since it was read; reload it and try again")]
    PreconditionFailed,
    /// Every password-hashing slot is busy (`rate_limited`).
    #[error("too many sign-ins at once; try again in a second")]
    Busy,
    /// Postgres is unreachable (`control_plane_unavailable`).
    #[error("the control plane (Postgres) is unavailable; try again shortly")]
    Unavailable,
    /// Anything else (`internal_error`); the cause goes to the log.
    #[error("internal error")]
    Internal(String),
}

impl From<TxError> for AuthError {
    fn from(error: TxError) -> Self {
        match error {
            TxError::Unavailable(_) | TxError::SerializationFailure | TxError::CommitUnknown => {
                Self::Unavailable
            }
            TxError::UniqueViolation(constraint) => {
                Self::Conflict(format!("that already exists ({constraint})"))
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<crate::password::PasswordError> for AuthError {
    fn from(error: crate::password::PasswordError) -> Self {
        match error {
            crate::password::PasswordError::Busy => Self::Busy,
            crate::password::PasswordError::Cancelled => Self::Unavailable,
            crate::password::PasswordError::Failed(e) => Self::Internal(e),
        }
    }
}

/// Identity over a control store.
pub struct Identity {
    env: Env,
    store: Arc<dyn ControlStore>,
    passwords: Passwords,
    sealer: Sealer,
    lookup: LookupKey,
    keys: Arc<DbKeys>,
    sessions: sessions::Cache,
    config: IdentityConfig,
    http: NetHttp,
    /// The last sign-in view read from Postgres, served while Postgres is unreachable.
    last_view: std::sync::Mutex<Option<SignInView>>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Identity over `store`, sealing with `sealer`, verifying keys with `keys` (whose lookup subkey is `lookup`).
    pub fn new(
        env: &Env,
        store: Arc<dyn ControlStore>,
        sealer: Sealer,
        lookup: LookupKey,
        keys: Arc<DbKeys>,
        config: IdentityConfig,
    ) -> Result<Self, AuthError> {
        let passwords = Passwords::new(env, config.hash_cost, config.hash_permits)?;
        Ok(Self {
            env: env.clone(),
            http: NetHttp::new(env, config.tls_roots.clone()),
            store,
            passwords,
            sealer,
            lookup,
            keys,
            sessions: sessions::Cache::default(),
            config,
            last_view: std::sync::Mutex::new(None),
        })
    }

    /// The settings.
    pub fn config(&self) -> &IdentityConfig {
        &self.config
    }

    /// The key snapshot.
    pub fn keys(&self) -> &Arc<DbKeys> {
        &self.keys
    }

    /// The organization, or `None` before first-owner setup.
    pub async fn org(&self) -> Result<Option<Org>, AuthError> {
        Ok(self.store.org().get().await?)
    }

    /// Refuses sign-in over plain HTTP when `security.require_https` is on (contract 11.4).
    fn https_required(&self) -> Result<(), AuthError> {
        if self.config.require_https && !self.config.origins.public_is_https() {
            return Err(AuthError::Forbidden(
                "sign-in needs HTTPS on this server: open it at its https:// address (server.public_url)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn now(&self) -> crucible_records::Timestamp {
        self.env.clock.wall_now()
    }

    fn later(&self, d: Duration) -> crucible_records::Timestamp {
        self.now()
            .saturating_add_micros(i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
mod tests;
