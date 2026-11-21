//! Sign-in, sessions, the organization, projects, members, invitations, sign-in providers and API keys (contract
//! 11 and 12.6 to 12.7). Ids are canonical UUIDs and times RFC 3339 strings; request bodies refuse unknown fields.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

/// Declares a wire enum: lowercase snake_case strings.
macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }
    };
}

wire_enum! {
    /// An organization role (contract 11.6).
    OrgRole {
        /// Everything, including transferring ownership.
        Owner,
        /// Org settings, members, projects; project admin everywhere.
        Admin,
        /// Their role on projects follows the project access setting.
        Member,
        /// Read-only.
        Viewer,
    }
}

wire_enum! {
    /// A project role (contract 11.6).
    ProjectRole {
        /// Every project capability.
        Admin,
        /// Reads and writes, no keys, members or settings.
        Member,
        /// Reads.
        Viewer,
    }
}

wire_enum! {
    /// How org members and viewers reach projects.
    ProjectAccess {
        /// Their org role applies on every project.
        All,
        /// Only through a project membership.
        Explicit,
    }
}

wire_enum! {
    /// An API key scope (contract 11.3).
    KeyScope {
        /// OTLP and compat ingest.
        Ingest,
        /// Telemetry and analytics reads, exports, the MCP server.
        Read,
        /// Feedback, tags, flags, corrections, comments.
        Write,
        /// Deletion requests.
        Delete,
        /// SQL workspace runs.
        Sql,
    }
}

wire_enum! {
    /// The kind of a sign-in provider.
    ProviderKind {
        /// GitHub.
        Github,
        /// Google.
        Google,
        /// Any OpenID Connect issuer.
        Oidc,
    }
}

wire_enum! {
    /// How a session signed in.
    AuthMethod {
        /// Email and password.
        Password,
        /// GitHub.
        Github,
        /// Google.
        Google,
        /// An OpenID Connect provider.
        Oidc,
    }
}

wire_enum! {
    /// Whether a user may sign in.
    UserStatus {
        /// May sign in.
        Active,
        /// May not sign in.
        Disabled,
    }
}

/// A sign-in button.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ProviderButton {
    /// The provider id; it names the provider in `auth/oauth/{provider_id}/start`.
    pub id: String,
    /// The kind.
    pub kind: ProviderKind,
    /// The text on the button.
    pub display_name: String,
}

/// What the sign-in page offers (`GET auth/providers`, public).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SignInOptions {
    /// The enabled providers.
    pub providers: Vec<ProviderButton>,
    /// Whether email-and-password login is on.
    pub password_login_enabled: bool,
    /// Whether the server has no owner yet, so the first-run setup page applies.
    pub setup_required: bool,
}

/// `POST auth/login`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct LoginRequest {
    /// The email.
    pub email: String,
    /// The password.
    pub password: String,
}

/// The signed-in user.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionUser {
    /// The user id.
    pub id: String,
    /// The email.
    pub email: String,
    /// The display name.
    pub name: String,
}

/// The organization and the user's role in it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionOrg {
    /// The org id.
    pub id: String,
    /// The org name.
    pub name: String,
    /// The user's org role.
    pub role: OrgRole,
}

/// A project the user can see, with what they may do on it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionProject {
    /// The project id.
    pub id: String,
    /// The name.
    pub name: String,
    /// The slug.
    pub slug: String,
    /// The user's effective role on the project.
    pub role: ProjectRole,
    /// The project permissions of that role, by name (see `docs/architecture/authz-matrix.md`).
    pub permissions: Vec<String>,
}

/// The session's own times.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionTimes {
    /// The absolute expiry.
    #[schema(format = DateTime)]
    pub expires_at: String,
    /// The idle expiry, which each request slides.
    #[schema(format = DateTime)]
    pub idle_expires_at: String,
    /// How the session signed in.
    pub auth_method: AuthMethod,
}

/// Whether an optional module is on.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct FeatureState {
    /// Whether it is on.
    pub enabled: bool,
    /// Why it is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reason: Option<String>,
}

/// Media uploads.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MediaFeature {
    /// Whether an object store is configured.
    pub enabled: bool,
    /// The largest upload, in bytes.
    pub max_bytes: u32,
}

/// The server's optional modules.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Features {
    /// The SQL workspace.
    pub sql_workspace: FeatureState,
    /// Media.
    pub media: MediaFeature,
    /// Whether the server sends email.
    pub smtp: bool,
    /// How people join (`invite_only`).
    pub signup_mode: String,
}

/// The server.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ServerInfo {
    /// The server version.
    pub version: String,
}

/// The session (`GET auth/session`, contract 12.7). The UI sends `csrf_token` as `X-Crucible-CSRF` on every
/// request other than GET and HEAD.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SessionInfo {
    /// The user.
    pub user: SessionUser,
    /// The organization.
    pub org: SessionOrg,
    /// The projects the user can see.
    pub projects: Vec<SessionProject>,
    /// The enabled sign-in providers.
    pub providers: Vec<ProviderButton>,
    /// Whether email-and-password login is on.
    pub password_login_enabled: bool,
    /// The CSRF token of the session.
    pub csrf_token: String,
    /// The session's times.
    pub session: SessionTimes,
    /// The server's optional modules.
    pub features: Features,
    /// The server.
    pub server: ServerInfo,
}

/// `POST auth/password/change`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    /// The current password.
    pub current_password: String,
    /// The new password, 12 to 128 characters.
    pub new_password: String,
}

/// `POST auth/password/reset-request`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PasswordResetRequest {
    /// The account's email.
    pub email: String,
}

/// `POST auth/password/reset-confirm`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct PasswordResetConfirm {
    /// The token of the reset link.
    pub token: String,
    /// The new password, 12 to 128 characters.
    pub new_password: String,
}

/// `POST auth/invitations/accept`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct InvitationAccept {
    /// The token of the invitation link.
    pub token: String,
    /// The display name.
    pub name: String,
    /// The password, 12 to 128 characters.
    pub password: String,
}

/// `POST auth/setup`: the first owner, with the one-time setup token from the server log.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SetupRequest {
    /// The setup token.
    pub token: String,
    /// The organization's name.
    pub org_name: String,
    /// The owner's email.
    pub email: String,
    /// The owner's display name.
    pub name: String,
    /// The owner's password, 12 to 128 characters.
    pub password: String,
}

/// The signed-in user's own profile (`GET account`).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Account {
    /// The user id.
    pub id: String,
    /// The email.
    pub email: String,
    /// The display name.
    pub name: String,
}

/// `PATCH account`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AccountPatch {
    /// The new display name.
    pub name: String,
}

/// The organization's settings.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct OrgSettings {
    /// How people join (`invite_only`).
    pub signup_mode: String,
    /// Email domains whose verified members may join without an invitation, as viewers.
    pub allowed_email_domains: Vec<String>,
    /// Whether email-and-password login is on.
    pub password_login_enabled: bool,
    /// How org members and viewers reach projects.
    pub project_access: ProjectAccess,
}

/// The organization (`GET org`). Its `version` is also the `ETag`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Org {
    /// The org id.
    pub id: String,
    /// The name.
    pub name: String,
    /// The settings.
    pub settings: OrgSettings,
    /// The version, for `If-Match`.
    pub version: u32,
}

/// `PATCH org`: the fields to change.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct OrgPatch {
    /// The name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
    /// Email domains whose verified members may join without an invitation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allowed_email_domains: Option<Vec<String>>,
    /// Whether email-and-password login is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub password_login_enabled: Option<bool>,
    /// How org members and viewers reach projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub project_access: Option<ProjectAccess>,
}

/// A member of the organization.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct OrgMember {
    /// The user id.
    pub user_id: String,
    /// The email.
    pub email: String,
    /// The display name.
    pub name: String,
    /// The org role.
    pub role: OrgRole,
    /// Whether the user may sign in.
    pub status: UserStatus,
}

/// The organization's members.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct OrgMemberList {
    /// The members, ordered by user id.
    pub data: Vec<OrgMember>,
}

/// `PATCH org/members/{user_id}`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct OrgMemberPatch {
    /// The new org role; `owner` only through `org/transfer-ownership`.
    pub role: OrgRole,
}

/// `POST org/transfer-ownership`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TransferOwnership {
    /// The member who becomes the owner; the current owner becomes an admin.
    pub user_id: String,
}

/// A project membership an invitation grants.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProjectGrant {
    /// The project id.
    pub project_id: String,
    /// The role.
    pub role: ProjectRole,
}

/// An invitation.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Invitation {
    /// The invitation id.
    pub id: String,
    /// The invited email.
    pub email: String,
    /// The org role the invitee gets.
    pub org_role: OrgRole,
    /// The project memberships the invitee gets.
    pub project_roles: Vec<ProjectGrant>,
    /// When the invitation stops working.
    #[schema(format = DateTime)]
    pub expires_at: String,
    /// When it was accepted.
    #[schema(required = true, format = DateTime)]
    pub accepted_at: Option<String>,
    /// When it was revoked.
    #[schema(required = true, format = DateTime)]
    pub revoked_at: Option<String>,
}

/// The organization's invitations.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct InvitationList {
    /// The invitations, ordered by id.
    pub data: Vec<Invitation>,
}

/// `POST org/invitations`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct InvitationCreate {
    /// The email to invite.
    pub email: String,
    /// The org role the invitee gets; never `owner`.
    pub org_role: OrgRole,
    /// The project memberships the invitee gets.
    #[serde(default)]
    #[ts(optional)]
    pub project_roles: Option<Vec<ProjectGrant>>,
}

/// A new invitation with its link, shown once. The server sends no email in this version, so the inviter passes
/// the link on.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct InvitationCreated {
    /// The invitation.
    pub invitation: Invitation,
    /// The path of the acceptance page with the token, for example `/invite/?token=...`.
    pub accept_path: String,
}

/// A sign-in provider as administrators see it. The client secret is never returned.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AuthProvider {
    /// The provider id.
    pub id: String,
    /// The kind.
    pub kind: ProviderKind,
    /// The text on the button.
    pub display_name: String,
    /// The OpenID Connect issuer; empty for GitHub.
    pub issuer: String,
    /// The OAuth client id.
    pub client_id: String,
    /// Scopes requested besides those the kind needs.
    pub scopes: Vec<String>,
    /// When not empty, only these email domains may sign in through the provider.
    pub allowed_domains: Vec<String>,
    /// Whether the provider is offered.
    pub enabled: bool,
    /// Whether the provider comes from the server configuration, which makes it read-only here.
    pub seeded: bool,
    /// The callback URL to register with the provider.
    pub redirect_uri: String,
    /// The version, for `If-Match`.
    pub version: u32,
}

/// The sign-in providers.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AuthProviderList {
    /// The providers, ordered by id.
    pub data: Vec<AuthProvider>,
}

/// `POST org/auth-providers`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AuthProviderCreate {
    /// The kind.
    pub kind: ProviderKind,
    /// The text on the button.
    pub display_name: String,
    /// The OpenID Connect issuer; required for `oidc`, ignored otherwise.
    #[serde(default)]
    #[ts(optional)]
    pub issuer: Option<String>,
    /// The OAuth client id.
    pub client_id: String,
    /// The OAuth client secret; stored sealed under the master key.
    pub client_secret: String,
    /// Scopes requested besides those the kind needs.
    #[serde(default)]
    #[ts(optional)]
    pub scopes: Option<Vec<String>>,
    /// When not empty, only these email domains may sign in through the provider.
    #[serde(default)]
    #[ts(optional)]
    pub allowed_domains: Option<Vec<String>>,
}

/// `PATCH org/auth-providers/{provider_id}`: the fields to change.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AuthProviderPatch {
    /// The text on the button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub display_name: Option<String>,
    /// The OpenID Connect issuer, for `oidc`; ignored otherwise. A new issuer unlinks the accounts that signed in
    /// through the provider, and they link again through their verified email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub issuer: Option<String>,
    /// The OAuth client id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub client_id: Option<String>,
    /// A new OAuth client secret. Leave it out to keep the stored one; an empty one is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub client_secret: Option<String>,
    /// Scopes requested besides those the kind needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub scopes: Option<Vec<String>>,
    /// When not empty, only these email domains may sign in through the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub allowed_domains: Option<Vec<String>>,
    /// Whether the provider is offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub enabled: Option<bool>,
}

/// A project.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Project {
    /// The project id.
    pub id: String,
    /// The name.
    pub name: String,
    /// The slug; unique.
    pub slug: String,
    /// The caller's effective role.
    pub role: ProjectRole,
    /// The version, for `If-Match`.
    pub version: u32,
}

/// The projects the caller can see.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ProjectList {
    /// The projects, ordered by id.
    pub data: Vec<Project>,
}

/// `POST projects`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProjectCreate {
    /// The name.
    pub name: String,
    /// The slug: 1 to 64 characters of `a-z`, `0-9` and `-`.
    pub slug: String,
}

/// `PATCH projects/{project_id}`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProjectPatch {
    /// The new name.
    pub name: String,
}

/// A member of a project.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ProjectMember {
    /// The user id.
    pub user_id: String,
    /// The email.
    pub email: String,
    /// The display name.
    pub name: String,
    /// The project role of the membership.
    pub role: ProjectRole,
}

/// The members of a project (explicit memberships).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ProjectMemberList {
    /// The members, ordered by user id.
    pub data: Vec<ProjectMember>,
}

/// `POST projects/{project_id}/members`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemberAdd {
    /// An org member's user id.
    pub user_id: String,
    /// The role.
    pub role: ProjectRole,
}

/// `PATCH projects/{project_id}/members/{user_id}`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemberPatch {
    /// The new role.
    pub role: ProjectRole,
}

/// An API key. The secret key is shown only once, by `POST`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ApiKey {
    /// The key id.
    pub id: String,
    /// The public key, `cpk_...`.
    pub public_key: String,
    /// What the key may do.
    pub scopes: Vec<KeyScope>,
    /// The note.
    pub note: String,
    /// When the key stops working.
    #[schema(required = true, format = DateTime)]
    pub expires_at: Option<String>,
    /// When the key was last used, to the minute.
    #[schema(required = true, format = DateTime)]
    pub last_used_at: Option<String>,
    /// When the key was revoked.
    #[schema(required = true, format = DateTime)]
    pub revoked_at: Option<String>,
    /// The version, for `If-Match`.
    pub version: u32,
}

/// The keys of a project.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ApiKeyList {
    /// The keys, ordered by id.
    pub data: Vec<ApiKey>,
}

/// `POST projects/{project_id}/api-keys`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyCreate {
    /// What the key may do; `ingest`, `read` and `write` when absent.
    #[serde(default)]
    #[ts(optional)]
    pub scopes: Option<Vec<KeyScope>>,
    /// A note.
    #[serde(default)]
    #[ts(optional)]
    pub note: Option<String>,
    /// When the key stops working.
    #[serde(default)]
    #[ts(optional)]
    pub expires_at: Option<String>,
}

/// A new key with its secret, which is never shown again.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ApiKeyCreated {
    /// The key.
    pub key: ApiKey,
    /// The secret key, `csk_...`.
    pub secret_key: String,
}

/// `PATCH projects/{project_id}/api-keys/{key_id}`: the fields to change.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyPatch {
    /// What the key may do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub scopes: Option<Vec<KeyScope>>,
    /// The note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub note: Option<String>,
    /// When the key stops working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub expires_at: Option<String>,
}
