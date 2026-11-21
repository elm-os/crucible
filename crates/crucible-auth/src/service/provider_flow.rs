//! Sign-in through a provider (contracts 11.4 and 11.5): the start redirect with its flow cookie, and the
//! callback that checks `state` against the flow cookie and the stored flow, exchanges the code with PKCE (and,
//! for OpenID Connect, the nonce), then finds or links the user.
//!
//! Linking: a provider that lists allowed domains first needs a verified email at one of them, at every sign-in.
//! Then a known (provider, subject) signs its user in. Otherwise the provider must assert a verified email; then an
//! existing user with that email is linked, an open invitation for it is accepted, or, when the email's domain is in
//! `org.settings.allowed_email_domains`, a viewer joins. Anything else is `not_invited`.

use crucible_control::{
    AuthFlow, Email, OauthAccount, OrgRole, TokenKind, User, UserStatus, transaction,
};
use crucible_records::{UserId, Uuid};

use super::account::{NewSession, join};
use super::{AuthError, FLOW_TTL, Identity};
use crate::next_path::safe_next;
use crate::oauth::{FlowSecrets, ProviderConfig, authorize_url, identify};
use crate::token::{self, Token};

/// What the sign-in page offers (`GET auth/providers`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SignInView {
    /// The providers people can sign in with.
    pub providers: Vec<crucible_control::AuthProvider>,
    /// Whether password sign-in is on.
    pub password_login_enabled: bool,
    /// Whether no owner exists yet.
    pub setup_required: bool,
}

/// Where the start redirect goes, with the flow cookie's value. `Debug` shows neither: both carry the state.
pub struct FlowStart {
    /// The provider's authorization URL.
    pub location: String,
    /// The flow cookie's value: the flow's `state`.
    pub flow_cookie: Token,
}

impl std::fmt::Debug for FlowStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FlowStart(..)")
    }
}

/// What the callback's query carries. `Debug` shows only the provider's error.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct CallbackQuery {
    /// The authorization code.
    pub code: Option<String>,
    /// The state.
    pub state: Option<String>,
    /// The provider's error, when the person declined.
    pub error: Option<String>,
}

impl std::fmt::Debug for CallbackQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackQuery")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// A finished provider sign-in.
#[derive(Debug)]
pub struct SignedIn {
    /// The new session.
    pub session: NewSession,
    /// Where to send the browser.
    pub next: String,
}

/// Why a provider sign-in failed: the `error` of `/login/?error=<code>` (contract 11.4).
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum FlowFailure {
    /// The person declined at the provider.
    #[error("oauth_denied")]
    Denied,
    /// The state is missing, wrong, used or expired.
    #[error("oauth_state_invalid")]
    StateInvalid,
    /// The provider's answer was not usable.
    #[error("oauth_exchange_failed")]
    ExchangeFailed,
    /// The provider gave no verified email.
    #[error("email_unverified")]
    EmailUnverified,
    /// The email is not invited and its domain is not allowed.
    #[error("not_invited")]
    NotInvited,
    /// The provider lets only these email domains sign in, and the email is at none of them.
    #[error("not_invited")]
    DomainNotAllowed(Vec<String>),
    /// The user is disabled.
    #[error("account_disabled")]
    AccountDisabled,
    /// The provider is disabled, unknown, or unusable without `server.public_url`.
    #[error("provider_disabled")]
    ProviderDisabled,
}

impl FlowFailure {
    /// The `error` parameter of the login page.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied => "oauth_denied",
            Self::StateInvalid => "oauth_state_invalid",
            Self::ExchangeFailed => "oauth_exchange_failed",
            Self::EmailUnverified => "email_unverified",
            Self::NotInvited | Self::DomainNotAllowed(_) => "not_invited",
            Self::AccountDisabled => "account_disabled",
            Self::ProviderDisabled => "provider_disabled",
        }
    }

    /// The query of the login page's URL: `error=<code>`, and for a refused domain the domains the provider
    /// allows, comma-separated, so the page can name them.
    pub fn login_query(&self) -> String {
        match self {
            Self::DomainNotAllowed(domains) => format!(
                "error={}&domains={}",
                self.code(),
                percent_encoded(&domains.join(","))
            ),
            other => format!("error={}", other.code()),
        }
    }
}

/// `text` with every byte other than an unreserved URL character percent-encoded.
fn percent_encoded(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn exchange(error: impl std::fmt::Display) -> FlowFailure {
    tracing::warn!(%error, "a provider sign-in failed");
    FlowFailure::ExchangeFailed
}

/// The associated data of a flow's sealed verifier.
fn verifier_aad(state_hash: &[u8; 32]) -> Vec<u8> {
    [b"auth_flow\0".as_slice(), state_hash].concat()
}

impl Identity {
    /// The enabled providers people can sign in with; none without `server.public_url`, which their callback
    /// URLs need, and none from the configuration that it no longer declares.
    pub async fn sign_in_providers(
        &self,
    ) -> Result<Vec<crucible_control::AuthProvider>, AuthError> {
        if self.config.public_url.is_none() {
            return Ok(Vec::new());
        }
        Ok(self
            .providers()
            .await?
            .into_iter()
            .filter(|p| self.offers(p))
            .collect())
    }

    /// What the sign-in page offers. While Postgres is unreachable the last view read on this node is served, so
    /// a page reload during an outage still finds the sign-in mode (contract 11.4: open sessions keep working).
    pub async fn sign_in_view(&self) -> Result<SignInView, AuthError> {
        let read = async {
            let org = self.org().await?;
            Ok::<_, AuthError>(SignInView {
                providers: self.sign_in_providers().await?,
                password_login_enabled: org
                    .as_ref()
                    .is_none_or(|org| org.settings.password_login_enabled),
                setup_required: org.is_none(),
            })
        };
        let last = || {
            self.last_view
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        match read.await {
            Ok(view) => {
                *last() = Some(view.clone());
                Ok(view)
            }
            Err(AuthError::Unavailable) => last().clone().ok_or(AuthError::Unavailable),
            Err(error) => Err(error),
        }
    }

    async fn provider_config(
        &self,
        provider_id: Uuid,
    ) -> Result<(crucible_control::AuthProvider, ProviderConfig), FlowFailure> {
        let provider = match self.store.auth_providers().get(provider_id).await {
            Ok(Some(provider)) if self.offers(&provider) => provider,
            Ok(_) => return Err(FlowFailure::ProviderDisabled),
            Err(error) => return Err(exchange(error)),
        };
        let redirect_uri = self
            .redirect_uri(provider.id)
            .ok_or(FlowFailure::ProviderDisabled)?;
        let client_secret = self.provider_secret(&provider).await.map_err(exchange)?;
        let config = ProviderConfig {
            kind: provider.kind,
            issuer: provider.issuer.clone(),
            client_id: provider.client_id.clone(),
            client_secret,
            scopes: provider.scopes.clone(),
            redirect_uri,
        };
        Ok((provider, config))
    }

    /// Starts a sign-in: stores the flow and answers the provider URL and the flow cookie.
    pub async fn start_flow(
        &self,
        provider_id: Uuid,
        next: Option<&str>,
    ) -> Result<FlowStart, FlowFailure> {
        let (provider, config) = self.provider_config(provider_id).await?;
        let state = Token::new(self.env.rng.as_ref());
        let verifier = Token::new(self.env.rng.as_ref());
        let nonce = Token::new(self.env.rng.as_ref());
        let secrets = FlowSecrets {
            state: state.as_str().to_owned(),
            verifier: verifier.as_str().to_owned(),
            nonce: nonce.as_str().to_owned(),
        };
        let location = authorize_url(&self.http, &config, &secrets)
            .await
            .map_err(exchange)?;
        let state_hash = state.hash();
        let flow = AuthFlow {
            state_hash,
            provider_id: provider.id,
            sealed_verifier: self.sealer.seal_bytes(
                self.env.rng.as_ref(),
                &verifier_aad(&state_hash),
                verifier.as_str().as_bytes(),
            ),
            nonce: secrets.nonce,
            next_path: safe_next(next),
            expires_at: self.later(FLOW_TTL),
            consumed_at: None,
        };
        self.store
            .auth_flows()
            .insert(&flow)
            .await
            .map_err(exchange)?;
        Ok(FlowStart {
            location,
            flow_cookie: state,
        })
    }

    /// Finishes a sign-in at the callback. `flow_cookie` is the flow cookie's value.
    pub async fn finish_flow(
        &self,
        provider_id: Uuid,
        query: &CallbackQuery,
        flow_cookie: Option<&str>,
    ) -> Result<SignedIn, FlowFailure> {
        if query.error.is_some() {
            return Err(FlowFailure::Denied);
        }
        let (Some(state), Some(code), Some(cookie)) = (&query.state, &query.code, flow_cookie)
        else {
            return Err(FlowFailure::StateInvalid);
        };
        // The browser that started the flow holds its state in the flow cookie.
        if !token::well_formed(state) || crate::csrf::check_token(cookie, Some(state)).is_err() {
            return Err(FlowFailure::StateInvalid);
        }
        let state_hash = token::hash(state);
        let flow = self
            .store
            .auth_flows()
            .consume(&state_hash, self.now())
            .await
            .map_err(exchange)?
            .ok_or(FlowFailure::StateInvalid)?;
        if flow.provider_id != provider_id {
            return Err(FlowFailure::StateInvalid);
        }
        let verifier = self
            .sealer
            .open_bytes(&verifier_aad(&state_hash), &flow.sealed_verifier)
            .map_err(exchange)?;
        let (provider, config) = self.provider_config(provider_id).await?;
        let secrets = FlowSecrets {
            state: state.clone(),
            verifier: String::from_utf8(verifier).map_err(exchange)?,
            nonce: flow.nonce.clone(),
        };
        let identity = identify(&self.http, &config, &secrets, code, self.now())
            .await
            .map_err(exchange)?;
        let user_id = self.link(&provider, &identity).await?;
        let session = self
            .start_session(user_id, provider.kind.auth_method())
            .await
            .map_err(exchange)?;
        Ok(SignedIn {
            session,
            next: flow.next_path,
        })
    }

    /// Finds or links the user of a provider identity (contract 11.5).
    async fn link(
        &self,
        provider: &crucible_control::AuthProvider,
        identity: &crate::oauth::ProviderIdentity,
    ) -> Result<UserId, FlowFailure> {
        let store = self.store.as_ref();
        let email = match (&identity.email, identity.email_verified) {
            (Some(email), true) => Email::new(email),
            _ => None,
        };
        let domain_of = |email: &Email| {
            email
                .as_str()
                .rsplit('@')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        // Only these domains may sign in through the provider: checked at every sign-in, so an account linked
        // before the list changed is held to it too.
        if !provider.allowed_domains.is_empty() {
            let email = email.as_ref().ok_or(FlowFailure::EmailUnverified)?;
            if !provider.allowed_domains.contains(&domain_of(email)) {
                return Err(FlowFailure::DomainNotAllowed(
                    provider.allowed_domains.clone(),
                ));
            }
        }
        let linked = store
            .oauth_accounts()
            .get(provider.id, &identity.subject)
            .await
            .map_err(exchange)?;
        if let Some(account) = linked {
            return self.active(account.user_id).await;
        }
        let email = email.ok_or(FlowFailure::EmailUnverified)?;
        let domain = domain_of(&email);
        let account = |user_id| OauthAccount {
            provider_id: provider.id,
            subject: identity.subject.clone(),
            user_id,
        };
        if let Some(user) = store
            .users()
            .get_by_email(email.as_str())
            .await
            .map_err(exchange)?
        {
            let user_id = self.active(user.id).await?;
            store
                .oauth_accounts()
                .insert(&account(user_id))
                .await
                .map_err(exchange)?;
            return Ok(user_id);
        }
        let org = self
            .org()
            .await
            .map_err(exchange)?
            .ok_or(FlowFailure::NotInvited)?;
        let now = self.now();
        let invitation = store
            .invitations()
            .list_for_email(&email)
            .await
            .map_err(exchange)?
            .into_iter()
            .find(|invitation| invitation.is_open(now));
        let by_domain = org.settings.allowed_email_domains.contains(&domain);
        if invitation.is_none() && !by_domain {
            return Err(FlowFailure::NotInvited);
        }
        let user = User {
            id: UserId::from_uuid(self.env.new_id()),
            email,
            name: identity.name.clone().unwrap_or_default(),
            status: UserStatus::Active,
            email_verified_at: Some(now),
        };
        let user_id = user.id;
        let linked = account(user_id);
        transaction(store, move |tx| {
            Box::pin(async move {
                tx.users().insert(&user).await?;
                tx.oauth_accounts().insert(&linked).await?;
                match &invitation {
                    Some(invitation) => {
                        if !tx.invitations().accept(invitation.id, now).await? {
                            return Err(crucible_control::TxError::Invalid(
                                "the invitation was used meanwhile".to_owned(),
                            ));
                        }
                        tx.one_time_tokens()
                            .delete_unused(TokenKind::Invite, &invitation.id.to_string())
                            .await?;
                        join(tx, user_id, invitation).await
                    }
                    None => {
                        tx.memberships()
                            .put_org(crucible_control::OrgMembership {
                                user_id,
                                role: OrgRole::Viewer,
                            })
                            .await
                    }
                }
            })
        })
        .await
        .map_err(exchange)?;
        self.forget_sessions();
        Ok(user_id)
    }

    async fn active(&self, user_id: UserId) -> Result<UserId, FlowFailure> {
        match self.store.users().get(user_id).await.map_err(exchange)? {
            Some(user) if user.status == UserStatus::Active => Ok(user_id),
            Some(_) => Err(FlowFailure::AccountDisabled),
            None => Err(FlowFailure::NotInvited),
        }
    }
}
