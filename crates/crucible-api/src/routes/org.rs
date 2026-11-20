//! The organization, its members, invitations and sign-in providers (contracts 11.5, 11.6 and 12.6).

use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::Problem;
use crucible_auth::service::{OrgChange, ProviderChange, ProviderInput};
use crucible_records::{ProjectId, UserId, Uuid};
use utoipa::OpenApi;

use crate::access::{ApiState, OrgMembers, OrgOwner, OrgRead, OrgScoped, OrgSettings, PathParams};
use crate::control::{
    self as c, ApiJson, auth, etag, if_match, invitation, org_role, org_role_in, project_role_in,
};
use crate::problem::{ApiError, field_error};

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/org", get(get_org).patch(patch_org))
        .route("/org/members", get(list_members))
        .route(
            "/org/members/{user_id}",
            patch(patch_member).delete(delete_member),
        )
        .route("/org/transfer-ownership", post(transfer_ownership))
        .route(
            "/org/invitations",
            get(list_invitations).post(create_invitation),
        )
        .route(
            "/org/invitations/{invitation_id}",
            delete(revoke_invitation),
        )
        .route(
            "/org/auth-providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/org/auth-providers/{provider_id}",
            patch(patch_provider).delete(delete_provider),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    get_org,
    patch_org,
    list_members,
    patch_member,
    delete_member,
    transfer_ownership,
    list_invitations,
    create_invitation,
    revoke_invitation,
    list_providers,
    create_provider,
    patch_provider,
    delete_provider
))]
pub(super) struct Paths;

/// Get the organization
#[utoipa::path(
    get,
    path = "/api/v1/org",
    operation_id = "get_org",
    tag = "org",
    responses(
        (status = 200, description = "The organization; `ETag` is its version.", body = wire::Org),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn get_org(org: OrgScoped<OrgRead>) -> (HeaderMap, Json<wire::Org>) {
    let current = &org.principal.org;
    (etag(current.version), Json(c::org(current)))
}

/// Update the organization
///
/// Changes the name and settings; send `If-Match` with the version you read.
#[utoipa::path(
    patch,
    path = "/api/v1/org",
    operation_id = "patch_org",
    tag = "org",
    request_body = wire::OrgPatch,
    responses(
        (status = 200, description = "The organization.", body = wire::Org),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_org(
    org: OrgScoped<OrgSettings>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::OrgPatch>,
) -> Result<(HeaderMap, Json<wire::Org>), ApiError> {
    let rid = org.request_id;
    let expected = if_match(&headers, rid)?;
    let change = OrgChange {
        name: body.name,
        allowed_email_domains: body.allowed_email_domains,
        password_login_enabled: body.password_login_enabled,
        project_access: body.project_access.map(c::access_in),
    };
    let updated = org
        .identity
        .update_org(expected, change)
        .await
        .map_err(auth(rid))?;
    Ok((etag(updated.version), Json(c::org(&updated))))
}

fn member(user: &crucible_control::User, role: crucible_control::OrgRole) -> wire::OrgMember {
    wire::OrgMember {
        user_id: user.id.to_string(),
        email: user.email.to_string(),
        name: user.name.clone(),
        role: org_role(role),
        status: c::status(user.status),
    }
}

/// List members
#[utoipa::path(
    get,
    path = "/api/v1/org/members",
    operation_id = "list_org_members",
    tag = "org",
    responses((status = 200, description = "The members.", body = wire::OrgMemberList)),
    security(("session" = [])),
)]
pub(crate) async fn list_members(
    org: OrgScoped<OrgRead>,
) -> Result<Json<wire::OrgMemberList>, ApiError> {
    let members = org
        .identity
        .org_members()
        .await
        .map_err(auth(org.request_id))?;
    Ok(Json(wire::OrgMemberList {
        data: members
            .iter()
            .map(|(user, role)| member(user, *role))
            .collect(),
    }))
}

/// Change a member's role
///
/// An admin cannot change the owner or make one; the owner changes by transfer. Revokes the member's sessions.
#[utoipa::path(
    patch,
    path = "/api/v1/org/members/{user_id}",
    operation_id = "patch_org_member",
    tag = "org",
    params(("user_id" = String, Path, description = "The user id.")),
    request_body = wire::OrgMemberPatch,
    responses(
        (status = 200, description = "The member.", body = wire::OrgMember),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_member(
    org: OrgScoped<OrgMembers>,
    path: PathParams,
    ApiJson(body): ApiJson<wire::OrgMemberPatch>,
) -> Result<Json<wire::OrgMember>, ApiError> {
    let rid = org.request_id;
    let user_id = path.id("user_id", UserId::from_uuid)?;
    let (user, role) = org
        .identity
        .set_org_role(&org.principal, user_id, org_role_in(body.role))
        .await
        .map_err(auth(rid))?;
    Ok(Json(member(&user, role)))
}

/// Remove a member
///
/// Deletes the member's account with its sessions and memberships.
#[utoipa::path(
    delete,
    path = "/api/v1/org/members/{user_id}",
    operation_id = "delete_org_member",
    tag = "org",
    params(("user_id" = String, Path, description = "The user id.")),
    responses(
        (status = 204, description = "Removed."),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_member(
    org: OrgScoped<OrgMembers>,
    path: PathParams,
) -> Result<StatusCode, ApiError> {
    let rid = org.request_id;
    let user_id = path.id("user_id", UserId::from_uuid)?;
    org.identity
        .remove_member(&org.principal, user_id)
        .await
        .map_err(auth(rid))?;
    Ok(StatusCode::NO_CONTENT)
}

/// Transfer ownership
///
/// Makes another member the owner; the caller becomes an admin.
#[utoipa::path(
    post,
    path = "/api/v1/org/transfer-ownership",
    operation_id = "transfer_ownership",
    tag = "org",
    request_body = wire::TransferOwnership,
    responses(
        (status = 204, description = "Transferred."),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn transfer_ownership(
    org: OrgScoped<OrgOwner>,
    ApiJson(body): ApiJson<wire::TransferOwnership>,
) -> Result<StatusCode, ApiError> {
    let rid = org.request_id;
    let user_id = body
        .user_id
        .parse::<Uuid>()
        .map(UserId::from_uuid)
        .map_err(|_| {
            ApiError::validation(rid, vec![field_error("user_id", "invalid", "a user id")])
        })?;
    org.identity
        .transfer_ownership(&org.principal, user_id)
        .await
        .map_err(auth(rid))?;
    Ok(StatusCode::NO_CONTENT)
}

/// List invitations
#[utoipa::path(
    get,
    path = "/api/v1/org/invitations",
    operation_id = "list_invitations",
    tag = "org",
    responses((status = 200, description = "The invitations.", body = wire::InvitationList)),
    security(("session" = [])),
)]
pub(crate) async fn list_invitations(
    org: OrgScoped<OrgRead>,
) -> Result<Json<wire::InvitationList>, ApiError> {
    let list = org
        .identity
        .invitations()
        .await
        .map_err(auth(org.request_id))?;
    Ok(Json(wire::InvitationList {
        data: list.iter().map(invitation).collect(),
    }))
}

/// Invite someone
///
/// Creates an invitation and answers its link once. This version sends no email; pass the link on.
#[utoipa::path(
    post,
    path = "/api/v1/org/invitations",
    operation_id = "create_invitation",
    tag = "org",
    request_body = wire::InvitationCreate,
    responses(
        (status = 201, description = "The invitation and its link.", body = wire::InvitationCreated),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: already a member.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_invitation(
    org: OrgScoped<OrgMembers>,
    ApiJson(body): ApiJson<wire::InvitationCreate>,
) -> Result<(StatusCode, Json<wire::InvitationCreated>), ApiError> {
    let rid = org.request_id;
    let mut grants = Vec::new();
    for grant in body.project_roles.unwrap_or_default() {
        let project = grant
            .project_id
            .parse::<Uuid>()
            .map(ProjectId::from_uuid)
            .map_err(|_| {
                ApiError::validation(
                    rid,
                    vec![field_error("project_roles", "invalid", "a project id")],
                )
            })?;
        grants.push((project, project_role_in(grant.role)));
    }
    let created = org
        .identity
        .invite(
            &org.principal,
            &body.email,
            org_role_in(body.org_role),
            grants,
        )
        .await
        .map_err(auth(rid))?;
    Ok((
        StatusCode::CREATED,
        Json(wire::InvitationCreated {
            invitation: invitation(&created.invitation),
            accept_path: format!("/invite/?token={}", created.token.as_str()),
        }),
    ))
}

/// Revoke an invitation
#[utoipa::path(
    delete,
    path = "/api/v1/org/invitations/{invitation_id}",
    operation_id = "revoke_invitation",
    tag = "org",
    params(("invitation_id" = String, Path, description = "The invitation id.")),
    responses(
        (status = 204, description = "Revoked."),
        (status = 404, description = "`not_found`: no open invitation.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn revoke_invitation(
    org: OrgScoped<OrgMembers>,
    path: PathParams,
) -> Result<StatusCode, ApiError> {
    let rid = org.request_id;
    let id = path.id("invitation_id", |u| u)?;
    org.identity
        .revoke_invitation(id)
        .await
        .map_err(auth(rid))?;
    Ok(StatusCode::NO_CONTENT)
}

/// List sign-in providers
#[utoipa::path(
    get,
    path = "/api/v1/org/auth-providers",
    operation_id = "list_auth_providers",
    tag = "org",
    responses((status = 200, description = "The providers.", body = wire::AuthProviderList)),
    security(("session" = [])),
)]
pub(crate) async fn list_providers(
    org: OrgScoped<OrgSettings>,
) -> Result<Json<wire::AuthProviderList>, ApiError> {
    let list = org
        .identity
        .providers()
        .await
        .map_err(auth(org.request_id))?;
    Ok(Json(wire::AuthProviderList {
        data: list.iter().map(|p| c::provider(&org.identity, p)).collect(),
    }))
}

/// Add a sign-in provider
///
/// The client secret is stored sealed under the master key and never returned. Provider sign-in needs
/// `server.public_url` for the callback URL.
#[utoipa::path(
    post,
    path = "/api/v1/org/auth-providers",
    operation_id = "create_auth_provider",
    tag = "org",
    request_body = wire::AuthProviderCreate,
    responses(
        (status = 201, description = "The provider.", body = wire::AuthProvider),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_provider(
    org: OrgScoped<OrgSettings>,
    ApiJson(body): ApiJson<wire::AuthProviderCreate>,
) -> Result<(StatusCode, Json<wire::AuthProvider>), ApiError> {
    let input = ProviderInput {
        kind: c::kind_in(body.kind),
        display_name: body.display_name,
        issuer: body.issuer,
        client_id: body.client_id,
        client_secret: body.client_secret,
        scopes: body.scopes.unwrap_or_default(),
        allowed_domains: body.allowed_domains.unwrap_or_default(),
    };
    let provider = org
        .identity
        .add_provider(input)
        .await
        .map_err(auth(org.request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(c::provider(&org.identity, &provider)),
    ))
}

/// Change a sign-in provider
#[utoipa::path(
    patch,
    path = "/api/v1/org/auth-providers/{provider_id}",
    operation_id = "patch_auth_provider",
    tag = "org",
    params(("provider_id" = String, Path, description = "The provider id.")),
    request_body = wire::AuthProviderPatch,
    responses(
        (status = 200, description = "The provider.", body = wire::AuthProvider),
        (status = 403, description = "`forbidden`: from the configuration.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_provider(
    org: OrgScoped<OrgSettings>,
    path: PathParams,
    ApiJson(body): ApiJson<wire::AuthProviderPatch>,
) -> Result<Json<wire::AuthProvider>, ApiError> {
    let rid = org.request_id;
    let id = path.id("provider_id", |u| u)?;
    let change = ProviderChange {
        display_name: body.display_name,
        client_secret: body.client_secret,
        allowed_domains: body.allowed_domains,
        enabled: body.enabled,
    };
    let provider = org
        .identity
        .update_provider(id, change)
        .await
        .map_err(auth(rid))?;
    Ok(Json(c::provider(&org.identity, &provider)))
}

/// Delete a sign-in provider
#[utoipa::path(
    delete,
    path = "/api/v1/org/auth-providers/{provider_id}",
    operation_id = "delete_auth_provider",
    tag = "org",
    params(("provider_id" = String, Path, description = "The provider id.")),
    responses(
        (status = 204, description = "Deleted with its linked accounts."),
        (status = 403, description = "`forbidden`: from the configuration.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_provider(
    org: OrgScoped<OrgSettings>,
    path: PathParams,
) -> Result<StatusCode, ApiError> {
    let rid = org.request_id;
    let id = path.id("provider_id", |u| u)?;
    org.identity.delete_provider(id).await.map_err(auth(rid))?;
    Ok(StatusCode::NO_CONTENT)
}
