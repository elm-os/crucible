//! Projects, project members and API keys (contracts 11.3, 11.6 and 12.6). Keys never manage keys, members or
//! settings, so these routes answer an API key with 403.

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, patch};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{Problem, ProblemCode};
use crucible_auth::service::{ApiKeyChange, Identity, SessionPrincipal};
use crucible_records::{ApiKeyId, ProjectId, RequestId, Timestamp, UserId, Uuid};
use std::sync::Arc;
use utoipa::OpenApi;

use crate::access::{ApiState, Manage, OrgProjects, OrgScoped, Principal, Scoped, Signed};
use crate::control::{self as c, ApiJson, auth, etag, if_match, project_role, project_role_in};
use crate::problem::{ApiError, field_error};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{project_id}",
            get(get_project).patch(patch_project).delete(delete_project),
        )
        .route(
            "/projects/{project_id}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/projects/{project_id}/members/{user_id}",
            patch(patch_member).delete(delete_member),
        )
        .route(
            "/projects/{project_id}/api-keys",
            get(list_keys).post(create_key),
        )
        .route(
            "/projects/{project_id}/api-keys/{key_id}",
            patch(patch_key).delete(revoke_key),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    list_projects,
    create_project,
    get_project,
    patch_project,
    delete_project,
    list_members,
    add_member,
    patch_member,
    delete_member,
    list_keys,
    create_key,
    patch_key,
    revoke_key
))]
pub(super) struct Paths;

/// The session behind a scoped request, and the identity; keys are refused.
fn session<P>(
    scoped: &Scoped<P>,
    state: &ApiState,
) -> Result<(Arc<Identity>, Arc<SessionPrincipal>), ApiError> {
    let rid = scoped.request_id;
    let identity = Arc::clone(state.identity(rid)?);
    match &scoped.principal {
        Principal::Session(principal) => Ok((identity, Arc::clone(principal))),
        Principal::Key(_) => Err(ApiError::new(
            rid,
            ProblemCode::Forbidden,
            "API keys cannot manage projects, members or keys; sign in to the UI",
        )),
    }
}

fn path_id<T>(
    rid: RequestId,
    path: &std::collections::BTreeMap<String, String>,
    name: &str,
    wrap: impl Fn(Uuid) -> T,
) -> Result<T, ApiError> {
    path.get(name)
        .and_then(|text| text.parse::<Uuid>().ok())
        .map(wrap)
        .ok_or_else(|| ApiError::not_found(rid))
}

fn time(rid: RequestId, field: &str, text: Option<String>) -> Result<Option<Timestamp>, ApiError> {
    text.map(|text| {
        Timestamp::parse_rfc3339(&text).map_err(|_| {
            ApiError::validation(rid, vec![field_error(field, "invalid", "an RFC 3339 time")])
        })
    })
    .transpose()
}

/// List projects
///
/// The projects the signed-in user can see, with their role on each.
#[utoipa::path(
    get,
    path = "/api/v1/projects",
    operation_id = "list_projects",
    tag = "projects",
    responses((status = 200, description = "The projects.", body = wire::ProjectList)),
    security(("session" = [])),
)]
pub(crate) async fn list_projects(signed: Signed) -> Json<wire::ProjectList> {
    Json(wire::ProjectList {
        data: signed
            .principal
            .projects
            .values()
            .map(|(project, role)| c::project(project, *role))
            .collect(),
    })
}

/// Create a project
#[utoipa::path(
    post,
    path = "/api/v1/projects",
    operation_id = "create_project",
    tag = "projects",
    request_body = wire::ProjectCreate,
    responses(
        (status = 201, description = "The project.", body = wire::Project),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the slug is taken.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_project(
    org: OrgScoped<OrgProjects>,
    ApiJson(body): ApiJson<wire::ProjectCreate>,
) -> Result<(StatusCode, HeaderMap, Json<wire::Project>), ApiError> {
    let project = org
        .identity
        .create_project(&body.name, &body.slug)
        .await
        .map_err(auth(org.request_id))?;
    Ok((
        StatusCode::CREATED,
        etag(project.version),
        Json(c::project(&project, crucible_control::ProjectRole::Admin)),
    ))
}

/// Get a project
///
/// The project and the signed-in user's role on it.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}",
    operation_id = "get_project",
    tag = "projects",
    params(("project_id" = String, Path, description = "The project id.")),
    responses(
        (status = 200, description = "The project; `ETag` is its version.", body = wire::Project),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn get_project(
    signed: Signed,
    Path(project_id): Path<String>,
) -> Result<(HeaderMap, Json<wire::Project>), ApiError> {
    let rid = signed.request_id;
    let (project, role) = project_id
        .parse::<Uuid>()
        .ok()
        .and_then(|id| signed.principal.projects.get(&ProjectId::from_uuid(id)))
        .ok_or_else(|| ApiError::not_found(rid))?;
    Ok((etag(project.version), Json(c::project(project, *role))))
}

/// Rename a project
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}",
    operation_id = "patch_project",
    tag = "projects",
    params(("project_id" = String, Path, description = "The project id.")),
    request_body = wire::ProjectPatch,
    responses(
        (status = 200, description = "The project.", body = wire::Project),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_project(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::ProjectPatch>,
) -> Result<(HeaderMap, Json<wire::Project>), ApiError> {
    let rid = scoped.request_id;
    let (identity, principal) = session(&scoped, &state)?;
    let expected = if_match(&headers, rid)?;
    let id = scoped.scope.project_id();
    let project = identity
        .rename_project(id, expected, &body.name)
        .await
        .map_err(auth(rid))?;
    let role = principal
        .project_role(id)
        .unwrap_or(crucible_control::ProjectRole::Admin);
    Ok((etag(project.version), Json(c::project(&project, role))))
}

/// Delete a project
///
/// A soft delete: the project disappears from every list and its keys stop working.
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}",
    operation_id = "delete_project",
    tag = "projects",
    params(("project_id" = String, Path, description = "The project id.")),
    responses(
        (status = 204, description = "Deleted."),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_project(
    org: OrgScoped<OrgProjects>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let rid = org.request_id;
    let id = project_id
        .parse::<Uuid>()
        .map(ProjectId::from_uuid)
        .map_err(|_| ApiError::not_found(rid))?;
    if org.principal.project_role(id).is_none() {
        return Err(ApiError::not_found(rid));
    }
    let expected = if_match(&headers, rid)?;
    org.identity
        .delete_project(id, expected)
        .await
        .map_err(auth(rid))?;
    Ok(StatusCode::NO_CONTENT)
}

fn project_member(
    user: &crucible_control::User,
    role: crucible_control::ProjectRole,
) -> wire::ProjectMember {
    wire::ProjectMember {
        user_id: user.id.to_string(),
        email: user.email.to_string(),
        name: user.name.clone(),
        role: project_role(role),
    }
}

/// List project members
///
/// The explicit memberships of the project.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/members",
    operation_id = "list_project_members",
    tag = "projects",
    params(("project_id" = String, Path, description = "The project id.")),
    responses(
        (status = 200, description = "The members.", body = wire::ProjectMemberList),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn list_members(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
) -> Result<Json<wire::ProjectMemberList>, ApiError> {
    let (identity, _) = session(&scoped, &state)?;
    let members = identity
        .project_members(scoped.scope.project_id())
        .await
        .map_err(auth(scoped.request_id))?;
    Ok(Json(wire::ProjectMemberList {
        data: members.iter().map(|(u, r)| project_member(u, *r)).collect(),
    }))
}

/// Add a project member
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/members",
    operation_id = "add_project_member",
    tag = "projects",
    params(("project_id" = String, Path, description = "The project id.")),
    request_body = wire::ProjectMemberAdd,
    responses(
        (status = 201, description = "The member.", body = wire::ProjectMember),
        (status = 422, description = "`validation_failed`: not an org member.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn add_member(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
    ApiJson(body): ApiJson<wire::ProjectMemberAdd>,
) -> Result<(StatusCode, Json<wire::ProjectMember>), ApiError> {
    let rid = scoped.request_id;
    let (identity, _) = session(&scoped, &state)?;
    let user_id = body
        .user_id
        .parse::<Uuid>()
        .map(UserId::from_uuid)
        .map_err(|_| {
            ApiError::validation(rid, vec![field_error("user_id", "invalid", "a user id")])
        })?;
    let role = project_role_in(body.role);
    let user = identity
        .set_project_role(scoped.scope.project_id(), user_id, role)
        .await
        .map_err(auth(rid))?;
    Ok((StatusCode::CREATED, Json(project_member(&user, role))))
}

/// Change a project member's role
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/members/{user_id}",
    operation_id = "patch_project_member",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("user_id" = String, Path, description = "The user id."),
    ),
    request_body = wire::ProjectMemberPatch,
    responses((status = 200, description = "The member.", body = wire::ProjectMember)),
    security(("session" = [])),
)]
pub(crate) async fn patch_member(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
    ApiJson(body): ApiJson<wire::ProjectMemberPatch>,
) -> Result<Json<wire::ProjectMember>, ApiError> {
    let rid = scoped.request_id;
    let (identity, _) = session(&scoped, &state)?;
    let user_id = path_id(rid, &scoped.path, "user_id", UserId::from_uuid)?;
    let role = project_role_in(body.role);
    let user = identity
        .set_project_role(scoped.scope.project_id(), user_id, role)
        .await
        .map_err(auth(rid))?;
    Ok(Json(project_member(&user, role)))
}

/// Remove a project member
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/members/{user_id}",
    operation_id = "delete_project_member",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("user_id" = String, Path, description = "The user id."),
    ),
    responses(
        (status = 204, description = "Removed."),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_member(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
) -> Result<StatusCode, ApiError> {
    let rid = scoped.request_id;
    let (identity, _) = session(&scoped, &state)?;
    let user_id = path_id(rid, &scoped.path, "user_id", UserId::from_uuid)?;
    identity
        .remove_project_member(scoped.scope.project_id(), user_id)
        .await
        .map_err(auth(rid))?;
    Ok(StatusCode::NO_CONTENT)
}

/// List API keys
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/api-keys",
    operation_id = "list_api_keys",
    tag = "projects",
    params(("project_id" = String, Path, description = "The project id.")),
    responses(
        (status = 200, description = "The keys, without their secrets.", body = wire::ApiKeyList),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn list_keys(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
) -> Result<Json<wire::ApiKeyList>, ApiError> {
    let (identity, _) = session(&scoped, &state)?;
    let keys = identity
        .api_keys(scoped.scope.project_id())
        .await
        .map_err(auth(scoped.request_id))?;
    Ok(Json(wire::ApiKeyList {
        data: keys.iter().map(c::api_key).collect(),
    }))
}

/// Create an API key
///
/// Answers the secret key once; it cannot be shown again. New keys have `ingest`, `read` and `write` unless
/// `scopes` says otherwise.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/api-keys",
    operation_id = "create_api_key",
    tag = "projects",
    params(("project_id" = String, Path, description = "The project id.")),
    request_body = wire::ApiKeyCreate,
    responses(
        (status = 201, description = "The key and its secret.", body = wire::ApiKeyCreated),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_key(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
    ApiJson(body): ApiJson<wire::ApiKeyCreate>,
) -> Result<(StatusCode, HeaderMap, Json<wire::ApiKeyCreated>), ApiError> {
    let rid = scoped.request_id;
    let (identity, _) = session(&scoped, &state)?;
    let expires_at = time(rid, "expires_at", body.expires_at)?;
    let scopes = body
        .scopes
        .map(|list| list.into_iter().map(c::scope_in).collect());
    let created = identity
        .create_key(scoped.scope.project_id(), scopes, body.note, expires_at)
        .await
        .map_err(auth(rid))?;
    Ok((
        StatusCode::CREATED,
        etag(created.key.version),
        Json(wire::ApiKeyCreated {
            key: c::api_key(&created.key),
            secret_key: created.secret,
        }),
    ))
}

/// Change an API key
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/api-keys/{key_id}",
    operation_id = "patch_api_key",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("key_id" = String, Path, description = "The key id."),
    ),
    request_body = wire::ApiKeyPatch,
    responses(
        (status = 200, description = "The key.", body = wire::ApiKey),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_key(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::ApiKeyPatch>,
) -> Result<(HeaderMap, Json<wire::ApiKey>), ApiError> {
    let rid = scoped.request_id;
    let (identity, _) = session(&scoped, &state)?;
    let key_id = path_id(rid, &scoped.path, "key_id", ApiKeyId::from_uuid)?;
    let expected = if_match(&headers, rid)?;
    let change = ApiKeyChange {
        scopes: body
            .scopes
            .map(|list| list.into_iter().map(c::scope_in).collect()),
        note: body.note,
        expires_at: time(rid, "expires_at", body.expires_at)?,
    };
    let key = identity
        .update_key(scoped.scope.project_id(), key_id, expected, change)
        .await
        .map_err(auth(rid))?;
    Ok((etag(key.version), Json(c::api_key(&key))))
}

/// Revoke an API key
///
/// The key stops working on this server at once and on every node within 5 s.
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/api-keys/{key_id}",
    operation_id = "revoke_api_key",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("key_id" = String, Path, description = "The key id."),
    ),
    responses(
        (status = 204, description = "Revoked."),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn revoke_key(
    axum::extract::State(state): axum::extract::State<ApiState>,
    scoped: Scoped<Manage>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let rid = scoped.request_id;
    let (identity, _) = session(&scoped, &state)?;
    let key_id = path_id(rid, &scoped.path, "key_id", ApiKeyId::from_uuid)?;
    let expected = if_match(&headers, rid)?;
    identity
        .revoke_key(scoped.scope.project_id(), key_id, expected)
        .await
        .map_err(auth(rid))?;
    Ok(StatusCode::NO_CONTENT)
}
