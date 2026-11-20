//! Principal resolution and the permission extractors (contracts 11.4, 11.6 and 12.2).
//!
//! - **Keys.** An `Authorization` header means key auth only: the cookie is ignored and the request is exempt
//!   from CSRF. With a control plane the keys are the Postgres snapshot (a secret as Bearer, a key pair as Basic),
//!   whose outage answer is 503 with `Retry-After`; without one they are the static keys file.
//! - **Sessions.** Otherwise the session cookie, when the server has a control plane. A cookie-authenticated
//!   request other than GET and HEAD must pass the CSRF and origin checks.
//! - **[`Scoped<P>`]** is the only producer of `ProjectScope` in the API: it resolves the principal, parses
//!   `{project_id}`, and checks permission `P` (404 when the project is not visible, 403 when the role or scope
//!   is too low). [`OrgScoped<P>`] checks an org permission for sessions; [`Signed`] is any session;
//!   [`Prelogin`] guards the pre-session POSTs.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, ORIGIN};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crucible_api_types::ProblemCode;
use crucible_auth::roles::{Denied, OrgPermission, ProjectPermission};
use crucible_auth::service::{Identity, SessionPrincipal, SessionVerdict};
use crucible_auth::{KeyGrant, KeyVerdict, KeyVerifier, Scopes, csrf};
use crucible_cursor::KeyRing;
use crucible_env::Env;
use crucible_export::Exports;
use crucible_plan::Caller;
use crucible_query::{QueryService, ReadContext};
use crucible_records::{AuthorKind, ProjectId, ProjectScope, RequestId};
use crucible_sqlws::SqlWorkspace;

use crate::cursor::Cursors;
use crate::idempotency::Keys;
use crate::problem::{ApiError, request_id};
use crate::settings::{ProjectSettings, ProjectSettingsSource};
use crate::writer::{FactAuthor, FactWriter};

/// What every handler shares.
#[derive(Clone, Debug)]
pub(crate) struct ApiState {
    pub(crate) env: Env,
    /// `None` until a telemetry store is configured: requests are still authenticated and validated, then
    /// answered with `telemetry_store_unavailable`.
    pub(crate) service: Option<QueryService>,
    /// The keys file, when the server has no control plane.
    pub(crate) keys: Arc<dyn KeyVerifier>,
    pub(crate) settings: Arc<dyn ProjectSettingsSource>,
    /// The cursor-signing keys.
    pub(crate) cursors: Arc<KeyRing>,
    /// The control plane's identity, when Postgres is configured.
    pub(crate) identity: Option<Arc<Identity>>,
    /// Where fact writes go; `None` until a telemetry store is configured.
    pub(crate) facts: Option<Arc<dyn FactWriter>>,
    /// The `Idempotency-Key`s of fact writes.
    pub(crate) idempotency: Arc<Keys>,
    /// The SQL workspace; `None` when it is not configured.
    pub(crate) sql: Option<SqlWorkspace>,
    /// Alert rules and channels; `None` without a control plane.
    pub(crate) alerts: Option<crate::AlertsApi>,
    /// Retention and deletion; `None` without a control plane.
    pub(crate) lifecycle: Option<crucible_lifecycle::Service>,
    /// Exports; `None` without a control plane, which holds them.
    pub(crate) exports: Option<Exports>,
    /// Where dashboards are stored; `None` without a control plane, when only the built-in ones are served.
    pub(crate) dashboards: Option<Arc<dyn crucible_control::ControlStore>>,
    /// Media; `None` without an object store and a control plane.
    pub(crate) media: Option<crucible_media::Media>,
}

impl ApiState {
    /// The cursors of a request of `project`, served now.
    pub(crate) fn cursors(&self, project: &Project) -> Cursors<'_> {
        Cursors {
            ring: &self.cursors,
            project_id: project.ctx.scope().project_id(),
            now: self.env.clock.wall_now(),
        }
    }

    /// The query service, or the problem that says why there is none.
    pub(crate) fn service(&self, request_id: RequestId) -> Result<&QueryService, ApiError> {
        self.service.as_ref().ok_or_else(|| {
            ApiError::new(
                request_id,
                ProblemCode::TelemetryStoreUnavailable,
                "no telemetry store is configured",
            )
        })
    }

    /// Alert rules and channels, or `feature_disabled` with how to turn them on.
    pub(crate) fn alerts(&self, request_id: RequestId) -> Result<&crate::AlertsApi, ApiError> {
        self.alerts.as_ref().ok_or_else(|| {
            ApiError::new(
                request_id,
                ProblemCode::FeatureDisabled,
                "alerts and channels need a control plane and a master key; set postgres.url \
                 (CRUCIBLE_POSTGRES_URL) and CRUCIBLE_MASTER_KEY",
            )
        })
    }

    /// The control plane, or `feature_disabled` with how to turn it on.
    pub(crate) fn identity(&self, request_id: RequestId) -> Result<&Arc<Identity>, ApiError> {
        self.identity.as_ref().ok_or_else(|| {
            ApiError::new(
                request_id,
                ProblemCode::FeatureDisabled,
                "sign-in, members and API keys need a control plane; set postgres.url (CRUCIBLE_POSTGRES_URL)",
            )
        })
    }
}

/// Who a request comes from.
#[derive(Clone, Debug)]
pub(crate) enum Principal {
    /// An API key.
    Key(KeyGrant),
    /// A signed-in user.
    Session(Arc<SessionPrincipal>),
}

/// A presented key: a secret as Bearer, or a public and secret key pair as Basic.
enum Presented<'a> {
    Bearer(&'a str),
    Basic(String, String),
}

fn presented(value: &str) -> Option<Presented<'_>> {
    let (scheme, credential) = value.trim().split_once(' ')?;
    let credential = credential.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(Presented::Bearer(credential));
    }
    if !scheme.eq_ignore_ascii_case("basic") || credential.len() > 512 {
        return None;
    }
    let decoded = STANDARD.decode(credential).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (public, secret) = decoded.split_once(':')?;
    Some(Presented::Basic(public.to_owned(), secret.to_owned()))
}

fn header(headers: &HeaderMap, name: impl axum::http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The session cookie's value.
fn session_cookie<'a>(headers: &'a HeaderMap, identity: &Identity) -> Option<&'a str> {
    let name = identity.config().cookies().session();
    crucible_auth::cookie::find(
        headers
            .get_all(COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok()),
        name,
    )
}

/// The CSRF and origin checks of a cookie-authenticated request other than GET and HEAD.
fn check_csrf(
    parts: &Parts,
    identity: &Identity,
    principal: &SessionPrincipal,
    request_id: RequestId,
) -> Result<(), ApiError> {
    if matches!(parts.method, Method::GET | Method::HEAD) {
        return Ok(());
    }
    let headers = &parts.headers;
    identity
        .config()
        .origins
        .check(
            header(headers, ORIGIN),
            header(headers, HOST),
            header(headers, "sec-fetch-site"),
        )
        .map_err(|refusal| {
            ApiError::new(request_id, ProblemCode::OriginInvalid, refusal.to_string())
        })?;
    csrf::check_token(
        &principal.session.csrf_token,
        header(headers, csrf::CSRF_HEADER),
    )
    .map_err(|refusal| ApiError::new(request_id, ProblemCode::CsrfInvalid, refusal.to_string()))
}

/// Resolves the request's principal.
pub(crate) async fn resolve(
    parts: &Parts,
    state: &ApiState,
    request_id: RequestId,
) -> Result<Principal, ApiError> {
    if let Some(value) = header(&parts.headers, AUTHORIZATION) {
        let presented = presented(value).ok_or_else(|| ApiError::unauthenticated(request_id))?;
        return match &state.identity {
            Some(identity) => {
                let verdict = match &presented {
                    Presented::Bearer(secret) => identity.keys().check_secret(secret),
                    Presented::Basic(public, secret) => identity.keys().check_pair(public, secret),
                };
                match verdict {
                    KeyVerdict::Valid(grant) => Ok(Principal::Key(grant)),
                    KeyVerdict::Invalid => Err(ApiError::unauthenticated(request_id)),
                    KeyVerdict::Unavailable { retry_after } => Err(ApiError::new(
                        request_id,
                        ProblemCode::ControlPlaneUnavailable,
                        "the server cannot check this key while Postgres is unreachable; try again",
                    )
                    .with_retry_after(
                        u32::try_from(retry_after.as_secs().max(1)).unwrap_or(u32::MAX),
                    )),
                }
            }
            None => {
                let principal = match presented {
                    Presented::Bearer(secret) => state.keys.verify(secret),
                    Presented::Basic(..) => None,
                };
                principal
                    .map(|principal| {
                        Principal::Key(KeyGrant {
                            principal,
                            scopes: Scopes::DEFAULT,
                            import: false,
                        })
                    })
                    .ok_or_else(|| ApiError::unauthenticated(request_id))
            }
        };
    }
    let Some(identity) = &state.identity else {
        return Err(ApiError::unauthenticated(request_id));
    };
    let Some(token) = session_cookie(&parts.headers, identity) else {
        return Err(ApiError::unauthenticated(request_id));
    };
    match identity.session(token).await {
        SessionVerdict::Valid(principal) => {
            check_csrf(parts, identity, &principal, request_id)?;
            Ok(Principal::Session(principal))
        }
        SessionVerdict::Invalid => Err(ApiError::unauthenticated(request_id)),
        SessionVerdict::Unavailable => Err(ApiError::new(
            request_id,
            ProblemCode::ControlPlaneUnavailable,
            "the control plane (Postgres) is unavailable; try again shortly",
        )),
    }
}

/// A project permission marker.
pub(crate) trait Permission: Send + Sync {
    /// The permission.
    const PERMISSION: ProjectPermission;
}

/// Declares permission markers.
macro_rules! permissions {
    ($($(#[$meta:meta])* $name:ident => $permission:ident,)+) => {
        $(
            $(#[$meta])*
            #[derive(Debug)]
            pub(crate) struct $name;
            impl Permission for $name {
                const PERMISSION: ProjectPermission = ProjectPermission::$permission;
            }
        )+
    };
}

permissions! {
    /// Read telemetry and analytics.
    Read => Read,
    /// Write tags, flags, feedback and corrections.
    WriteFacts => WriteFacts,
    /// API keys, project settings and project members.
    Manage => Manage,
    /// Delete traces.
    DeleteTraces => DeleteTraces,
    /// Create and edit saved queries and alert rules. Keys never hold it.
    EditContent => EditContent,
    /// Run ad hoc SQL.
    RunSql => RunSql,
    /// Run a project-visible saved query with its declared parameters.
    RunSavedQuery => RunSavedQuery,
    /// Manage channels and test-send. Keys never hold it.
    Channels => Channels,
    /// Create exports.
    CreateExports => Export,
    /// Upload and confirm media (API keys with the `ingest` scope).
    IngestMedia => Ingest,
}

/// A request authorized for permission `P` on the project of its path (contract 11.6).
#[derive(Debug)]
pub(crate) struct Scoped<P> {
    pub(crate) request_id: RequestId,
    pub(crate) principal: Principal,
    pub(crate) scope: ProjectScope,
    /// The path parameters, `project_id` included.
    pub(crate) path: BTreeMap<String, String>,
    permission: PhantomData<P>,
}

impl<P: Permission> FromRequestParts<ApiState> for Scoped<P> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let request_id = request_id(&parts.extensions, &state.env);
        let principal = resolve(parts, state, request_id).await?;
        let Path(path) = Path::<BTreeMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::not_found(request_id))?;
        let project_id = path
            .get("project_id")
            .and_then(|id| ProjectId::parse(id).ok())
            .ok_or_else(|| ApiError::not_found(request_id))?;
        let scope = match &principal {
            Principal::Key(grant) => grant.authorize(project_id, P::PERMISSION),
            Principal::Session(session) => session.authorize(project_id, P::PERMISSION),
        }
        .map_err(|denied| match denied {
            Denied::NotVisible => ApiError::not_found(request_id),
            Denied::TooLow => ApiError::new(
                request_id,
                ProblemCode::Forbidden,
                format!(
                    "this needs the `{}` permission on the project; ask a project admin",
                    P::PERMISSION.name()
                ),
            ),
        })?;
        Ok(Self {
            request_id,
            principal,
            scope,
            path,
            permission: PhantomData,
        })
    }
}

/// A read of one project's telemetry: [`Scoped`] with [`Read`], in the read class of its caller.
#[derive(Debug)]
pub(crate) struct Project {
    pub(crate) request_id: RequestId,
    /// Who the request comes from: the author of the facts it writes, the key or the signed-in user.
    pub(crate) author: FactAuthor,
    pub(crate) ctx: ReadContext,
    pub(crate) settings: ProjectSettings,
    /// The path parameters, `project_id` included.
    pub(crate) path: BTreeMap<String, String>,
}

impl Project {
    async fn authorized<P: Permission>(
        parts: &mut Parts,
        state: &ApiState,
    ) -> Result<Self, ApiError> {
        let scoped = Scoped::<P>::from_request_parts(parts, state).await?;
        let settings = state.settings.settings(scoped.scope.project_id());
        let (caller, author) = match &scoped.principal {
            Principal::Key(grant) => (
                Caller::Agent,
                FactAuthor {
                    kind: AuthorKind::ApiKey,
                    id: grant.principal.api_key_id.uuid(),
                },
            ),
            Principal::Session(session) => (
                Caller::Session,
                FactAuthor {
                    kind: AuthorKind::User,
                    id: session.user.id.uuid(),
                },
            ),
        };
        Ok(Self {
            request_id: scoped.request_id,
            author,
            ctx: ReadContext::new(scoped.scope, settings.retention, caller)
                .with_floors(settings.floors),
            settings,
            path: scoped.path,
        })
    }
}

impl FromRequestParts<ApiState> for Project {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        Self::authorized::<Read>(parts, state).await
    }
}

/// A fact write to one project: [`Project`] with [`WriteFacts`] instead of [`Read`].
#[derive(Debug)]
pub(crate) struct ProjectWrite(pub(crate) Project);

impl FromRequestParts<ApiState> for ProjectWrite {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        Project::authorized::<WriteFacts>(parts, state)
            .await
            .map(Self)
    }
}

/// A signed-in user, any role.
#[derive(Debug)]
pub(crate) struct Signed {
    pub(crate) request_id: RequestId,
    pub(crate) identity: Arc<Identity>,
    pub(crate) principal: Arc<SessionPrincipal>,
}

impl FromRequestParts<ApiState> for Signed {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let request_id = request_id(&parts.extensions, &state.env);
        let identity = Arc::clone(state.identity(request_id)?);
        match resolve(parts, state, request_id).await? {
            Principal::Session(principal) => Ok(Self {
                request_id,
                identity,
                principal,
            }),
            Principal::Key(_) => Err(ApiError::new(
                request_id,
                ProblemCode::Forbidden,
                "API keys cannot manage accounts, members or settings; sign in to the UI",
            )),
        }
    }
}

/// An org permission marker.
pub(crate) trait OrgGate: Send + Sync {
    /// The permission.
    const PERMISSION: OrgPermission;
}

/// Declares org permission markers.
macro_rules! org_permissions {
    ($($(#[$meta:meta])* $name:ident => $permission:ident,)+) => {
        $(
            $(#[$meta])*
            #[derive(Debug)]
            pub(crate) struct $name;
            impl OrgGate for $name {
                const PERMISSION: OrgPermission = OrgPermission::$permission;
            }
        )+
    };
}

org_permissions! {
    /// Read the organization and list members and invitations.
    OrgRead => Read,
    /// Org settings and sign-in providers.
    OrgSettings => Settings,
    /// Members and invitations.
    OrgMembers => Members,
    /// Create and delete projects.
    OrgProjects => Projects,
    /// Transfer ownership.
    OrgOwner => TransferOwnership,
}

/// A signed-in user with org permission `P`.
#[derive(Debug)]
pub(crate) struct OrgScoped<P> {
    pub(crate) request_id: RequestId,
    pub(crate) identity: Arc<Identity>,
    pub(crate) principal: Arc<SessionPrincipal>,
    permission: PhantomData<P>,
}

impl<P: OrgGate> FromRequestParts<ApiState> for OrgScoped<P> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let signed = Signed::from_request_parts(parts, state).await?;
        if !P::PERMISSION.allowed_for(signed.principal.org_role) {
            return Err(ApiError::new(
                signed.request_id,
                ProblemCode::Forbidden,
                format!(
                    "this needs the `{}` org permission; ask an org admin",
                    P::PERMISSION.name()
                ),
            ));
        }
        Ok(Self {
            request_id: signed.request_id,
            identity: signed.identity,
            principal: signed.principal,
            permission: PhantomData,
        })
    }
}

/// A pre-session POST (login, password reset, invitation acceptance, first-owner setup): the origin checks
/// and `X-Crucible-CSRF: prelogin` (contract 11.4).
#[derive(Debug)]
pub(crate) struct Prelogin {
    pub(crate) request_id: RequestId,
    pub(crate) identity: Arc<Identity>,
}

impl FromRequestParts<ApiState> for Prelogin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let request_id = request_id(&parts.extensions, &state.env);
        let identity = Arc::clone(state.identity(request_id)?);
        let headers = &parts.headers;
        identity
            .config()
            .origins
            .check(
                header(headers, ORIGIN),
                header(headers, HOST),
                header(headers, "sec-fetch-site"),
            )
            .map_err(|r| ApiError::new(request_id, ProblemCode::OriginInvalid, r.to_string()))?;
        csrf::check_prelogin(header(headers, csrf::CSRF_HEADER))
            .map_err(|r| ApiError::new(request_id, ProblemCode::CsrfInvalid, r.to_string()))?;
        Ok(Self {
            request_id,
            identity,
        })
    }
}

/// The control plane of a public route.
#[derive(Debug)]
pub(crate) struct Public {
    pub(crate) request_id: RequestId,
    pub(crate) identity: Arc<Identity>,
}

impl FromRequestParts<ApiState> for Public {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let request_id = request_id(&parts.extensions, &state.env);
        let identity = Arc::clone(state.identity(request_id)?);
        Ok(Self {
            request_id,
            identity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_and_basic_are_read() {
        assert!(matches!(
            presented("Bearer csk_1"),
            Some(Presented::Bearer("csk_1"))
        ));
        assert!(matches!(
            presented("bearer  csk_1 "),
            Some(Presented::Bearer("csk_1"))
        ));
        let basic = format!("Basic {}", STANDARD.encode("cpk_1:csk_2"));
        match presented(&basic) {
            Some(Presented::Basic(public, secret)) => {
                assert_eq!((public.as_str(), secret.as_str()), ("cpk_1", "csk_2"));
            }
            _ => panic!("basic"),
        }
        for bad in ["Bearer", "csk_1", "Basic !!!", "Digest x", "Basic Y3BrMQ=="] {
            assert!(presented(bad).is_none(), "{bad}");
        }
    }
}
