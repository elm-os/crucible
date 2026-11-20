//! What the control-plane routes share: the JSON body extractor, `If-Match` and `ETag`, cookies, and the
//! conversions from domain rows to DTOs (contract 12.5 and 12.7).

use axum::extract::{FromRequest, Request};
use axum::http::header::{CONTENT_TYPE, ETAG, IF_MATCH, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use bytes::Bytes;
use crucible_api_types as wire;
use crucible_api_types::ProblemCode;
use crucible_auth::roles::permission_names;
use crucible_auth::service::{AuthError, Identity, SessionPrincipal};
use crucible_control as control;
use crucible_records::{RequestId, Timestamp};
use crucible_sqlws::SqlWorkspace;
use serde::de::DeserializeOwned;

use crate::access::ApiState;
use crate::problem::{ApiError, field_error, request_id};

/// A JSON request body; a body that is not JSON of the expected shape is `validation_failed` (or
/// `unsupported_media_type` without a JSON content type).
#[derive(Debug)]
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T: DeserializeOwned> FromRequest<ApiState> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &ApiState) -> Result<Self, ApiError> {
        let rid = request_id(request.extensions(), &state.env);
        let json = request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|t| t.trim() == "application/json")
            });
        if !json {
            return Err(ApiError::new(
                rid,
                ProblemCode::UnsupportedMediaType,
                "send the body as application/json",
            ));
        }
        let bytes = Bytes::from_request(request, state).await.map_err(|_| {
            ApiError::new(
                rid,
                ProblemCode::PayloadTooLarge,
                "the request body is too large",
            )
        })?;
        serde_json::from_slice(&bytes).map(Self).map_err(|error| {
            ApiError::validation(
                rid,
                vec![field_error("", "invalid_body", &error.to_string())],
            )
        })
    }
}

/// The version an `If-Match` header names: `"<version>"`. Missing is `precondition_required`.
pub(crate) fn if_match(headers: &HeaderMap, rid: RequestId) -> Result<u64, ApiError> {
    let value = headers.get(IF_MATCH).ok_or_else(|| {
        ApiError::new(
            rid,
            ProblemCode::PreconditionRequired,
            "send If-Match with the ETag of the version you read",
        )
    })?;
    value
        .to_str()
        .ok()
        .map(|v| v.trim().trim_start_matches("W/").trim_matches('"'))
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            ApiError::new(
                rid,
                ProblemCode::PreconditionFailed,
                "If-Match must be the ETag of the version you read",
            )
        })
}

/// Headers with the `ETag` of `version`.
pub(crate) fn etag(version: u64) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&format!("\"{version}\"")) {
        headers.insert(ETAG, value);
    }
    headers
}

/// Appends `Set-Cookie` values.
pub(crate) fn set_cookies(headers: &mut HeaderMap, cookies: &[String]) {
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            headers.append(SET_COOKIE, value);
        }
    }
}

/// The problem of an identity call made by request `rid`.
pub(crate) fn auth(rid: RequestId) -> impl FnOnce(AuthError) -> ApiError {
    move |error| ApiError::auth(rid, error)
}

/// A version as the wire carries it.
pub(crate) fn version(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

fn time(t: Timestamp) -> String {
    t.to_string()
}

fn opt_time(t: Option<Timestamp>) -> Option<String> {
    t.map(time)
}

pub(crate) fn org_role(role: control::OrgRole) -> wire::OrgRole {
    match role {
        control::OrgRole::Owner => wire::OrgRole::Owner,
        control::OrgRole::Admin => wire::OrgRole::Admin,
        control::OrgRole::Member => wire::OrgRole::Member,
        control::OrgRole::Viewer => wire::OrgRole::Viewer,
    }
}

pub(crate) fn org_role_in(role: wire::OrgRole) -> control::OrgRole {
    match role {
        wire::OrgRole::Owner => control::OrgRole::Owner,
        wire::OrgRole::Admin => control::OrgRole::Admin,
        wire::OrgRole::Member => control::OrgRole::Member,
        wire::OrgRole::Viewer => control::OrgRole::Viewer,
    }
}

pub(crate) fn project_role(role: control::ProjectRole) -> wire::ProjectRole {
    match role {
        control::ProjectRole::Admin => wire::ProjectRole::Admin,
        control::ProjectRole::Member => wire::ProjectRole::Member,
        control::ProjectRole::Viewer => wire::ProjectRole::Viewer,
    }
}

pub(crate) fn project_role_in(role: wire::ProjectRole) -> control::ProjectRole {
    match role {
        wire::ProjectRole::Admin => control::ProjectRole::Admin,
        wire::ProjectRole::Member => control::ProjectRole::Member,
        wire::ProjectRole::Viewer => control::ProjectRole::Viewer,
    }
}

pub(crate) fn scope(scope: control::KeyScope) -> wire::KeyScope {
    match scope {
        control::KeyScope::Ingest => wire::KeyScope::Ingest,
        control::KeyScope::Read => wire::KeyScope::Read,
        control::KeyScope::Write => wire::KeyScope::Write,
        control::KeyScope::Delete => wire::KeyScope::Delete,
        control::KeyScope::Sql => wire::KeyScope::Sql,
    }
}

pub(crate) fn scope_in(scope: wire::KeyScope) -> control::KeyScope {
    match scope {
        wire::KeyScope::Ingest => control::KeyScope::Ingest,
        wire::KeyScope::Read => control::KeyScope::Read,
        wire::KeyScope::Write => control::KeyScope::Write,
        wire::KeyScope::Delete => control::KeyScope::Delete,
        wire::KeyScope::Sql => control::KeyScope::Sql,
    }
}

pub(crate) fn kind(kind: control::ProviderKind) -> wire::ProviderKind {
    match kind {
        control::ProviderKind::Github => wire::ProviderKind::Github,
        control::ProviderKind::Google => wire::ProviderKind::Google,
        control::ProviderKind::Oidc => wire::ProviderKind::Oidc,
    }
}

pub(crate) fn kind_in(kind: wire::ProviderKind) -> control::ProviderKind {
    match kind {
        wire::ProviderKind::Github => control::ProviderKind::Github,
        wire::ProviderKind::Google => control::ProviderKind::Google,
        wire::ProviderKind::Oidc => control::ProviderKind::Oidc,
    }
}

pub(crate) fn access(access: control::ProjectAccess) -> wire::ProjectAccess {
    match access {
        control::ProjectAccess::All => wire::ProjectAccess::All,
        control::ProjectAccess::Explicit => wire::ProjectAccess::Explicit,
    }
}

pub(crate) fn access_in(access: wire::ProjectAccess) -> control::ProjectAccess {
    match access {
        wire::ProjectAccess::All => control::ProjectAccess::All,
        wire::ProjectAccess::Explicit => control::ProjectAccess::Explicit,
    }
}

fn method(method: control::AuthMethod) -> wire::AuthMethod {
    match method {
        control::AuthMethod::Password => wire::AuthMethod::Password,
        control::AuthMethod::Github => wire::AuthMethod::Github,
        control::AuthMethod::Google => wire::AuthMethod::Google,
        control::AuthMethod::Oidc => wire::AuthMethod::Oidc,
    }
}

pub(crate) fn status(status: control::UserStatus) -> wire::UserStatus {
    match status {
        control::UserStatus::Active => wire::UserStatus::Active,
        control::UserStatus::Disabled => wire::UserStatus::Disabled,
    }
}

pub(crate) fn button(provider: &control::AuthProvider) -> wire::ProviderButton {
    wire::ProviderButton {
        id: provider.id.to_string(),
        kind: kind(provider.kind),
        display_name: provider.display_name.clone(),
    }
}

pub(crate) fn org(org: &control::Org) -> wire::Org {
    wire::Org {
        id: org.id.to_string(),
        name: org.name.clone(),
        settings: wire::OrgSettings {
            signup_mode: org.settings.signup_mode.as_str().to_owned(),
            allowed_email_domains: org.settings.allowed_email_domains.clone(),
            password_login_enabled: org.settings.password_login_enabled,
            project_access: access(org.settings.project_access),
        },
        version: version(org.version),
    }
}

pub(crate) fn project(project: &control::Project, role: control::ProjectRole) -> wire::Project {
    wire::Project {
        id: project.id.to_string(),
        name: project.name.clone(),
        slug: project.slug.clone(),
        role: project_role(role),
        version: version(project.version),
    }
}

pub(crate) fn invitation(invitation: &control::Invitation) -> wire::Invitation {
    wire::Invitation {
        id: invitation.id.to_string(),
        email: invitation.email.to_string(),
        org_role: org_role(invitation.org_role),
        project_roles: invitation
            .project_roles
            .iter()
            .map(|(project_id, role)| wire::ProjectGrant {
                project_id: project_id.to_string(),
                role: project_role(*role),
            })
            .collect(),
        expires_at: time(invitation.expires_at),
        accepted_at: opt_time(invitation.accepted_at),
        revoked_at: opt_time(invitation.revoked_at),
    }
}

pub(crate) fn provider(
    identity: &Identity,
    provider: &control::AuthProvider,
) -> wire::AuthProvider {
    wire::AuthProvider {
        id: provider.id.to_string(),
        kind: kind(provider.kind),
        display_name: provider.display_name.clone(),
        issuer: provider.issuer.clone(),
        client_id: provider.client_id.clone(),
        scopes: provider.scopes.clone(),
        allowed_domains: provider.allowed_domains.clone(),
        enabled: provider.enabled,
        seeded: provider.seeded,
        redirect_uri: identity.redirect_uri(provider.id).unwrap_or_default(),
    }
}

pub(crate) fn api_key(key: &control::ApiKey) -> wire::ApiKey {
    wire::ApiKey {
        id: key.id.to_string(),
        public_key: key.public_key.clone(),
        scopes: key.scopes.iter().copied().map(scope).collect(),
        note: key.note.clone(),
        expires_at: opt_time(key.expires_at),
        last_used_at: opt_time(key.last_used_at),
        revoked_at: opt_time(key.revoked_at),
        version: version(key.version),
    }
}

/// The session DTO (contract 12.7), with the features of this server's modules in `state`.
pub(crate) async fn session_info(
    identity: &Identity,
    principal: &SessionPrincipal,
    state: &ApiState,
) -> Result<wire::SessionInfo, AuthError> {
    // The reasons are those of `GET sql/schema`: `not_configured`, or what the self-probe said.
    let sql_workspace = match state.sql.as_ref().map(SqlWorkspace::status) {
        Some(status) => wire::FeatureState {
            enabled: status.enabled,
            reason: status.reason.map(str::to_owned),
        },
        None => wire::FeatureState {
            enabled: false,
            reason: Some("not_configured".to_owned()),
        },
    };
    // During a Postgres outage the session still answers, from the cache, with the providers this node last read.
    let providers = identity
        .sign_in_view()
        .await
        .map(|view| view.providers)
        .unwrap_or_default();
    let session = &principal.session;
    Ok(wire::SessionInfo {
        user: wire::SessionUser {
            id: principal.user.id.to_string(),
            email: principal.user.email.to_string(),
            name: principal.user.name.clone(),
        },
        org: wire::SessionOrg {
            id: principal.org.id.to_string(),
            name: principal.org.name.clone(),
            role: org_role(principal.org_role),
        },
        projects: principal
            .projects
            .values()
            .map(|(project, role)| wire::SessionProject {
                id: project.id.to_string(),
                name: project.name.clone(),
                slug: project.slug.clone(),
                role: project_role(*role),
                permissions: permission_names(*role),
            })
            .collect(),
        providers: providers.iter().map(button).collect(),
        password_login_enabled: principal.org.settings.password_login_enabled,
        csrf_token: session.csrf_token.clone(),
        session: wire::SessionTimes {
            expires_at: time(session.absolute_expires_at),
            idle_expires_at: time(session.idle_expires_at),
            auth_method: method(session.auth_method),
        },
        features: wire::Features {
            sql_workspace,
            media: wire::MediaFeature {
                enabled: false,
                max_bytes: 0,
            },
            // Email channels deliver through the mail server of `smtp.*`, which alerting holds.
            smtp: state
                .alerts
                .as_ref()
                .is_some_and(|alerts| alerts.channels.sends_email()),
            signup_mode: principal.org.settings.signup_mode.as_str().to_owned(),
        },
        server: wire::ServerInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    })
}
