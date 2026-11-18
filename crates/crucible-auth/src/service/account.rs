//! Password sign-in, sign-out, first-owner setup, invitation acceptance, password changes and resets, and the
//! user's own profile (contracts 11.4 and 11.5).

use std::sync::Arc;

use crucible_control::{
    AuthMethod, Credential, Email, Org, OrgMembership, OrgRole, OrgSettings, ProjectMembership,
    Session, TokenKind, User, UserStatus, transaction,
};
use crucible_records::{OrgId, Timestamp, UserId};

use super::sessions::SessionPrincipal;
use super::{AuthError, Identity, RESET_TTL, SESSION_ABSOLUTE, SESSION_IDLE, SETUP_TTL};
use crate::password::{PARAMS_VERSION, check_policy};
use crate::token::{self, Token};

/// A new session: the token for the cookie, and the session as requests see it.
#[derive(Debug)]
pub struct NewSession {
    /// The cookie's value.
    pub token: Token,
    /// The session.
    pub principal: Arc<SessionPrincipal>,
}

/// A one-time link token, shown once (setup, invitations, password resets).
#[derive(Debug)]
pub struct SetupLink {
    /// The token, for the link's `token` parameter.
    pub token: Token,
}

fn invalid(field: &'static str, message: impl Into<String>) -> AuthError {
    AuthError::Invalid {
        field,
        message: message.into(),
    }
}

/// A display name: 1 to 200 characters after trimming.
pub(super) fn display_name(field: &'static str, name: &str) -> Result<String, AuthError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(invalid(field, "use 1 to 200 characters"));
    }
    Ok(name.to_owned())
}

pub(super) fn email(field: &'static str, text: &str) -> Result<Email, AuthError> {
    Email::new(text.trim())
        .ok_or_else(|| invalid(field, "use an email address such as ada@example.com"))
}

fn password(field: &'static str, text: &str) -> Result<(), AuthError> {
    check_policy(text).map_err(|e| invalid(field, e.to_string()))
}

impl Identity {
    /// Creates a session for `user_id` and caches it.
    pub(super) async fn start_session(
        &self,
        user_id: UserId,
        method: AuthMethod,
    ) -> Result<NewSession, AuthError> {
        self.https_required()?;
        let token = Token::new(self.env.rng.as_ref());
        let csrf = Token::new(self.env.rng.as_ref());
        let absolute = self.later(SESSION_ABSOLUTE);
        let session = Session {
            token_hash: token.hash(),
            user_id,
            csrf_token: csrf.as_str().to_owned(),
            idle_expires_at: self.later(SESSION_IDLE).min(absolute),
            absolute_expires_at: absolute,
            auth_method: method,
            revoked_at: None,
        };
        self.store.sessions().insert(&session).await?;
        let principal = self
            .load(session.token_hash)
            .await?
            .ok_or(AuthError::Unauthenticated)?;
        let principal = Arc::new(principal);
        self.remember(&principal);
        Ok(NewSession { token, principal })
    }

    /// Signs in with email and password. Unknown emails take as long as wrong passwords.
    pub async fn login(
        &self,
        email_text: &str,
        password_text: &str,
    ) -> Result<NewSession, AuthError> {
        self.https_required()?;
        let Some(org) = self.org().await? else {
            return Err(AuthError::Forbidden(
                "no owner exists yet; finish the first-run setup at /setup/".to_owned(),
            ));
        };
        if !org.settings.password_login_enabled {
            return Err(AuthError::Forbidden(
                "password sign-in is turned off for this organization; use a sign-in provider"
                    .to_owned(),
            ));
        }
        let user = match Email::new(email_text.trim()) {
            Some(email) => self.store.users().get_by_email(email.as_str()).await?,
            None => None,
        };
        let credential = match &user {
            Some(user) => self.store.credentials().get(user.id).await?,
            None => None,
        };
        let matched = self
            .passwords
            .verify(
                password_text,
                credential.as_ref().map(|c| c.password_hash.as_str()),
            )
            .await?;
        let (Some(user), Some(credential), true) = (user, credential, matched) else {
            return Err(AuthError::InvalidCredentials);
        };
        if user.status != UserStatus::Active {
            return Err(AuthError::Forbidden(
                "this account is disabled; ask an administrator to enable it".to_owned(),
            ));
        }
        if credential.params_version != PARAMS_VERSION {
            let hash = self.passwords.hash(password_text).await?;
            let upgraded = Credential {
                password_hash: hash,
                params_version: PARAMS_VERSION,
                ..credential
            };
            if let Err(error) = self.store.credentials().put(&upgraded).await {
                tracing::warn!(%error, "upgrading a password hash failed");
            }
        }
        self.start_session(user.id, AuthMethod::Password).await
    }

    /// Revokes the caller's session.
    pub async fn logout(&self, principal: &SessionPrincipal) -> Result<(), AuthError> {
        self.forget(&principal.token_hash);
        self.store
            .sessions()
            .revoke(&principal.token_hash, self.now())
            .await?;
        Ok(())
    }

    /// Whether no organization exists yet, so the first-run setup applies.
    pub async fn setup_required(&self) -> Result<bool, AuthError> {
        Ok(self.org().await?.is_none())
    }

    /// Issues the one-time setup token when no organization exists, replacing any earlier unused one; the server
    /// prints it at startup. `None` once an owner exists.
    pub async fn issue_setup_token(&self) -> Result<Option<SetupLink>, AuthError> {
        if !self.setup_required().await? {
            return Ok(None);
        }
        let tokens = self.store.one_time_tokens();
        tokens.delete_unused(TokenKind::Setup, "").await?;
        let token = Token::new(self.env.rng.as_ref());
        tokens
            .insert(&crucible_control::OneTimeToken {
                token_hash: token.hash(),
                kind: TokenKind::Setup,
                subject: String::new(),
                expires_at: self.later(SETUP_TTL),
                consumed_at: None,
            })
            .await?;
        Ok(Some(SetupLink { token }))
    }

    /// Creates the organization and its first owner. With `token` (the setup page), the one-time setup token
    /// is consumed in the same transaction; without it (`crucible admin init`), the caller has local access.
    async fn create_owner(
        &self,
        token: Option<&str>,
        org_name: &str,
        email_text: &str,
        name: &str,
        password_text: &str,
    ) -> Result<UserId, AuthError> {
        let org_name = display_name("org_name", org_name)?;
        let email = email("email", email_text)?;
        let name = display_name("name", name)?;
        password("password", password_text)?;
        if let Some(token) = token
            && !token::well_formed(token)
        {
            return Err(AuthError::TokenInvalid);
        }
        let hash = self.passwords.hash(password_text).await?;
        let now = self.now();
        let org = Org {
            id: OrgId::from_uuid(self.env.new_id()),
            name: org_name,
            settings: OrgSettings::default(),
            version: 1,
        };
        let user = User {
            id: UserId::from_uuid(self.env.new_id()),
            email,
            name,
            status: UserStatus::Active,
            email_verified_at: None,
        };
        let token_hash = token.map(token::hash);
        let user_id = user.id;
        let result = transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                if let Some(hash) = token_hash
                    && tx
                        .one_time_tokens()
                        .consume(&hash, TokenKind::Setup, now)
                        .await?
                        .is_none()
                {
                    return Ok(Err(AuthError::TokenInvalid));
                }
                if tx.org().get().await?.is_some() {
                    return Ok(Err(AuthError::Conflict(
                        "the server already has an owner; sign in instead".to_owned(),
                    )));
                }
                tx.org().insert(&org).await?;
                tx.users().insert(&user).await?;
                tx.credentials()
                    .put(&Credential {
                        user_id: user.id,
                        password_hash: hash,
                        params_version: PARAMS_VERSION,
                        failed_count: 0,
                        locked_until: None,
                    })
                    .await?;
                tx.memberships()
                    .put_org(OrgMembership {
                        user_id: user.id,
                        role: OrgRole::Owner,
                    })
                    .await?;
                Ok(Ok(()))
            })
        })
        .await?;
        result.map(|()| user_id)
    }

    /// First-owner setup through the setup page, with the token from the server log; signs the owner in.
    pub async fn setup(
        &self,
        token: &str,
        org_name: &str,
        email_text: &str,
        name: &str,
        password_text: &str,
    ) -> Result<NewSession, AuthError> {
        self.https_required()?;
        let user_id = self
            .create_owner(Some(token), org_name, email_text, name, password_text)
            .await?;
        self.start_session(user_id, AuthMethod::Password).await
    }

    /// First-owner setup from the command line (`crucible admin init`).
    pub async fn init_owner(
        &self,
        org_name: &str,
        email_text: &str,
        name: &str,
        password_text: &str,
    ) -> Result<UserId, AuthError> {
        self.create_owner(None, org_name, email_text, name, password_text)
            .await
    }

    /// Accepts an invitation through its link: creates the user with a password and the invitation's
    /// memberships, and signs them in.
    pub async fn accept_invitation(
        &self,
        token: &str,
        name: &str,
        password_text: &str,
    ) -> Result<NewSession, AuthError> {
        self.https_required()?;
        let name = display_name("name", name)?;
        password("password", password_text)?;
        if !token::well_formed(token) {
            return Err(AuthError::TokenInvalid);
        }
        let hash = self.passwords.hash(password_text).await?;
        let now = self.now();
        let token_hash = token::hash(token);
        let user_id = UserId::from_uuid(self.env.new_id());
        let result = transaction(self.store.as_ref(), move |tx| {
            Box::pin(async move {
                let Some(used) = tx
                    .one_time_tokens()
                    .consume(&token_hash, TokenKind::Invite, now)
                    .await?
                else {
                    return Ok(Err(AuthError::TokenInvalid));
                };
                let invitation = match used.subject.parse() {
                    Ok(id) => tx.invitations().get(id).await?,
                    Err(_) => None,
                };
                let Some(invitation) = invitation else {
                    return Ok(Err(AuthError::TokenInvalid));
                };
                if !tx.invitations().accept(invitation.id, now).await? {
                    return Ok(Err(AuthError::TokenInvalid));
                }
                if tx
                    .users()
                    .get_by_email(invitation.email.as_str())
                    .await?
                    .is_some()
                {
                    return Ok(Err(AuthError::Conflict(
                        "an account with this email exists already; sign in instead".to_owned(),
                    )));
                }
                let user = User {
                    id: user_id,
                    email: invitation.email.clone(),
                    name,
                    status: UserStatus::Active,
                    // The link reached the invited address's owner.
                    email_verified_at: Some(now),
                };
                tx.users().insert(&user).await?;
                tx.credentials()
                    .put(&Credential {
                        user_id,
                        password_hash: hash,
                        params_version: PARAMS_VERSION,
                        failed_count: 0,
                        locked_until: None,
                    })
                    .await?;
                join(tx, user_id, &invitation).await?;
                Ok(Ok(()))
            })
        })
        .await?;
        result?;
        self.forget_sessions();
        self.start_session(user_id, AuthMethod::Password).await
    }

    /// Changes the caller's password and revokes their other sessions.
    pub async fn change_password(
        &self,
        principal: &SessionPrincipal,
        current: &str,
        new: &str,
    ) -> Result<(), AuthError> {
        password("new_password", new)?;
        let user_id = principal.user.id;
        let credential = self.store.credentials().get(user_id).await?;
        let matched = self
            .passwords
            .verify(
                current,
                credential.as_ref().map(|c| c.password_hash.as_str()),
            )
            .await?;
        if !matched {
            return Err(AuthError::InvalidCredentials);
        }
        self.set_password(user_id, new, Some(&principal.token_hash))
            .await
    }

    /// Stores a new password and revokes the user's sessions except `keep`.
    async fn set_password(
        &self,
        user_id: UserId,
        new: &str,
        keep: Option<&crucible_control::TokenHash>,
    ) -> Result<(), AuthError> {
        let hash = self.passwords.hash(new).await?;
        self.store
            .credentials()
            .put(&Credential {
                user_id,
                password_hash: hash,
                params_version: PARAMS_VERSION,
                failed_count: 0,
                locked_until: None,
            })
            .await?;
        self.store
            .sessions()
            .revoke_user(user_id, keep, self.now())
            .await?;
        self.forget_sessions();
        Ok(())
    }

    /// Makes a password-reset token for the user with this email (`crucible admin reset-password`), replacing
    /// earlier unused ones. The server sends no email in this version, so the operator passes the link on.
    pub async fn issue_reset(&self, email_text: &str) -> Result<SetupLink, AuthError> {
        let email = email("email", email_text)?;
        let Some(user) = self.store.users().get_by_email(email.as_str()).await? else {
            return Err(AuthError::NotFound);
        };
        let subject = user.id.to_string();
        let tokens = self.store.one_time_tokens();
        tokens
            .delete_unused(TokenKind::PasswordReset, &subject)
            .await?;
        let token = Token::new(self.env.rng.as_ref());
        tokens
            .insert(&crucible_control::OneTimeToken {
                token_hash: token.hash(),
                kind: TokenKind::PasswordReset,
                subject,
                expires_at: self.later(RESET_TTL),
                consumed_at: None,
            })
            .await?;
        Ok(SetupLink { token })
    }

    /// Sets a new password through a reset link and revokes every session of the user.
    pub async fn reset_password(&self, token: &str, new: &str) -> Result<(), AuthError> {
        password("new_password", new)?;
        if !token::well_formed(token) {
            return Err(AuthError::TokenInvalid);
        }
        let used = self
            .store
            .one_time_tokens()
            .consume(&token::hash(token), TokenKind::PasswordReset, self.now())
            .await?
            .ok_or(AuthError::TokenInvalid)?;
        let user_id = used
            .subject
            .parse()
            .map(UserId::from_uuid)
            .map_err(|_| AuthError::TokenInvalid)?;
        if self.store.users().get(user_id).await?.is_none() {
            return Err(AuthError::TokenInvalid);
        }
        self.set_password(user_id, new, None).await
    }

    /// Renames the caller.
    pub async fn rename(
        &self,
        principal: &SessionPrincipal,
        name: &str,
    ) -> Result<User, AuthError> {
        let name = display_name("name", name)?;
        let mut user = self
            .store
            .users()
            .get(principal.user.id)
            .await?
            .ok_or(AuthError::Unauthenticated)?;
        user.name = name;
        if !self.store.users().update(&user).await? {
            return Err(AuthError::Unauthenticated);
        }
        self.forget(&principal.token_hash);
        Ok(user)
    }
}

/// Adds a new user to the organization with an invitation's roles.
pub(super) async fn join(
    tx: &dyn crucible_control::ControlTx,
    user_id: UserId,
    invitation: &crucible_control::Invitation,
) -> Result<(), crucible_control::TxError> {
    tx.memberships()
        .put_org(OrgMembership {
            user_id,
            role: invitation.org_role,
        })
        .await?;
    for (project_id, role) in &invitation.project_roles {
        if tx
            .projects()
            .get(*project_id)
            .await?
            .is_some_and(|p| p.deleted_at.is_none())
        {
            tx.memberships()
                .put_project(ProjectMembership {
                    project_id: *project_id,
                    user_id,
                    role: *role,
                })
                .await?;
        }
    }
    Ok(())
}

/// When an invitation made now expires.
pub(super) fn invitation_expiry(now: Timestamp) -> Timestamp {
    now.saturating_add_micros(super::sessions::micros(super::INVITATION_TTL))
}
