//! Session lookup with the node cache (contract 11.4). A session is read from Postgres at most every 5 s per node,
//! with the user, the org role, the memberships and the visible projects. While Postgres is unreachable a cached
//! session stays valid for `auth.session_outage_grace`, so open UIs keep reading telemetry; a revocation made
//! before the outage still applies, because the cache refreshes every 5 s while Postgres is up.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use crucible_control::{
    Org, OrgRole, Project, ProjectMembership, ProjectRole, Session, TokenHash, User, UserStatus,
};
use crucible_env::Instant;
use crucible_records::{ProjectId, ProjectScope};

use super::{AuthError, CACHE_FOR, Identity, SESSION_IDLE, TOUCH_EVERY};
use crate::roles::{Denied, ProjectPermission, effective_project_role};
use crate::token;

/// A signed-in user, as one request sees them.
#[derive(Clone, Debug)]
pub struct SessionPrincipal {
    /// The hash of the session token.
    pub token_hash: TokenHash,
    /// The session row.
    pub session: Session,
    /// The user.
    pub user: User,
    /// The organization.
    pub org: Org,
    /// The user's org role.
    pub org_role: OrgRole,
    /// The projects the user can see, with the effective role on each.
    pub projects: BTreeMap<ProjectId, (Project, ProjectRole)>,
}

impl SessionPrincipal {
    /// The user's effective role on a project; `None` when it is not visible to them.
    pub fn project_role(&self, project_id: ProjectId) -> Option<ProjectRole> {
        self.projects.get(&project_id).map(|(_, role)| *role)
    }

    /// The scope of a request to `project_id` that needs `permission` (contract 11.6): a project the user cannot
    /// see is invisible, and a role below the permission is too low. The only way a session obtains a scope.
    pub fn authorize(
        &self,
        project_id: ProjectId,
        permission: ProjectPermission,
    ) -> Result<ProjectScope, Denied> {
        let role = self.project_role(project_id).ok_or(Denied::NotVisible)?;
        if !permission.allowed_for(role) {
            return Err(Denied::TooLow);
        }
        crate::scope::mint(project_id).map_err(|_| Denied::NotVisible)
    }
}

/// The answer to a presented session token.
#[derive(Clone, Debug)]
pub enum SessionVerdict {
    /// A live session.
    Valid(Arc<SessionPrincipal>),
    /// No live session: 401.
    Invalid,
    /// Postgres is unreachable and the cache cannot answer: 503.
    Unavailable,
}

/// Sessions read recently, by token hash, with when they were read.
#[derive(Debug, Default)]
pub(super) struct Cache {
    entries: Mutex<BTreeMap<TokenHash, (Arc<SessionPrincipal>, Instant)>>,
}

impl Cache {
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<TokenHash, (Arc<SessionPrincipal>, Instant)>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Identity {
    /// The live session of a session token (the cookie's value).
    pub async fn session(&self, token: &str) -> SessionVerdict {
        if !token::well_formed(token) {
            return SessionVerdict::Invalid;
        }
        let hash = token::hash(token);
        let now = self.env.clock.now();
        let cached = self.sessions.lock().get(&hash).cloned();
        if let Some((principal, read_at)) = &cached
            && now.saturating_duration_since(*read_at) < CACHE_FOR
        {
            return self.live(principal);
        }
        match self.load(hash).await {
            Ok(Some(principal)) => {
                let principal = Arc::new(principal);
                self.remember(&principal);
                self.live(&principal)
            }
            Ok(None) => {
                self.sessions.lock().remove(&hash);
                SessionVerdict::Invalid
            }
            Err(AuthError::Unavailable) => match cached {
                Some((principal, read_at))
                    if now.saturating_duration_since(read_at)
                        < self.config.session_outage_grace =>
                {
                    self.live(&principal)
                }
                _ => SessionVerdict::Unavailable,
            },
            Err(error) => {
                tracing::error!(%error, "reading a session failed");
                SessionVerdict::Unavailable
            }
        }
    }

    /// Whether the cached session is still within its expiry at the wall clock.
    fn live(&self, principal: &Arc<SessionPrincipal>) -> SessionVerdict {
        let now = self.now();
        let session = &principal.session;
        if session.revoked_at.is_some()
            || session.idle_expires_at <= now
            || session.absolute_expires_at <= now
        {
            SessionVerdict::Invalid
        } else {
            SessionVerdict::Valid(Arc::clone(principal))
        }
    }

    pub(super) fn remember(&self, principal: &Arc<SessionPrincipal>) {
        let now = self.env.clock.now();
        let grace = self.config.session_outage_grace.max(CACHE_FOR);
        let mut entries = self.sessions.lock();
        entries.retain(|_, (_, read_at)| now.saturating_duration_since(*read_at) < grace);
        entries.insert(principal.token_hash, (Arc::clone(principal), now));
    }

    /// Drops one session from this node's cache.
    pub(super) fn forget(&self, hash: &TokenHash) {
        self.sessions.lock().remove(hash);
    }

    /// Drops every cached session, after a change on this node to roles, memberships or projects, so the change
    /// applies here at once (other nodes see it within 5 s).
    pub fn forget_sessions(&self) {
        self.sessions.lock().clear();
    }

    /// Reads a session with everything a request needs; `None` when it is not live.
    pub(super) async fn load(
        &self,
        hash: TokenHash,
    ) -> Result<Option<SessionPrincipal>, AuthError> {
        let store = self.store.as_ref();
        let Some(mut session) = store.sessions().get(&hash).await? else {
            return Ok(None);
        };
        let now = self.now();
        if session.revoked_at.is_some()
            || session.idle_expires_at <= now
            || session.absolute_expires_at <= now
        {
            return Ok(None);
        }
        let Some(user) = store.users().get(session.user_id).await? else {
            return Ok(None);
        };
        if user.status != UserStatus::Active {
            return Ok(None);
        }
        let (Some(org), Some(org_role)) = (
            store.org().get().await?,
            store.memberships().org_role(user.id).await?,
        ) else {
            return Ok(None);
        };
        let memberships = store.memberships().list_for_user(user.id).await?;
        let projects = visible(&org, org_role, &memberships, store.projects().list().await?);
        // Slide the idle expiry, at most once a minute.
        let touched_at = session
            .idle_expires_at
            .saturating_add_micros(-micros(SESSION_IDLE));
        if now.micros() - touched_at.micros() >= micros(TOUCH_EVERY) {
            let idle = self.later(SESSION_IDLE).min(session.absolute_expires_at);
            if store.sessions().touch(&hash, idle).await.unwrap_or(false) {
                session.idle_expires_at = idle;
            }
        }
        Ok(Some(SessionPrincipal {
            token_hash: hash,
            session,
            user,
            org,
            org_role,
            projects,
        }))
    }
}

/// The projects visible to a member, with the effective role on each (contract 11.6).
pub(super) fn visible(
    org: &Org,
    org_role: OrgRole,
    memberships: &[ProjectMembership],
    projects: Vec<Project>,
) -> BTreeMap<ProjectId, (Project, ProjectRole)> {
    projects
        .into_iter()
        .filter(|project| project.deleted_at.is_none())
        .filter_map(|project| {
            let membership = memberships
                .iter()
                .find(|m| m.project_id == project.id)
                .map(|m| m.role);
            let role = effective_project_role(org_role, org.settings.project_access, membership)?;
            Some((project.id, (project, role)))
        })
        .collect()
}

pub(super) fn micros(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}
