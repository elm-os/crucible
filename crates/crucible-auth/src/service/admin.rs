//! The organization, its members and invitations, sign-in providers, projects, project members and API keys
//! (contracts 11.3, 11.5 and 11.6). The API layer checks the caller's permission before calling; the rules here
//! are the ones a permission cannot express (an admin cannot touch owners, the owner changes only by transfer).

use crucible_control::{
    ApiKey, AuthProvider, Invitation, JsonDoc, KeyChangeKind, KeyScope, OneTimeToken, Org,
    OrgMembership, OrgRole, Project, ProjectAccess, ProjectMembership, ProjectRole, ProviderKind,
    Secret, TokenKind, User, transaction,
};
use crucible_records::{ApiKeyId, ProjectId, Timestamp, UserId, Uuid};

use super::account::{display_name, email, invitation_expiry};
use super::sessions::SessionPrincipal;
use super::{AuthError, Identity};
use crate::mint::mint;
use crate::seal::Sealed;
use crate::token::Token;

/// The fields of `PATCH org`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct OrgChange {
    /// The name.
    pub name: Option<String>,
    /// Email domains whose verified members may join as viewers without an invitation.
    pub allowed_email_domains: Option<Vec<String>>,
    /// Whether password sign-in is on.
    pub password_login_enabled: Option<bool>,
    /// How org members and viewers reach projects.
    pub project_access: Option<ProjectAccess>,
}

/// A new invitation and the token of its link, shown once.
#[derive(Debug)]
pub struct InvitationCreated {
    /// The invitation.
    pub invitation: Invitation,
    /// The token for `/invite/?token=`.
    pub token: Token,
}

/// What `POST org/auth-providers` sets.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderInput {
    /// The kind.
    pub kind: ProviderKind,
    /// The text on the button.
    pub display_name: String,
    /// The issuer, for `oidc`.
    pub issuer: Option<String>,
    /// The client id.
    pub client_id: String,
    /// The client secret, sealed before it is stored.
    pub client_secret: String,
    /// Extra scopes.
    pub scopes: Vec<String>,
    /// Allowed email domains.
    pub allowed_domains: Vec<String>,
}

impl std::fmt::Debug for ProviderInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderInput")
            .field("kind", &self.kind)
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// The fields of `PATCH org/auth-providers/{id}`.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ProviderChange {
    /// The text on the button.
    pub display_name: Option<String>,
    /// A new client secret.
    pub client_secret: Option<String>,
    /// Allowed email domains.
    pub allowed_domains: Option<Vec<String>>,
    /// Whether it is offered.
    pub enabled: Option<bool>,
}

impl std::fmt::Debug for ProviderChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderChange")
            .field("display_name", &self.display_name)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// A new key with its secret, shown once.
#[derive(Debug)]
pub struct KeyCreated {
    /// The stored key.
    pub key: ApiKey,
    /// The secret key.
    pub secret: String,
}

/// The fields of `PATCH projects/{id}/api-keys/{key_id}`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ApiKeyChange {
    /// The scopes.
    pub scopes: Option<Vec<KeyScope>>,
    /// The note.
    pub note: Option<String>,
    /// A new expiry.
    pub expires_at: Option<Timestamp>,
}

/// The associated data that binds a provider's sealed client secret to its row.
fn secret_aad(id: Uuid) -> Vec<u8> {
    format!("{id}\0auth_provider\0org").into_bytes()
}

fn domains(field: &'static str, list: Vec<String>) -> Result<Vec<String>, AuthError> {
    list.into_iter()
        .map(|d| {
            let d = d.trim().trim_start_matches('@').to_ascii_lowercase();
            if d.is_empty() || d.contains(['@', ' ', '/']) {
                Err(AuthError::Invalid {
                    field,
                    message: "use bare domains such as example.com".to_owned(),
                })
            } else {
                Ok(d)
            }
        })
        .collect()
}

/// A project slug: 1 to 64 characters of `a-z`, `0-9` and `-`.
fn slug(text: &str) -> Result<String, AuthError> {
    let ok = !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(text.to_owned())
    } else {
        Err(AuthError::Invalid {
            field: "slug",
            message: "use 1 to 64 characters of a-z, 0-9 and -".to_owned(),
        })
    }
}

fn check_version(found: u64, expected: Option<u64>) -> Result<u64, AuthError> {
    match expected {
        Some(expected) if expected != found => Err(AuthError::PreconditionFailed),
        _ => Ok(found),
    }
}

impl Identity {
    /// Changes the organization; `expected` is the `If-Match` version.
    pub async fn update_org(&self, expected: u64, change: OrgChange) -> Result<Org, AuthError> {
        let mut org = self.org().await?.ok_or(AuthError::NotFound)?;
        check_version(org.version, Some(expected))?;
        if let Some(name) = &change.name {
            org.name = display_name("name", name)?;
        }
        if let Some(list) = change.allowed_email_domains {
            org.settings.allowed_email_domains = domains("allowed_email_domains", list)?;
        }
        if let Some(enabled) = change.password_login_enabled {
            org.settings.password_login_enabled = enabled;
        }
        if let Some(access) = change.project_access {
            org.settings.project_access = access;
        }
        let version = org.version;
        org.version += 1;
        if !self.store.org().update_if_version(&org, version).await? {
            return Err(AuthError::PreconditionFailed);
        }
        self.forget_sessions();
        Ok(org)
    }

    /// Every org member with their user row, ordered by user id.
    pub async fn org_members(&self) -> Result<Vec<(User, OrgRole)>, AuthError> {
        let users = self.store.users().list().await?;
        let roles = self.store.memberships().list_org().await?;
        Ok(users
            .into_iter()
            .filter_map(|user| {
                let role = roles.iter().find(|m| m.user_id == user.id)?.role;
                Some((user, role))
            })
            .collect())
    }

    async fn member(&self, user_id: UserId) -> Result<(User, OrgRole), AuthError> {
        let user = self
            .store
            .users()
            .get(user_id)
            .await?
            .ok_or(AuthError::NotFound)?;
        let role = self
            .store
            .memberships()
            .org_role(user_id)
            .await?
            .ok_or(AuthError::NotFound)?;
        Ok((user, role))
    }

    /// An admin may not change owners or make one; the owner changes only by transfer.
    fn may_manage(
        actor: &SessionPrincipal,
        target: OrgRole,
        new: Option<OrgRole>,
    ) -> Result<(), AuthError> {
        if target == OrgRole::Owner || new == Some(OrgRole::Owner) {
            return Err(AuthError::Forbidden(if actor.org_role == OrgRole::Owner {
                "the owner changes only through POST org/transfer-ownership".to_owned()
            } else {
                "only the owner can change the owner".to_owned()
            }));
        }
        Ok(())
    }

    /// Sets a member's org role and revokes their sessions (contract 11.4: a role change revokes them).
    pub async fn set_org_role(
        &self,
        actor: &SessionPrincipal,
        user_id: UserId,
        role: OrgRole,
    ) -> Result<(User, OrgRole), AuthError> {
        let (user, current) = self.member(user_id).await?;
        Self::may_manage(actor, current, Some(role))?;
        if current != role {
            self.store
                .memberships()
                .put_org(OrgMembership { user_id, role })
                .await?;
            let keep = (user_id == actor.user.id).then_some(&actor.token_hash);
            self.store
                .sessions()
                .revoke_user(user_id, keep, self.now())
                .await?;
            self.forget_sessions();
        }
        Ok((user, role))
    }

    /// Removes a member: the user, their sessions and memberships go.
    pub async fn remove_member(
        &self,
        actor: &SessionPrincipal,
        user_id: UserId,
    ) -> Result<(), AuthError> {
        let (_, current) = self.member(user_id).await?;
        Self::may_manage(actor, current, None)?;
        self.store.users().delete(user_id).await?;
        self.forget_sessions();
        Ok(())
    }

    /// Makes `user_id` the owner; the caller, the current owner, becomes an admin.
    pub async fn transfer_ownership(
        &self,
        actor: &SessionPrincipal,
        user_id: UserId,
    ) -> Result<(), AuthError> {
        if actor.org_role != OrgRole::Owner {
            return Err(AuthError::Forbidden(
                "only the owner can transfer ownership".to_owned(),
            ));
        }
        if user_id == actor.user.id {
            return Err(AuthError::Invalid {
                field: "user_id",
                message: "name another member".to_owned(),
            });
        }
        self.member(user_id).await?;
        let owner = actor.user.id;
        transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                tx.memberships()
                    .put_org(OrgMembership {
                        user_id,
                        role: OrgRole::Owner,
                    })
                    .await?;
                tx.memberships()
                    .put_org(OrgMembership {
                        user_id: owner,
                        role: OrgRole::Admin,
                    })
                    .await
            })
        })
        .await?;
        self.forget_sessions();
        Ok(())
    }

    /// Invites an email with an org role and project roles; answers the link token, shown once.
    pub async fn invite(
        &self,
        actor: &SessionPrincipal,
        email_text: &str,
        org_role: OrgRole,
        project_roles: Vec<(ProjectId, ProjectRole)>,
    ) -> Result<InvitationCreated, AuthError> {
        Self::may_manage(actor, OrgRole::Viewer, Some(org_role))?;
        let email = email("email", email_text)?;
        if self
            .store
            .users()
            .get_by_email(email.as_str())
            .await?
            .is_some()
        {
            return Err(AuthError::Conflict(
                "a member with this email exists already".to_owned(),
            ));
        }
        let mut project_roles = project_roles;
        project_roles.sort();
        project_roles.dedup_by_key(|(project, _)| *project);
        for (project_id, _) in &project_roles {
            if actor.project_role(*project_id).is_none() {
                return Err(AuthError::Invalid {
                    field: "project_roles",
                    message: format!("project {project_id} does not exist"),
                });
            }
        }
        let now = self.now();
        let invitation = Invitation {
            id: self.env.new_id(),
            email,
            org_role,
            project_roles,
            expires_at: invitation_expiry(now),
            accepted_at: None,
            revoked_at: None,
        };
        let token = Token::new(self.env.rng.as_ref());
        let row = OneTimeToken {
            token_hash: token.hash(),
            kind: TokenKind::Invite,
            subject: invitation.id.to_string(),
            expires_at: invitation.expires_at,
            consumed_at: None,
        };
        let stored = invitation.clone();
        transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                tx.invitations().insert(&stored).await?;
                tx.one_time_tokens().insert(&row).await
            })
        })
        .await?;
        Ok(InvitationCreated { invitation, token })
    }

    /// Every invitation, ordered by id.
    pub async fn invitations(&self) -> Result<Vec<Invitation>, AuthError> {
        Ok(self.store.invitations().list().await?)
    }

    /// Revokes an open invitation and its link.
    pub async fn revoke_invitation(&self, id: Uuid) -> Result<(), AuthError> {
        if !self.store.invitations().revoke(id, self.now()).await? {
            return Err(AuthError::NotFound);
        }
        self.store
            .one_time_tokens()
            .delete_unused(TokenKind::Invite, &id.to_string())
            .await?;
        Ok(())
    }

    /// Every sign-in provider, ordered by id.
    pub async fn providers(&self) -> Result<Vec<AuthProvider>, AuthError> {
        Ok(self.store.auth_providers().list().await?)
    }

    /// The callback URL of a provider, from `server.public_url`; `None` without one.
    pub fn redirect_uri(&self, provider_id: Uuid) -> Option<String> {
        let base = self.config.public_url.as_deref()?.trim_end_matches('/');
        Some(format!("{base}/api/v1/auth/oauth/{provider_id}/callback"))
    }

    /// Adds a sign-in provider, sealing its client secret.
    pub async fn add_provider(&self, input: ProviderInput) -> Result<AuthProvider, AuthError> {
        let display_name = display_name("display_name", &input.display_name)?;
        let issuer = match input.kind {
            ProviderKind::Oidc => {
                let issuer = input.issuer.unwrap_or_default();
                if !(issuer.starts_with("https://") || issuer.starts_with("http://")) {
                    return Err(AuthError::Invalid {
                        field: "issuer",
                        message: "use the issuer URL, for example https://login.example.com"
                            .to_owned(),
                    });
                }
                issuer
            }
            ProviderKind::Google => crate::oauth::GOOGLE_ISSUER.to_owned(),
            ProviderKind::Github => String::new(),
        };
        if input.client_id.trim().is_empty() || input.client_secret.is_empty() {
            return Err(AuthError::Invalid {
                field: "client_id",
                message: "give the client id and secret the provider issued".to_owned(),
            });
        }
        let id = self.env.new_id();
        let secret = self.seal_secret(id, &input.client_secret);
        let provider = AuthProvider {
            id,
            kind: input.kind,
            display_name,
            issuer,
            client_id: input.client_id.trim().to_owned(),
            secret_id: Some(secret.id),
            scopes: input.scopes,
            allowed_domains: domains("allowed_domains", input.allowed_domains)?,
            enabled: true,
            seeded: false,
        };
        let stored = provider.clone();
        transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                tx.secrets().insert(&secret).await?;
                tx.auth_providers().insert(&stored).await
            })
        })
        .await?;
        Ok(provider)
    }

    fn seal_secret(&self, provider_id: Uuid, plaintext: &str) -> Secret {
        let Sealed {
            key_id,
            nonce,
            ciphertext,
        } = self.sealer.seal(
            self.env.rng.as_ref(),
            &secret_aad(provider_id),
            plaintext.as_bytes(),
        );
        Secret {
            id: provider_id,
            scope: "org".to_owned(),
            kind: "auth_provider".to_owned(),
            key_id,
            nonce,
            ciphertext,
        }
    }

    /// The opened client secret of a provider.
    pub(super) async fn provider_secret(
        &self,
        provider: &AuthProvider,
    ) -> Result<String, AuthError> {
        let id = provider
            .secret_id
            .ok_or_else(|| AuthError::Internal("the provider has no secret".to_owned()))?;
        let secret =
            self.store.secrets().get(id).await?.ok_or_else(|| {
                AuthError::Internal("the provider's secret is missing".to_owned())
            })?;
        let plaintext = self
            .sealer
            .open(
                &Sealed {
                    key_id: secret.key_id,
                    nonce: secret.nonce,
                    ciphertext: secret.ciphertext,
                },
                &secret_aad(provider.id),
            )
            .map_err(|e| AuthError::Internal(e.to_string()))?;
        String::from_utf8(plaintext).map_err(|e| AuthError::Internal(e.to_string()))
    }

    /// Changes a provider added through the API.
    pub async fn update_provider(
        &self,
        id: Uuid,
        change: ProviderChange,
    ) -> Result<AuthProvider, AuthError> {
        let mut provider = self
            .store
            .auth_providers()
            .get(id)
            .await?
            .ok_or(AuthError::NotFound)?;
        if provider.seeded {
            return Err(AuthError::Forbidden(
                "this provider comes from the server configuration; change it there".to_owned(),
            ));
        }
        if let Some(name) = change.display_name {
            provider.display_name = display_name("display_name", &name)?;
        }
        if let Some(list) = change.allowed_domains {
            provider.allowed_domains = domains("allowed_domains", list)?;
        }
        if let Some(enabled) = change.enabled {
            provider.enabled = enabled;
        }
        let sealed = change
            .client_secret
            .map(|secret| self.seal_secret(provider.id, &secret));
        let stored = provider.clone();
        transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                if let Some(secret) = &sealed {
                    tx.secrets().delete(secret.id).await?;
                    tx.secrets().insert(secret).await?;
                }
                tx.auth_providers().update(&stored).await
            })
        })
        .await?;
        Ok(provider)
    }

    /// Deletes a provider added through the API, with its linked accounts and its secret.
    pub async fn delete_provider(&self, id: Uuid) -> Result<(), AuthError> {
        let provider = self
            .store
            .auth_providers()
            .get(id)
            .await?
            .ok_or(AuthError::NotFound)?;
        if provider.seeded {
            return Err(AuthError::Forbidden(
                "this provider comes from the server configuration; remove it there".to_owned(),
            ));
        }
        transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                tx.auth_providers().delete(id).await?;
                if let Some(secret) = provider.secret_id {
                    tx.secrets().delete(secret).await?;
                }
                Ok(())
            })
        })
        .await?;
        Ok(())
    }

    /// Creates a project; the creator, an org admin or owner, is project admin through the org role.
    pub async fn create_project(&self, name: &str, slug_text: &str) -> Result<Project, AuthError> {
        let project = Project {
            id: ProjectId::from_uuid(self.env.new_id()),
            name: display_name("name", name)?,
            slug: slug(slug_text)?,
            settings: JsonDoc::empty(),
            retention_days: None,
            payload_retention_days: None,
            mcp_content_retention_days: None,
            config_version: 1,
            version: 1,
            deleted_at: None,
        };
        match self.store.projects().insert(&project).await {
            Err(crucible_control::TxError::UniqueViolation(_)) => {
                return Err(AuthError::Conflict(format!(
                    "the slug {:?} is taken; choose another",
                    project.slug
                )));
            }
            other => other?,
        }
        self.forget_sessions();
        Ok(project)
    }

    async fn live_project(&self, id: ProjectId) -> Result<Project, AuthError> {
        match self.store.projects().get(id).await? {
            Some(project) if project.deleted_at.is_none() => Ok(project),
            _ => Err(AuthError::NotFound),
        }
    }

    /// Renames a project; `expected` is the `If-Match` version.
    pub async fn rename_project(
        &self,
        id: ProjectId,
        expected: u64,
        name: &str,
    ) -> Result<Project, AuthError> {
        let mut project = self.live_project(id).await?;
        let version = check_version(project.version, Some(expected))?;
        project.name = display_name("name", name)?;
        project.version += 1;
        if !self
            .store
            .projects()
            .update_if_version(&project, version)
            .await?
        {
            return Err(AuthError::PreconditionFailed);
        }
        self.forget_sessions();
        Ok(project)
    }

    /// Deletes a project (a soft delete); `expected` is the `If-Match` version.
    pub async fn delete_project(&self, id: ProjectId, expected: u64) -> Result<(), AuthError> {
        let mut project = self.live_project(id).await?;
        let version = check_version(project.version, Some(expected))?;
        project.deleted_at = Some(self.now());
        project.version += 1;
        if !self
            .store
            .projects()
            .update_if_version(&project, version)
            .await?
        {
            return Err(AuthError::PreconditionFailed);
        }
        self.forget_sessions();
        Ok(())
    }

    /// The explicit members of a project with their user rows, ordered by user id.
    pub async fn project_members(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(User, ProjectRole)>, AuthError> {
        let memberships = self.store.memberships().list_project(id).await?;
        let mut members = Vec::new();
        for membership in memberships {
            if let Some(user) = self.store.users().get(membership.user_id).await? {
                members.push((user, membership.role));
            }
        }
        Ok(members)
    }

    /// Adds or changes an org member's role on a project.
    pub async fn set_project_role(
        &self,
        id: ProjectId,
        user_id: UserId,
        role: ProjectRole,
    ) -> Result<User, AuthError> {
        self.live_project(id).await?;
        let (user, _) = self.member(user_id).await.map_err(|_| AuthError::Invalid {
            field: "user_id",
            message: "name a member of the organization".to_owned(),
        })?;
        self.store
            .memberships()
            .put_project(ProjectMembership {
                project_id: id,
                user_id,
                role,
            })
            .await?;
        self.forget_sessions();
        Ok(user)
    }

    /// Removes a project membership.
    pub async fn remove_project_member(
        &self,
        id: ProjectId,
        user_id: UserId,
    ) -> Result<(), AuthError> {
        if !self.store.memberships().delete_project(id, user_id).await? {
            return Err(AuthError::NotFound);
        }
        self.forget_sessions();
        Ok(())
    }

    /// The keys of a project, ordered by id.
    pub async fn api_keys(&self, project: ProjectId) -> Result<Vec<ApiKey>, AuthError> {
        Ok(self.store.api_keys().list(project).await?)
    }

    /// Mints a key for a project; it works on this node before the response returns.
    pub async fn create_key(
        &self,
        project: ProjectId,
        scopes: Option<Vec<KeyScope>>,
        note: Option<String>,
        expires_at: Option<Timestamp>,
    ) -> Result<KeyCreated, AuthError> {
        self.live_project(project).await?;
        let mut scopes =
            scopes.unwrap_or_else(|| vec![KeyScope::Ingest, KeyScope::Read, KeyScope::Write]);
        scopes.sort();
        scopes.dedup();
        if scopes.is_empty() {
            return Err(AuthError::Invalid {
                field: "scopes",
                message: "give at least one scope".to_owned(),
            });
        }
        let minted = mint(self.env.rng.as_ref(), &self.lookup);
        let key = ApiKey {
            id: ApiKeyId::from_uuid(self.env.new_id()),
            project_id: project,
            public_key: minted.public_key.clone(),
            secret_salt: minted.salt,
            secret_hash: minted.hash,
            lookup_tag: minted.lookup_tag,
            scopes,
            import: false,
            note: note.unwrap_or_default(),
            expires_at,
            last_used_at: None,
            revoked_at: None,
            version: 1,
        };
        self.store.api_keys().insert(&key).await?;
        self.keys.apply_local(&key);
        Ok(KeyCreated {
            key,
            secret: minted.secret,
        })
    }

    async fn project_key(&self, project: ProjectId, id: ApiKeyId) -> Result<ApiKey, AuthError> {
        match self.store.api_keys().get(id).await? {
            Some(key) if key.project_id == project => Ok(key),
            _ => Err(AuthError::NotFound),
        }
    }

    /// Changes a key's scopes, note or expiry; `expected` is the `If-Match` version.
    pub async fn update_key(
        &self,
        project: ProjectId,
        id: ApiKeyId,
        expected: u64,
        change: ApiKeyChange,
    ) -> Result<ApiKey, AuthError> {
        let mut key = self.project_key(project, id).await?;
        let version = check_version(key.version, Some(expected))?;
        let kind = if change.expires_at.is_some() {
            KeyChangeKind::ExpiryEdited
        } else {
            KeyChangeKind::Changed
        };
        if let Some(mut scopes) = change.scopes {
            scopes.sort();
            scopes.dedup();
            if scopes.is_empty() {
                return Err(AuthError::Invalid {
                    field: "scopes",
                    message: "give at least one scope".to_owned(),
                });
            }
            key.scopes = scopes;
        }
        if let Some(note) = change.note {
            key.note = note;
        }
        if change.expires_at.is_some() {
            key.expires_at = change.expires_at;
        }
        key.version += 1;
        if !self
            .store
            .api_keys()
            .update_if_version(&key, version, kind)
            .await?
        {
            return Err(AuthError::PreconditionFailed);
        }
        self.keys.apply_local(&key);
        Ok(key)
    }

    /// Revokes a key; it stops working on this node before the response returns, and on every node that reaches
    /// Postgres within 5 s (contract 11.8).
    pub async fn revoke_key(
        &self,
        project: ProjectId,
        id: ApiKeyId,
        expected: u64,
    ) -> Result<(), AuthError> {
        let mut key = self.project_key(project, id).await?;
        let version = check_version(key.version, Some(expected))?;
        if key.revoked_at.is_some() {
            return Ok(());
        }
        key.revoked_at = Some(self.now());
        key.version += 1;
        if !self
            .store
            .api_keys()
            .update_if_version(&key, version, KeyChangeKind::Revoked)
            .await?
        {
            return Err(AuthError::PreconditionFailed);
        }
        self.keys.apply_local(&key);
        Ok(())
    }
}
