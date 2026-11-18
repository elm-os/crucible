//! Sign-in, sessions and the user's own account (contracts 11.4, 11.5, 12.6 and 12.7).

use axum::extract::{Path, Query};
use axum::http::header::{COOKIE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, SessionInfo, SignInOptions};
use crucible_auth::cookie;
use crucible_auth::service::{
    CallbackQuery, FLOW_TTL, FlowFailure, Identity, NewSession, SESSION_ABSOLUTE,
};
use crucible_records::Uuid;
use serde::Deserialize;
use utoipa::{IntoParams, OpenApi};

use crate::access::{ApiState, Prelogin, Public, Signed};
use crate::control::{ApiJson, auth, session_info, set_cookies};
use crate::problem::ApiError;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/auth/providers", get(get_providers))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/session", get(get_session))
        .route("/auth/oauth/{provider_id}/start", get(oauth_start))
        .route("/auth/oauth/{provider_id}/callback", get(oauth_callback))
        .route("/auth/password/reset-request", post(reset_request))
        .route("/auth/password/reset-confirm", post(reset_confirm))
        .route("/auth/password/change", post(change_password))
        .route("/auth/invitations/accept", post(accept_invitation))
        .route("/auth/setup", post(setup))
        .route("/account", get(get_account).patch(patch_account))
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    get_providers,
    login,
    logout,
    get_session,
    oauth_start,
    oauth_callback,
    reset_request,
    reset_confirm,
    change_password,
    accept_invitation,
    setup,
    get_account,
    patch_account
))]
pub(super) struct Paths;

/// A response that signs the browser in: the session DTO and the session cookie.
async fn signed_in(
    identity: &Identity,
    rid: crucible_records::RequestId,
    new: NewSession,
) -> Result<Response, ApiError> {
    let info = session_info(identity, &new.principal)
        .await
        .map_err(auth(rid))?;
    let cookies = identity.config().cookies();
    let mut headers = HeaderMap::new();
    set_cookies(
        &mut headers,
        &[cookies.set(cookies.session(), new.token.as_str(), SESSION_ABSOLUTE)],
    );
    Ok((headers, Json(info)).into_response())
}

/// List sign-in options
///
/// The enabled sign-in providers, whether password sign-in is on and whether first-run setup is needed. Without
/// a control plane this answers `feature_disabled`, and clients use a project key instead.
#[utoipa::path(
    get,
    path = "/api/v1/auth/providers",
    operation_id = "get_auth_providers",
    tag = "auth",
    responses(
        (status = 200, description = "The sign-in options.", body = SignInOptions),
        (status = 404, description = "`feature_disabled`: no control plane.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(()),
)]
pub(crate) async fn get_providers(public: Public) -> Result<Json<SignInOptions>, ApiError> {
    let rid = public.request_id;
    let identity = &public.identity;
    let org = identity.org().await.map_err(auth(rid))?;
    let providers = identity.sign_in_providers().await.map_err(auth(rid))?;
    Ok(Json(SignInOptions {
        providers: providers.iter().map(crate::control::button).collect(),
        password_login_enabled: org
            .as_ref()
            .is_none_or(|org| org.settings.password_login_enabled),
        setup_required: org.is_none(),
    }))
}

/// Sign in
///
/// Signs in with email and password and sets the session cookie. Send `X-Crucible-CSRF: prelogin`.
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    operation_id = "login",
    tag = "auth",
    request_body = wire::LoginRequest,
    responses(
        (status = 200, description = "Signed in; the session cookie is set.", body = SessionInfo),
        (status = 401, description = "`invalid_credentials`.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "`forbidden`, `csrf_invalid` or `origin_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "`rate_limited`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(()),
)]
pub(crate) async fn login(
    pre: Prelogin,
    ApiJson(body): ApiJson<wire::LoginRequest>,
) -> Result<Response, ApiError> {
    let rid = pre.request_id;
    let new = pre
        .identity
        .login(&body.email, &body.password)
        .await
        .map_err(auth(rid))?;
    signed_in(&pre.identity, rid, new).await
}

/// Sign out
///
/// Revokes the session and clears its cookie.
#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    operation_id = "logout",
    tag = "auth",
    responses(
        (status = 204, description = "Signed out."),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn logout(signed: Signed) -> Result<Response, ApiError> {
    signed
        .identity
        .logout(&signed.principal)
        .await
        .map_err(auth(signed.request_id))?;
    let cookies = signed.identity.config().cookies();
    let mut headers = HeaderMap::new();
    set_cookies(&mut headers, &[cookies.clear(cookies.session())]);
    Ok((StatusCode::NO_CONTENT, headers).into_response())
}

/// Get the session
///
/// The signed-in user, their organization role, the projects they can see with their permissions, and the
/// CSRF token to send as `X-Crucible-CSRF`.
#[utoipa::path(
    get,
    path = "/api/v1/auth/session",
    operation_id = "get_session",
    tag = "auth",
    responses(
        (status = 200, description = "The session.", body = SessionInfo),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn get_session(signed: Signed) -> Result<Json<SessionInfo>, ApiError> {
    let info = session_info(&signed.identity, &signed.principal)
        .await
        .map_err(auth(signed.request_id))?;
    Ok(Json(info))
}

/// The query of a sign-in start.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct StartQuery {
    /// Where to go after sign-in: a path on this server.
    next: Option<String>,
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(location) {
        headers.insert(LOCATION, value);
    }
    set_cookies(&mut headers, cookies);
    (StatusCode::SEE_OTHER, headers).into_response()
}

fn login_error(identity: &Identity, failure: FlowFailure) -> Response {
    let cookies = identity.config().cookies();
    redirect(
        &format!("/login/?error={}", failure.code()),
        &[cookies.clear(cookies.flow())],
    )
}

fn provider_id(text: &str) -> Result<Uuid, FlowFailure> {
    text.parse().map_err(|_| FlowFailure::ProviderDisabled)
}

/// Start a provider sign-in
///
/// Redirects to the provider (303) and sets the flow cookie. On failure it redirects to `/login/?error=<code>`.
#[utoipa::path(
    get,
    path = "/api/v1/auth/oauth/{provider_id}/start",
    operation_id = "oauth_start",
    tag = "auth",
    params(("provider_id" = String, Path, description = "The provider id."), StartQuery),
    responses((status = 303, description = "To the provider, or to `/login/?error=<code>`.")),
    security(()),
)]
pub(crate) async fn oauth_start(
    public: Public,
    Path(id): Path<String>,
    Query(query): Query<StartQuery>,
) -> Response {
    let identity = &public.identity;
    let started = match provider_id(&id) {
        Ok(id) => identity.start_flow(id, query.next.as_deref()).await,
        Err(failure) => Err(failure),
    };
    match started {
        Ok(start) => {
            let cookies = identity.config().cookies();
            redirect(
                &start.location,
                &[cookies.set(cookies.flow(), start.flow_cookie.as_str(), FLOW_TTL)],
            )
        }
        Err(failure) => login_error(identity, failure),
    }
}

/// The query of a provider callback.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct CallbackParams {
    /// The authorization code.
    code: Option<String>,
    /// The state.
    state: Option<String>,
    /// The provider's error.
    error: Option<String>,
}

/// Finish a provider sign-in
///
/// The provider's redirect target. Checks the state against the flow cookie, exchanges the code, signs the
/// person in and redirects (303) to where the sign-in began, or to `/login/?error=<code>`.
#[utoipa::path(
    get,
    path = "/api/v1/auth/oauth/{provider_id}/callback",
    operation_id = "oauth_callback",
    tag = "auth",
    params(("provider_id" = String, Path, description = "The provider id."), CallbackParams),
    responses((status = 303, description = "To the page the sign-in began on, or to `/login/?error=<code>`.")),
    security(()),
)]
pub(crate) async fn oauth_callback(
    public: Public,
    Path(id): Path<String>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    let identity = &public.identity;
    let cookies = identity.config().cookies();
    let flow_cookie = cookie::find(
        headers
            .get_all(COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok()),
        cookies.flow(),
    );
    let query = CallbackQuery {
        code: params.code,
        state: params.state,
        error: params.error,
    };
    let finished = match provider_id(&id) {
        Ok(id) => identity.finish_flow(id, &query, flow_cookie).await,
        Err(failure) => Err(failure),
    };
    match finished {
        Ok(signed_in) => redirect(
            &signed_in.next,
            &[
                cookies.clear(cookies.flow()),
                cookies.set(
                    cookies.session(),
                    signed_in.session.token.as_str(),
                    SESSION_ABSOLUTE,
                ),
            ],
        ),
        Err(failure) => login_error(identity, failure),
    }
}

/// Ask for a password reset
///
/// Always 202, so it does not reveal which emails have accounts. This version sends no email: an operator makes
/// a reset link with `crucible admin reset-password <email>`.
#[utoipa::path(
    post,
    path = "/api/v1/auth/password/reset-request",
    operation_id = "password_reset_request",
    tag = "auth",
    request_body = wire::PasswordResetRequest,
    responses((status = 202, description = "Accepted.")),
    security(()),
)]
pub(crate) async fn reset_request(
    pre: Prelogin,
    ApiJson(_body): ApiJson<wire::PasswordResetRequest>,
) -> StatusCode {
    tracing::info!(
        request_id = %pre.request_id,
        "a password reset was requested; this server sends no email, so run `crucible admin reset-password <email>`"
    );
    StatusCode::ACCEPTED
}

/// Reset the password
///
/// Sets a new password with the token of a reset link and revokes every session of the user.
#[utoipa::path(
    post,
    path = "/api/v1/auth/password/reset-confirm",
    operation_id = "password_reset_confirm",
    tag = "auth",
    request_body = wire::PasswordResetConfirm,
    responses(
        (status = 204, description = "The password is set; sign in with it."),
        (status = 422, description = "`token_invalid` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(()),
)]
pub(crate) async fn reset_confirm(
    pre: Prelogin,
    ApiJson(body): ApiJson<wire::PasswordResetConfirm>,
) -> Result<StatusCode, ApiError> {
    pre.identity
        .reset_password(&body.token, &body.new_password)
        .await
        .map_err(auth(pre.request_id))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Change the password
///
/// Changes the signed-in user's password and revokes their other sessions.
#[utoipa::path(
    post,
    path = "/api/v1/auth/password/change",
    operation_id = "password_change",
    tag = "auth",
    request_body = wire::PasswordChange,
    responses(
        (status = 204, description = "Changed."),
        (status = 401, description = "`invalid_credentials`: the current password is wrong.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn change_password(
    signed: Signed,
    ApiJson(body): ApiJson<wire::PasswordChange>,
) -> Result<StatusCode, ApiError> {
    signed
        .identity
        .change_password(
            &signed.principal,
            &body.current_password,
            &body.new_password,
        )
        .await
        .map_err(auth(signed.request_id))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Accept an invitation
///
/// Creates the invited account with a password, signs it in and sets the session cookie.
#[utoipa::path(
    post,
    path = "/api/v1/auth/invitations/accept",
    operation_id = "accept_invitation",
    tag = "auth",
    request_body = wire::InvitationAccept,
    responses(
        (status = 200, description = "Signed in.", body = SessionInfo),
        (status = 409, description = "`conflict`: the account exists.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`token_invalid` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(()),
)]
pub(crate) async fn accept_invitation(
    pre: Prelogin,
    ApiJson(body): ApiJson<wire::InvitationAccept>,
) -> Result<Response, ApiError> {
    let new = pre
        .identity
        .accept_invitation(&body.token, &body.name, &body.password)
        .await
        .map_err(auth(pre.request_id))?;
    signed_in(&pre.identity, pre.request_id, new).await
}

/// Set up the first owner
///
/// Creates the organization and its owner with the one-time token from the server log, and signs the owner in.
#[utoipa::path(
    post,
    path = "/api/v1/auth/setup",
    operation_id = "setup",
    tag = "auth",
    request_body = wire::SetupRequest,
    responses(
        (status = 200, description = "Signed in as the owner.", body = SessionInfo),
        (status = 409, description = "`conflict`: an owner exists.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`token_invalid` or `validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(()),
)]
pub(crate) async fn setup(
    pre: Prelogin,
    ApiJson(body): ApiJson<wire::SetupRequest>,
) -> Result<Response, ApiError> {
    let new = pre
        .identity
        .setup(
            &body.token,
            &body.org_name,
            &body.email,
            &body.name,
            &body.password,
        )
        .await
        .map_err(auth(pre.request_id))?;
    signed_in(&pre.identity, pre.request_id, new).await
}

/// Get the account
///
/// The signed-in user's own profile.
#[utoipa::path(
    get,
    path = "/api/v1/account",
    operation_id = "get_account",
    tag = "auth",
    responses(
        (status = 200, description = "The account.", body = wire::Account),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn get_account(signed: Signed) -> Json<wire::Account> {
    let user = &signed.principal.user;
    Json(wire::Account {
        id: user.id.to_string(),
        email: user.email.to_string(),
        name: user.name.clone(),
    })
}

/// Update the account
///
/// Renames the signed-in user.
#[utoipa::path(
    patch,
    path = "/api/v1/account",
    operation_id = "patch_account",
    tag = "auth",
    request_body = wire::AccountPatch,
    responses(
        (status = 200, description = "The account.", body = wire::Account),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_account(
    signed: Signed,
    ApiJson(body): ApiJson<wire::AccountPatch>,
) -> Result<Json<wire::Account>, ApiError> {
    let user = signed
        .identity
        .rename(&signed.principal, &body.name)
        .await
        .map_err(auth(signed.request_id))?;
    Ok(Json(wire::Account {
        id: user.id.to_string(),
        email: user.email.to_string(),
        name: user.name,
    }))
}
