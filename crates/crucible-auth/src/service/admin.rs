//! The organization, its members and invitations, sign-in providers, projects, project members and API keys
//! (contracts 11.3, 11.5 and 11.6). The API layer checks the caller's permission before calling; the rules here
//! are the ones a permission cannot express (an admin cannot touch owners, the owner changes only by transfer).

use crucible_control::{
    ApiKey, AuthProvider, ControlTx, DeletionRequest, Invitation, JsonDoc, KeyChangeKind, KeyScope,
    OneTimeToken, Org, OrgMembership, OrgRole, Project, ProjectAccess, ProjectMembership,
    ProjectRole, ProviderKind, Secret, TokenKind, TxError, User, transaction,
};
use crucible_records::{ApiKeyId, DeletionRequestId, ProjectId, Timestamp, UserId, Uuid};

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

/// What `POST org/auth-providers` sets, and what the server configuration declares of a provider.
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
    /// The issuer, for `oidc`; GitHub and Google keep theirs, as on create.
    pub issuer: Option<String>,
    /// The client id.
    pub client_id: Option<String>,
    /// A new client secret; without one the stored secret stays.
    pub client_secret: Option<String>,
    /// Extra scopes.
    pub scopes: Option<Vec<String>>,
    /// Allowed email domains.
    pub allowed_domains: Option<Vec<String>>,
    /// Whether it is offered.
    pub enabled: Option<bool>,
}

impl std::fmt::Debug for ProviderChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderChange")
            .field("display_name", &self.display_name)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// How a start changed the row of a provider from the configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seeded {
    /// The configuration declares it for the first time.
    Added,
    /// The configuration changed it; its row and id stay.
    Changed,
    /// The configuration no longer declares it: its row stays, turned off.
    TurnedOff,
    /// The configuration no longer declares it, but it is the last way to sign in (password sign-in is off and no
    /// other provider is offered), so its row stays on. This server cannot sign anyone in through it, because its
    /// client secret was in the configuration.
    KeptOn,
}

/// Whether the sign-in page offers a provider, as this node sees it (contract 11.4): the provider is on, its
/// callback URL can be built from `server.public_url`, and one from the configuration is still declared there,
/// which holds its secret.
#[derive(Clone, Debug)]
struct Offering {
    public_url: bool,
    configured: Vec<ProviderKind>,
}

impl Offering {
    fn offers(&self, provider: &AuthProvider) -> bool {
        provider.enabled
            && self.public_url
            && (!provider.seeded || self.configured.contains(&provider.kind))
    }
}

/// Inside the transaction that turns off or deletes the provider `id` at version `expected`: refuses the change when
/// it leaves no way to sign in, and moves the organization's version. The guard reads the organization and the
/// providers and the change writes one provider, so two changes made at once could each count the other's provider
/// as a way in; with both writing the organization's row, one of them fails instead.
async fn keep_a_way_in(
    tx: &dyn ControlTx,
    offering: &Offering,
    id: Uuid,
    expected: u64,
) -> Result<Result<(), AuthError>, TxError> {
    let providers = tx.auth_providers().list().await?;
    // The organization's row moves only together with the provider's change, so a stale version writes nothing.
    match providers.iter().find(|provider| provider.id == id) {
        Some(provider) if provider.version == expected => {}
        Some(_) => return Ok(Err(AuthError::PreconditionFailed)),
        None => return Ok(Err(AuthError::NotFound)),
    }
    let Some(org) = tx.org().get().await? else {
        return Ok(Ok(()));
    };
    let another = providers
        .iter()
        .any(|provider| provider.id != id && offering.offers(provider));
    if !org.settings.password_login_enabled && !another {
        return Ok(Err(AuthError::Conflict(
            "password sign-in is off and no other sign-in provider is offered, so nobody could sign in: turn \
             password sign-in on in the organization settings, or turn on another provider, first"
                .to_owned(),
        )));
    }
    let mut moved = org.clone();
    moved.version += 1;
    if !tx.org().update_if_version(&moved, org.version).await? {
        return Err(TxError::SerializationFailure);
    }
    Ok(Ok(()))
}

/// The issuer a provider of `kind` keeps: the URL given for OpenID Connect, Google's own, none for GitHub.
fn issuer(kind: ProviderKind, given: Option<String>) -> Result<String, AuthError> {
    match kind {
        ProviderKind::Oidc => {
            let issuer = given.unwrap_or_default();
            if !(issuer.starts_with("https://") || issuer.starts_with("http://")) {
                return Err(AuthError::Invalid {
                    field: "issuer",
                    message: "use the issuer URL, for example https://login.example.com".to_owned(),
                });
            }
            Ok(issuer)
        }
        ProviderKind::Google => Ok(crate::oauth::GOOGLE_ISSUER.to_owned()),
        ProviderKind::Github => Ok(String::new()),
    }
}

/// A new key with its secret, shown once. `Debug` never shows the secret.
pub struct KeyCreated {
    /// The stored key.
    pub key: ApiKey,
    /// The secret key.
    pub secret: String,
}

impl std::fmt::Debug for KeyCreated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyCreated")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
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
    /// Changes the organization; `expected` is the `If-Match` version. Turning password sign-in off is refused
    /// while it is the only way to sign in.
    pub async fn update_org(&self, expected: u64, change: OrgChange) -> Result<Org, AuthError> {
        let mut org = self.org().await?.ok_or(AuthError::NotFound)?;
        check_version(org.version, Some(expected))?;
        if let Some(name) = &change.name {
            org.name = display_name("name", name)?;
        }
        if let Some(list) = change.allowed_email_domains {
            org.settings.allowed_email_domains = domains("allowed_email_domains", list)?;
        }
        let password_goes =
            org.settings.password_login_enabled && change.password_login_enabled == Some(false);
        if let Some(enabled) = change.password_login_enabled {
            org.settings.password_login_enabled = enabled;
        }
        if let Some(access) = change.project_access {
            org.settings.project_access = access;
        }
        // The providers are read after the organization: a provider turned off meanwhile moved the organization's
        // version, so the write below fails instead of passing on what it read.
        if password_goes {
            self.password_can_go().await?;
        }
        let version = org.version;
        org.version += 1;
        if !self.store.org().update_if_version(&org, version).await? {
            return Err(AuthError::PreconditionFailed);
        }
        self.forget_sessions();
        Ok(org)
    }

    /// Refuses turning password sign-in off while the sign-in page offers no provider (contract 1.7: the refusal
    /// says what to turn on first).
    async fn password_can_go(&self) -> Result<(), AuthError> {
        let offering = self.offering();
        let providers = self.store.auth_providers().list().await?;
        if providers.iter().any(|provider| offering.offers(provider)) {
            return Ok(());
        }
        Err(AuthError::Conflict(if offering.public_url {
            "password sign-in is the only way to sign in: add or turn on a sign-in provider first, then turn \
             password sign-in off"
                .to_owned()
        } else {
            "password sign-in is the only way to sign in, because sign-in providers are offered only once \
             server.public_url is set: set server.public_url and turn on a sign-in provider first, then turn \
             password sign-in off"
                .to_owned()
        }))
    }

    /// Turns password sign-in on, for a deployment whose providers no longer sign anyone in (`crucible admin
    /// enable-password-login`). `false` when it was on already; `NotFound` before first-owner setup.
    pub async fn enable_password_login(&self) -> Result<bool, AuthError> {
        let on = OrgChange {
            password_login_enabled: Some(true),
            ..OrgChange::default()
        };
        // Another change to the organization may land between the read and the write; then read again.
        for _ in 0..5 {
            let org = self.org().await?.ok_or(AuthError::NotFound)?;
            if org.settings.password_login_enabled {
                return Ok(false);
            }
            match self.update_org(org.version, on.clone()).await {
                Err(AuthError::PreconditionFailed) => {}
                done => return done.map(|_| true),
            }
        }
        Err(AuthError::PreconditionFailed)
    }

    /// Whether nobody can sign in: password sign-in is off and the sign-in page offers no provider. The server
    /// says so at start, with the command that turns password sign-in back on.
    pub async fn nobody_can_sign_in(&self) -> Result<bool, AuthError> {
        let Some(org) = self.org().await? else {
            return Ok(false);
        };
        Ok(!org.settings.password_login_enabled && self.sign_in_providers().await?.is_empty())
    }

    fn offering(&self) -> Offering {
        Offering {
            public_url: self.config.public_url.is_some(),
            configured: self.config.providers.iter().map(|p| p.kind).collect(),
        }
    }

    /// Whether the sign-in page offers `provider`.
    pub(super) fn offers(&self, provider: &AuthProvider) -> bool {
        self.offering().offers(provider)
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

    /// The row of provider `id` that `input` describes, checked as `POST org/auth-providers` checks it. A row from
    /// the configuration keeps its secret there, so it names no stored secret.
    fn provider_row(
        id: Uuid,
        input: &ProviderInput,
        seeded: bool,
    ) -> Result<AuthProvider, AuthError> {
        let display_name = display_name("display_name", &input.display_name)?;
        let issuer = issuer(input.kind, input.issuer.clone())?;
        if input.client_id.trim().is_empty() || input.client_secret.is_empty() {
            return Err(AuthError::Invalid {
                field: "client_id",
                message: "give the client id and secret the provider issued".to_owned(),
            });
        }
        Ok(AuthProvider {
            id,
            kind: input.kind,
            display_name,
            issuer,
            client_id: input.client_id.trim().to_owned(),
            secret_id: (!seeded).then_some(id),
            scopes: input.scopes.clone(),
            allowed_domains: domains("allowed_domains", input.allowed_domains.clone())?,
            enabled: true,
            seeded,
            version: 1,
        })
    }

    /// Adds a sign-in provider, sealing its client secret.
    pub async fn add_provider(&self, input: ProviderInput) -> Result<AuthProvider, AuthError> {
        let provider = Self::provider_row(self.env.new_id(), &input, false)?;
        let secret = self.seal_secret(provider.id, &input.client_secret);
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

    /// The client secret of a provider: from the configuration for a row seeded from it, otherwise opened from its
    /// sealed row.
    pub(super) async fn provider_secret(
        &self,
        provider: &AuthProvider,
    ) -> Result<String, AuthError> {
        if provider.seeded {
            return self
                .config
                .providers
                .iter()
                .find(|configured| configured.kind == provider.kind)
                .map(|configured| configured.client_secret.clone())
                .ok_or_else(|| {
                    AuthError::Internal(
                        "the server configuration no longer declares this provider".to_owned(),
                    )
                });
        }
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

    /// A provider added through the API, at the version `expected` names.
    async fn editable_provider(
        &self,
        id: Uuid,
        expected: u64,
        verb: &str,
    ) -> Result<AuthProvider, AuthError> {
        let provider = self
            .store
            .auth_providers()
            .get(id)
            .await?
            .ok_or(AuthError::NotFound)?;
        if provider.seeded {
            return Err(AuthError::Forbidden(format!(
                "this provider comes from the server configuration; {verb} it there"
            )));
        }
        check_version(provider.version, Some(expected))?;
        Ok(provider)
    }

    /// Changes a provider added through the API; `expected` is the `If-Match` version. The client id, issuer and
    /// scopes are checked as on create; without a client secret the stored one stays. A new issuer unlinks the
    /// provider's accounts, because a subject at another issuer is another person; people link again through their
    /// verified email. Turning off the last way to sign in is refused.
    pub async fn update_provider(
        &self,
        id: Uuid,
        expected: u64,
        change: ProviderChange,
    ) -> Result<AuthProvider, AuthError> {
        let current = self.editable_provider(id, expected, "change").await?;
        let mut provider = current.clone();
        if let Some(name) = change.display_name {
            provider.display_name = display_name("display_name", &name)?;
        }
        if let Some(given) = change.issuer
            && provider.kind == ProviderKind::Oidc
        {
            provider.issuer = issuer(ProviderKind::Oidc, Some(given))?;
        }
        if let Some(client_id) = change.client_id {
            if client_id.trim().is_empty() {
                return Err(AuthError::Invalid {
                    field: "client_id",
                    message: "give the client id the provider issued".to_owned(),
                });
            }
            provider.client_id = client_id.trim().to_owned();
        }
        if let Some(scopes) = change.scopes {
            provider.scopes = scopes;
        }
        if let Some(list) = change.allowed_domains {
            provider.allowed_domains = domains("allowed_domains", list)?;
        }
        if let Some(enabled) = change.enabled {
            provider.enabled = enabled;
        }
        let sealed = match change.client_secret {
            Some(secret) if secret.trim().is_empty() => {
                return Err(AuthError::Invalid {
                    field: "client_secret",
                    message:
                        "leave client_secret out to keep the stored secret, or give the new secret \
                              the provider issued"
                            .to_owned(),
                });
            }
            Some(secret) => Some(self.seal_secret(provider.id, &secret)),
            None => None,
        };
        let unlink = provider.issuer != current.issuer;
        let guard = (current.enabled && !provider.enabled).then(|| self.offering());
        let stored = provider.clone();
        let written = transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                if let Some(offering) = &guard
                    && let Err(refused) = keep_a_way_in(tx, offering, id, expected).await?
                {
                    return Ok(Err(refused));
                }
                if !tx
                    .auth_providers()
                    .update_if_version(&stored, expected)
                    .await?
                {
                    return Ok(Err(AuthError::PreconditionFailed));
                }
                if let Some(secret) = &sealed {
                    tx.secrets().delete(secret.id).await?;
                    tx.secrets().insert(secret).await?;
                }
                if unlink {
                    tx.oauth_accounts().delete_for_provider(id).await?;
                }
                Ok(Ok(()))
            })
        })
        .await?;
        written?;
        if current.enabled && !provider.enabled {
            self.forget_sessions();
        }
        provider.version = expected + 1;
        Ok(provider)
    }

    /// Deletes a provider added through the API, with its linked accounts and its secret; `expected` is the
    /// `If-Match` version. Deleting the last way to sign in is refused.
    pub async fn delete_provider(&self, id: Uuid, expected: u64) -> Result<(), AuthError> {
        let provider = self.editable_provider(id, expected, "remove").await?;
        let guard = provider.enabled.then(|| self.offering());
        let deleted = transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                if let Some(offering) = &guard
                    && let Err(refused) = keep_a_way_in(tx, offering, id, expected).await?
                {
                    return Ok(Err(refused));
                }
                if !tx.auth_providers().delete_if_version(id, expected).await? {
                    return Ok(Err(AuthError::PreconditionFailed));
                }
                if let Some(secret) = provider.secret_id {
                    tx.secrets().delete(secret).await?;
                }
                Ok(Ok(()))
            })
        })
        .await?;
        deleted?;
        if provider.enabled {
            self.forget_sessions();
        }
        Ok(())
    }

    /// Makes the rows seeded from the configuration match its providers (contract 11.1), at start: a provider the
    /// configuration declares is added, or its row changed in place, since the row's id is in the callback URL; a
    /// row whose provider left the configuration stays, turned off, and comes back with the same id. A row that is
    /// the last way to sign in stays on instead. Answers what changed, by kind.
    pub async fn seed_providers(&self) -> Result<Vec<(ProviderKind, Seeded)>, AuthError> {
        // The declared kinds go first, so a provider this start adds counts as a way in when another one goes.
        let (declared, gone): (Vec<ProviderKind>, Vec<ProviderKind>) = ProviderKind::ALL
            .iter()
            .partition(|kind| self.config.providers.iter().any(|p| p.kind == **kind));
        let mut changed = Vec::new();
        for kind in declared.into_iter().chain(gone) {
            if let Some(seeded) = self.seed(kind).await? {
                changed.push((kind, seeded));
            }
        }
        Ok(changed)
    }

    async fn seed(&self, kind: ProviderKind) -> Result<Option<Seeded>, AuthError> {
        let wanted = self.config.providers.iter().find(|p| p.kind == kind);
        // Nodes that start together race on a kind's row: the one that loses reads the winner's row and goes on.
        for _ in 0..4 {
            let row = self
                .providers()
                .await?
                .into_iter()
                .find(|p| p.seeded && p.kind == kind);
            let (outcome, written) = match (wanted, row) {
                (None, None) => return Ok(None),
                (None, Some(row)) if !row.enabled => return Ok(None),
                (None, Some(row)) => {
                    let mut off = row;
                    off.enabled = false;
                    // Checked and written as an API turn-off is, so a peer that turns password sign-in off at the
                    // same time cannot pass too.
                    let offering = self.offering();
                    let turned = transaction(self.store.as_ref(), move |tx| {
                        Box::pin(async move {
                            if let Err(refused) =
                                keep_a_way_in(tx, &offering, off.id, off.version).await?
                            {
                                return Ok(Err(refused));
                            }
                            let written = tx
                                .auth_providers()
                                .update_if_version(&off, off.version)
                                .await?;
                            Ok(Ok(written))
                        })
                    })
                    .await;
                    match turned {
                        Ok(Ok(written)) => (Seeded::TurnedOff, written),
                        Ok(Err(AuthError::Conflict(_))) => return Ok(Some(Seeded::KeptOn)),
                        // The row or the organization changed meanwhile: read again.
                        Ok(Err(_)) | Err(TxError::SerializationFailure) => {
                            (Seeded::TurnedOff, false)
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                (Some(input), None) => {
                    let row = Self::provider_row(self.env.new_id(), input, true)?;
                    match self.store.auth_providers().insert(&row).await {
                        Err(TxError::UniqueViolation(_)) => (Seeded::Added, false),
                        inserted => (Seeded::Added, inserted.map(|()| true)?),
                    }
                }
                (Some(input), Some(row)) => {
                    let mut wanted = Self::provider_row(row.id, input, true)?;
                    wanted.version = row.version;
                    if wanted == row {
                        return Ok(None);
                    }
                    // A subject at another issuer is another person, as for an edit through the API.
                    let unlink = wanted.issuer != row.issuer;
                    let written = transaction(self.store.as_ref(), move |tx| {
                        Box::pin(async move {
                            let written = tx
                                .auth_providers()
                                .update_if_version(&wanted, wanted.version)
                                .await?;
                            if written && unlink {
                                tx.oauth_accounts().delete_for_provider(wanted.id).await?;
                            }
                            Ok(written)
                        })
                    })
                    .await?;
                    (Seeded::Changed, written)
                }
            };
            if written {
                return Ok(Some(outcome));
            }
        }
        Err(AuthError::Unavailable)
    }

    /// Creates a project with its salt (contract 9.3) in one transaction; the creator, an org admin or owner, is
    /// project admin through the org role.
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
        let salt = self
            .sealer
            .new_project_salt(self.env.rng.as_ref(), project.id);
        let row = project.clone();
        let created = transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                tx.projects().insert(&row).await?;
                tx.project_salts().insert_if_absent(&salt).await?;
                Ok(())
            })
        })
        .await;
        match created {
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

    /// Deletes a project (a soft delete) for the user `by`; `expected` is the `If-Match` version. Its keys are
    /// revoked in the same transaction, so they stop working here at once and on every node through the key feed,
    /// its dashboards with their widgets and its media rows are deleted with it, and the deletion request that
    /// purges the rest of its data, in the telemetry store and the control plane, is made with it (contract 13.4).
    /// Returns the deleted project's row; the media objects under its key prefix are the caller's to remove.
    pub async fn delete_project(
        &self,
        id: ProjectId,
        expected: u64,
        by: UserId,
    ) -> Result<Project, AuthError> {
        let mut project = self.live_project(id).await?;
        let version = check_version(project.version, Some(expected))?;
        let now = self.now();
        project.deleted_at = Some(now);
        project.version += 1;
        let deleted_row = project.clone();
        let purge = DeletionRequest::project(
            DeletionRequestId::from_uuid(self.env.new_id()),
            id,
            now,
            format!("user:{by}"),
        );
        let revoked: Vec<ApiKey> = self
            .store
            .api_keys()
            .list(id)
            .await?
            .into_iter()
            .filter(|key| key.revoked_at.is_none())
            .map(|mut key| {
                key.revoked_at = Some(now);
                key.version += 1;
                key
            })
            .collect();
        let writes = revoked.clone();
        let deleted = transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                if !tx.projects().update_if_version(&project, version).await? {
                    return Ok(false);
                }
                for key in &writes {
                    let changed = tx
                        .api_keys()
                        .update_if_version(key, key.version - 1, KeyChangeKind::Revoked)
                        .await?;
                    if !changed {
                        // A key changed since it was read: nothing is applied, and the caller retries.
                        return Err(crucible_control::TxError::SerializationFailure);
                    }
                }
                tx.dashboards().delete_project(id).await?;
                // A media use recorded after this transaction's snapshot keeps its object row: the race is lost,
                // nothing is applied, and the caller retries.
                tx.media()
                    .delete_project(id)
                    .await
                    .map_err(|error| match error {
                        crucible_control::TxError::ForeignKeyViolation(_) => {
                            crucible_control::TxError::SerializationFailure
                        }
                        other => other,
                    })?;
                // With the project's row, so that a deleted project always has the request that purges it.
                tx.lifecycle().insert_request(&purge).await?;
                Ok(true)
            })
        })
        .await?;
        if !deleted {
            return Err(AuthError::PreconditionFailed);
        }
        for key in &revoked {
            self.keys.apply_local(key);
        }
        self.forget_sessions();
        Ok(deleted_row)
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

    /// The live project with this slug or id.
    pub async fn project_by_reference(&self, reference: &str) -> Result<Project, AuthError> {
        let project = match reference.parse::<Uuid>() {
            Ok(id) => self.store.projects().get(ProjectId::from_uuid(id)).await?,
            Err(_) => self.store.projects().get_by_slug(reference).await?,
        };
        match project {
            Some(project) if project.deleted_at.is_none() => Ok(project),
            _ => Err(AuthError::NotFound),
        }
    }

    /// Mints a key for a project; it works on this node before the response returns. `import` lets the key send
    /// spans older than the event-age limit (contract 1.2); only `crucible admin create-key --import` sets it.
    pub async fn create_key(
        &self,
        project: ProjectId,
        scopes: Option<Vec<KeyScope>>,
        note: Option<String>,
        expires_at: Option<Timestamp>,
        import: bool,
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
            import,
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
