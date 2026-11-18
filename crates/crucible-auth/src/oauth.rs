//! Sign-in through providers (contract 11.4): GitHub over OAuth 2.0 (`oauth2`), Google and any OpenID Connect
//! issuer over `openidconnect`, all with PKCE (S256) and, for OpenID Connect, a nonce. Requests go through
//! [`NetHttp`]; `state`, the PKCE verifier and the nonce come from `Rng`, and ID-token times are checked against
//! the `Env` clock, never the crates' own randomness or clock.

use crucible_control::ProviderKind;
use crucible_records::Timestamp;
use http::header::{ACCEPT, AUTHORIZATION};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse as _, TokenUrl,
};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, Nonce, TokenResponse as _,
};
use serde::Deserialize;

use crate::http::{HttpError, NetHttp};

/// GitHub's authorization endpoint.
const GITHUB_AUTHORIZE: &str = "https://github.com/login/oauth/authorize";
/// GitHub's token endpoint.
const GITHUB_TOKEN: &str = "https://github.com/login/oauth/access_token";
/// GitHub's API.
const GITHUB_API: &str = "https://api.github.com";
/// Google's issuer.
pub const GOOGLE_ISSUER: &str = "https://accounts.google.com";
/// How far an ID token's `iat` may lie ahead of the clock, in seconds.
const CLOCK_SKEW_SECS: i64 = 300;

/// A provider's configuration, as stored in `auth_providers` with its opened client secret.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    /// The kind.
    pub kind: ProviderKind,
    /// The OpenID Connect issuer; ignored for GitHub, [`GOOGLE_ISSUER`] for Google.
    pub issuer: String,
    /// The OAuth client id.
    pub client_id: String,
    /// The OAuth client secret.
    pub client_secret: String,
    /// The scopes requested besides those the kind needs.
    pub scopes: Vec<String>,
    /// The callback URL registered with the provider.
    pub redirect_uri: String,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("kind", &self.kind)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// The secrets of one sign-in attempt, made from `Rng` at the start and needed again at the callback.
#[derive(Clone, PartialEq, Eq)]
pub struct FlowSecrets {
    /// The `state` parameter.
    pub state: String,
    /// The PKCE verifier.
    pub verifier: String,
    /// The OpenID Connect nonce; unused for GitHub.
    pub nonce: String,
}

impl std::fmt::Debug for FlowSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FlowSecrets(..)")
    }
}

/// Who signed in, as the provider says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProviderIdentity {
    /// The provider's stable subject for the person.
    pub subject: String,
    /// The email the provider asserts, if any.
    pub email: Option<String>,
    /// Whether the provider says the email is verified.
    pub email_verified: bool,
    /// The person's name, if the provider gives one.
    pub name: Option<String>,
}

/// Why a provider step failed. The callback maps every one to `oauth_exchange_failed`; the message goes to the
/// log only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The provider's configuration does not parse (for example a URL).
    #[error("the provider configuration is invalid: {0}")]
    Config(String),
    /// A request to the provider failed or its answer does not parse.
    #[error("the provider did not answer as expected: {0}")]
    Exchange(String),
}

fn config_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::Config(error.to_string())
}

fn exchange_error(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::Exchange(error.to_string())
}

type OidcClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// The issuer of an OpenID Connect provider.
fn issuer_of(config: &ProviderConfig) -> &str {
    match config.kind {
        ProviderKind::Google => GOOGLE_ISSUER,
        ProviderKind::Github | ProviderKind::Oidc => &config.issuer,
    }
}

async fn discover(http: &NetHttp, config: &ProviderConfig) -> Result<OidcClient, ProviderError> {
    let issuer = IssuerUrl::new(issuer_of(config).to_owned()).map_err(config_error)?;
    let client = |request| http.send(request);
    let metadata = CoreProviderMetadata::discover_async(issuer, &client)
        .await
        .map_err(exchange_error)?;
    Ok(CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id.clone()),
        Some(ClientSecret::new(config.client_secret.clone())),
    )
    .set_redirect_uri(RedirectUrl::new(config.redirect_uri.clone()).map_err(config_error)?))
}

fn github_client(
    config: &ProviderConfig,
) -> Result<
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>,
    ProviderError,
> {
    Ok(BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_client_secret(ClientSecret::new(config.client_secret.clone()))
        .set_auth_uri(AuthUrl::new(GITHUB_AUTHORIZE.to_owned()).map_err(config_error)?)
        .set_token_uri(TokenUrl::new(GITHUB_TOKEN.to_owned()).map_err(config_error)?)
        .set_redirect_uri(RedirectUrl::new(config.redirect_uri.clone()).map_err(config_error)?))
}

/// The URL that sends the browser to the provider. For OpenID Connect this runs discovery.
pub async fn authorize_url(
    http: &NetHttp,
    config: &ProviderConfig,
    secrets: &FlowSecrets,
) -> Result<String, ProviderError> {
    let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
        secrets.verifier.clone(),
    ));
    let state = CsrfToken::new(secrets.state.clone());
    let extra = config.scopes.iter().map(|s| Scope::new(s.clone()));
    let url = match config.kind {
        ProviderKind::Github => {
            let (url, _) = github_client(config)?
                .authorize_url(move || state)
                .add_scopes(
                    ["read:user", "user:email"]
                        .into_iter()
                        .map(|s| Scope::new(s.to_owned()))
                        .chain(extra),
                )
                .set_pkce_challenge(challenge)
                .url();
            url
        }
        ProviderKind::Google | ProviderKind::Oidc => {
            let nonce = Nonce::new(secrets.nonce.clone());
            let (url, _, _) = discover(http, config)
                .await?
                .authorize_url(
                    CoreAuthenticationFlow::AuthorizationCode,
                    move || state,
                    move || nonce,
                )
                .add_scopes(
                    ["email", "profile"]
                        .into_iter()
                        .map(|s| Scope::new(s.to_owned()))
                        .chain(extra),
                )
                .set_pkce_challenge(challenge)
                .url();
            url
        }
    };
    Ok(url.to_string())
}

/// Exchanges the callback's `code` and returns who signed in. `now` checks the ID token's times.
pub async fn identify(
    http: &NetHttp,
    config: &ProviderConfig,
    secrets: &FlowSecrets,
    code: &str,
    now: Timestamp,
) -> Result<ProviderIdentity, ProviderError> {
    let client = |request| http.send(request);
    let verifier = PkceCodeVerifier::new(secrets.verifier.clone());
    let code = AuthorizationCode::new(code.to_owned());
    match config.kind {
        ProviderKind::Github => {
            let token = github_client(config)?
                .exchange_code(code)
                .set_pkce_verifier(verifier)
                .request_async(&client)
                .await
                .map_err(exchange_error)?;
            github_identity(http, token.access_token().secret()).await
        }
        ProviderKind::Google | ProviderKind::Oidc => {
            let oidc = discover(http, config).await?;
            let token = oidc
                .exchange_code(code)
                .map_err(config_error)?
                .set_pkce_verifier(verifier)
                .request_async(&client)
                .await
                .map_err(exchange_error)?;
            let id_token = token.id_token().ok_or_else(|| {
                ProviderError::Exchange("the token response has no ID token".to_owned())
            })?;
            let now = chrono::DateTime::from_timestamp_micros(now.micros()).unwrap_or_default();
            let latest_iat = now
                .checked_add_signed(chrono::TimeDelta::seconds(CLOCK_SKEW_SECS))
                .unwrap_or(now);
            let verifier = oidc
                .id_token_verifier()
                .set_time_fn(move || now)
                .set_issue_time_verifier_fn(move |iat| {
                    if iat <= latest_iat {
                        Ok(())
                    } else {
                        Err("the ID token was issued in the future".to_owned())
                    }
                });
            let claims = id_token
                .claims(&verifier, &Nonce::new(secrets.nonce.clone()))
                .map_err(exchange_error)?;
            Ok(ProviderIdentity {
                subject: claims.subject().as_str().to_owned(),
                email: claims.email().map(|email| email.as_str().to_owned()),
                email_verified: claims.email_verified().unwrap_or(false),
                name: claims
                    .name()
                    .and_then(|name| name.get(None))
                    .map(|name| name.as_str().to_owned()),
            })
        }
    }
}

#[derive(Deserialize)]
struct GithubUser {
    id: u64,
    name: Option<String>,
    login: String,
}

#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

async fn github_get<T: for<'de> Deserialize<'de>>(
    http: &NetHttp,
    token: &str,
    path: &str,
) -> Result<T, ProviderError> {
    let request = http::Request::get(format!("{GITHUB_API}{path}"))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(ACCEPT, "application/vnd.github+json")
        .body(Vec::new())
        .map_err(exchange_error)?;
    let response = http
        .send(request)
        .await
        .map_err(|HttpError(e)| ProviderError::Exchange(e))?;
    if !response.status().is_success() {
        return Err(ProviderError::Exchange(format!(
            "GitHub answered {} for {path}",
            response.status()
        )));
    }
    serde_json::from_slice(response.body()).map_err(exchange_error)
}

/// GitHub's identity: the numeric id as the subject, and the primary email when GitHub says it is verified.
async fn github_identity(http: &NetHttp, token: &str) -> Result<ProviderIdentity, ProviderError> {
    let user: GithubUser = github_get(http, token, "/user").await?;
    let emails: Vec<GithubEmail> = github_get(http, token, "/user/emails").await?;
    let primary = emails.into_iter().find(|email| email.primary);
    Ok(ProviderIdentity {
        subject: user.id.to_string(),
        email_verified: primary.as_ref().is_some_and(|email| email.verified),
        email: primary.map(|email| email.email),
        name: user.name.or(Some(user.login)),
    })
}

#[cfg(test)]
mod tests {
    use crucible_env::TlsRoots;

    use super::*;
    use crate::test_support::idp::{CLIENT_ID, CLIENT_SECRET, FakeIdp, FakeUser, ISSUER};
    use crate::token::Token;

    fn person(verified: bool) -> FakeUser {
        FakeUser {
            subject: "4242".to_owned(),
            email: "ada@example.com".to_owned(),
            email_verified: verified,
            name: "Ada".to_owned(),
        }
    }

    fn config(kind: ProviderKind) -> ProviderConfig {
        ProviderConfig {
            kind,
            issuer: ISSUER.to_owned(),
            client_id: CLIENT_ID.to_owned(),
            client_secret: CLIENT_SECRET.to_owned(),
            scopes: Vec::new(),
            redirect_uri: "http://crucible.test/api/v1/auth/oauth/p/callback".to_owned(),
        }
    }

    fn secrets(env: &crucible_env::Env) -> FlowSecrets {
        let token = || Token::new(env.rng.as_ref()).as_str().to_owned();
        FlowSecrets {
            state: token(),
            verifier: token(),
            nonce: token(),
        }
    }

    /// A whole sign-in, start to callback, against the fake provider of `kind`.
    async fn sign_in(
        idp: &FakeIdp,
        http: &NetHttp,
        env: &crucible_env::Env,
        config: &ProviderConfig,
        secrets: &FlowSecrets,
    ) -> Result<(ProviderIdentity, String), ProviderError> {
        let url = authorize_url(http, config, secrets).await?;
        let (code, state) = idp.authorize(&url).map_err(ProviderError::Exchange)?;
        assert_eq!(state, secrets.state, "the state round-trips");
        let identity = identify(http, config, secrets, &code, env.clock.wall_now()).await?;
        Ok((identity, code))
    }

    #[test]
    fn openid_connect_and_github_sign_ins_round_trip() {
        for seed in [1, 2] {
            crate::testing::sim(seed, async |env| {
                let idp = FakeIdp::new(&env, person(true));
                let env = idp.env(&env);
                let http = NetHttp::new(&env, TlsRoots::WebPki);
                for kind in [ProviderKind::Oidc, ProviderKind::Github] {
                    let config = config(kind);
                    let flow = secrets(&env);
                    let (identity, code) = sign_in(&idp, &http, &env, &config, &flow)
                        .await
                        .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
                    assert_eq!(
                        identity,
                        ProviderIdentity {
                            subject: "4242".to_owned(),
                            email: Some("ada@example.com".to_owned()),
                            email_verified: true,
                            name: Some("Ada".to_owned()),
                        },
                        "{kind:?}"
                    );
                    // A code works once.
                    let again = identify(&http, &config, &flow, &code, env.clock.wall_now()).await;
                    assert!(again.is_err(), "{kind:?}: {again:?}");
                    // A verifier that does not match the challenge is refused.
                    let url = authorize_url(&http, &config, &flow).await.unwrap();
                    let (code, _) = idp.authorize(&url).unwrap();
                    let wrong = FlowSecrets {
                        verifier: secrets(&env).verifier,
                        ..flow.clone()
                    };
                    let refused =
                        identify(&http, &config, &wrong, &code, env.clock.wall_now()).await;
                    assert!(refused.is_err(), "{kind:?}: {refused:?}");
                }
            });
        }
    }

    #[test]
    fn id_tokens_are_checked_for_nonce_and_expiry_and_unverified_emails_say_so() {
        crate::testing::sim(3, async |env| {
            let idp = FakeIdp::new(&env, person(false));
            let env = idp.env(&env);
            let http = NetHttp::new(&env, TlsRoots::WebPki);
            let config = config(ProviderKind::Oidc);
            let flow = secrets(&env);
            let (identity, _) = sign_in(&idp, &http, &env, &config, &flow).await.unwrap();
            assert!(!identity.email_verified);

            // A token minted for another flow's nonce is refused.
            idp.forge_nonce("someone-elses-nonce");
            let forged = sign_in(&idp, &http, &env, &config, &flow).await;
            assert!(
                matches!(forged, Err(ProviderError::Exchange(_))),
                "{forged:?}"
            );

            // A token checked after it expired (5 minutes after issue) is refused.
            let url = authorize_url(&http, &config, &flow).await.unwrap();
            let (code, _) = idp.authorize(&url).unwrap();
            let late = env.clock.wall_now().saturating_add_micros(301 * 1_000_000);
            let expired = identify(&http, &config, &flow, &code, late).await;
            assert!(expired.is_err(), "{expired:?}");

            // A wrong client secret fails the exchange.
            let mut wrong = config.clone();
            wrong.client_secret = "nope".to_owned();
            assert!(sign_in(&idp, &http, &env, &wrong, &flow).await.is_err());
        });
    }
}
