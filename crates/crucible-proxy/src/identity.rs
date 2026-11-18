//! Opt-in identity from the bearer token of an HTTP request (contract 9.3).
//!
//! The token is read here, in the forwarding layer, and only the derived fields go on: the analyser never
//! holds the token. Identity is off by default. In `claims` mode the payload is decoded without verification
//! and the row is labelled `token_claims`. In `verified` mode the signature is checked against a key set
//! fetched from a configured JWKS URL, and only a token that verifies and has not expired yields an identity,
//! labelled `token_verified`. A token that is opaque, malformed or fails verification yields no identity; the
//! request is forwarded the same either way.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use crucible_env::{Clock, Env};
use crucible_records::{DetMap, IdentitySource, det_map};
use http::header::ACCEPT;
use http::{HeaderValue, Method, Request};
use http_body_util::Full;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{AlgorithmFamily, DecodingKey, Validation, decode, decode_header};
use serde_json::Value as Json;
use sha2::{Digest as _, Sha256};
use tokio::sync::Notify;

use crate::core::Identity;
use crate::net::{Target, request_once};

/// A longer token is not looked at.
const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// Verified tokens remembered, so a token is verified once.
const CACHE_ENTRIES: usize = 4_096;
/// Clock skew allowed when checking `exp`, in seconds.
const LEEWAY_SECONDS: i64 = 60;
/// The shortest time between two fetches of the key set.
const MIN_REFRESH_GAP: Duration = Duration::from_secs(30);

/// How verified mode finds and checks keys.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VerifyConfig {
    /// The origin of the JWKS URL.
    pub jwks: Target,
    /// When set, the token's `aud` must contain it.
    pub audience: Option<String>,
    /// When set, the token's `iss` must equal it.
    pub issuer: Option<String>,
    /// How often the key set is fetched again.
    pub refresh: Duration,
}

#[derive(Debug)]
enum Mode {
    Off,
    Claims,
    Verified {
        config: VerifyConfig,
        keys: Mutex<Option<JwkSet>>,
        /// Verified identities by token hash, with the token's expiry in Unix seconds.
        cache: Mutex<DetMap<[u8; 32], (Identity, i64)>>,
        refresh: Arc<Notify>,
    },
}

/// Derives an [`Identity`] from an `Authorization` header value.
#[derive(Debug)]
pub struct IdentityResolver {
    mode: Mode,
    clock: Arc<dyn Clock>,
}

fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The identity fields of a claims object.
fn identity_of(claims: &Json, source: IdentitySource) -> Identity {
    let claim = |names: &[&str], max: usize| {
        names
            .iter()
            .find_map(|name| claims.get(name).and_then(Json::as_str))
            .map(|text| cut(text, max))
            .unwrap_or_default()
    };
    Identity {
        source: Some(source),
        issuer: claim(&["iss"], 512),
        subject: claim(&["sub"], 512),
        client_id: claim(&["client_id", "azp"], 256),
        tenant: claim(&["tid", "tenant_id", "tenant"], 256),
    }
}

/// The claims of a JWT, decoded without any verification.
fn unverified_claims(token: &str) -> Option<Json> {
    let mut parts = token.split('.');
    let (_header, payload, _signature) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<Json>(&payload)
        .ok()
        .filter(Json::is_object)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl IdentityResolver {
    /// Identity is off: no identity key is ever sent.
    pub fn off(env: &Env) -> Arc<Self> {
        Arc::new(Self {
            mode: Mode::Off,
            clock: Arc::clone(&env.clock),
        })
    }

    /// Claims are decoded without verification and labelled so.
    pub fn claims(env: &Env) -> Arc<Self> {
        Arc::new(Self {
            mode: Mode::Claims,
            clock: Arc::clone(&env.clock),
        })
    }

    /// Tokens are verified against the key set at `config.jwks`, which a background task fetches now, every
    /// `config.refresh`, and when a token names a key that is not in the set. Until the first fetch succeeds
    /// no token verifies, so requests carry no identity.
    pub fn verified(env: &Env, config: VerifyConfig) -> Arc<Self> {
        let refresh = Arc::new(Notify::new());
        let resolver = Arc::new(Self {
            mode: Mode::Verified {
                config,
                keys: Mutex::new(None),
                cache: Mutex::new(det_map()),
                refresh: Arc::clone(&refresh),
            },
            clock: Arc::clone(&env.clock),
        });
        // Detached: the task runs until the scope shuts down.
        drop(env.spawn(
            "proxy-jwks",
            refresh_keys(env.clone(), Arc::clone(&resolver), refresh),
        ));
        resolver
    }

    /// The identity of a request, from its `Authorization` header. Never fails: anything that is not a usable
    /// token gives the empty identity.
    pub fn resolve(&self, authorization: Option<&HeaderValue>) -> Identity {
        if matches!(self.mode, Mode::Off) {
            return Identity::default();
        }
        let token = authorization
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim())
            .filter(|token| !token.is_empty() && token.len() <= MAX_TOKEN_BYTES);
        let Some(token) = token else {
            return Identity::default();
        };
        match &self.mode {
            Mode::Off => Identity::default(),
            Mode::Claims => unverified_claims(token)
                .map(|claims| identity_of(&claims, IdentitySource::TokenClaims))
                .unwrap_or_default(),
            Mode::Verified {
                config,
                keys,
                cache,
                refresh,
            } => {
                let now = self.clock.wall_now().micros() / 1_000_000;
                let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
                if let Some((identity, expires)) = lock(cache).get(&digest)
                    && *expires + LEEWAY_SECONDS >= now
                {
                    return identity.clone();
                }
                let Some((identity, expires)) = verify(token, config, keys, refresh, now) else {
                    return Identity::default();
                };
                let mut cache = lock(cache);
                if cache.len() >= CACHE_ENTRIES {
                    cache.clear();
                }
                cache.insert(digest, (identity.clone(), expires));
                identity
            }
        }
    }
}

/// Verifies a token's signature, audience, issuer and expiry. Returns its identity and expiry.
fn verify(
    token: &str,
    config: &VerifyConfig,
    keys: &Mutex<Option<JwkSet>>,
    refresh: &Notify,
    now: i64,
) -> Option<(Identity, i64)> {
    let header = decode_header(token).ok()?;
    // Only public-key signatures: a shared secret is not something a key set publishes.
    if header.alg.family() == AlgorithmFamily::Hmac {
        return None;
    }
    let key = {
        let keys = lock(keys);
        let set = keys.as_ref()?;
        let jwk = match &header.kid {
            Some(kid) => set.find(kid),
            None => match set.keys.as_slice() {
                [only] => Some(only),
                _ => None,
            },
        };
        let Some(jwk) = jwk else {
            // The issuer may have rotated its keys.
            refresh.notify_one();
            return None;
        };
        DecodingKey::from_jwk(jwk).ok()?
    };
    let mut validation = Validation::new(header.alg);
    // Expiry is checked below against the `Env` clock, not the ambient one.
    validation.validate_exp = false;
    validation.required_spec_claims.clear();
    match &config.audience {
        Some(audience) => {
            validation.set_audience(&[audience]);
            validation.required_spec_claims.insert("aud".to_owned());
        }
        None => validation.validate_aud = false,
    }
    if let Some(issuer) = &config.issuer {
        validation.set_issuer(&[issuer]);
        validation.required_spec_claims.insert("iss".to_owned());
    }
    let claims = decode::<Json>(token, &key, &validation).ok()?.claims;
    let expires = claims.get("exp").and_then(Json::as_f64)? as i64;
    if expires + LEEWAY_SECONDS < now {
        return None;
    }
    Some((identity_of(&claims, IdentitySource::TokenVerified), expires))
}

async fn fetch_keys(env: &Env, config: &VerifyConfig) -> Result<JwkSet, String> {
    let path = if config.jwks.path.is_empty() {
        "/"
    } else {
        &config.jwks.path
    };
    let request = Request::builder()
        .method(Method::GET)
        .uri(path)
        .header(ACCEPT, "application/json")
        .body(Full::new(Bytes::new()))
        .map_err(|e| e.to_string())?;
    let response = request_once(env, &config.jwks, request, 1 << 20, Duration::from_secs(10))
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("the key set answered {}", response.status()));
    }
    serde_json::from_slice(response.body()).map_err(|e| format!("the key set is not a JWKS: {e}"))
}

async fn refresh_keys(env: Env, resolver: Arc<IdentityResolver>, refresh: Arc<Notify>) {
    let Mode::Verified { config, keys, .. } = &resolver.mode else {
        return;
    };
    loop {
        let wait = match fetch_keys(&env, config).await {
            Ok(set) => {
                *lock(keys) = Some(set);
                config.refresh
            }
            Err(error) => {
                tracing::warn!(%error, "fetching the JWKS failed; tokens cannot be verified until it works");
                MIN_REFRESH_GAP
            }
        };
        env.clock.sleep(MIN_REFRESH_GAP).await;
        let rest = env.clock.sleep(wait.saturating_sub(MIN_REFRESH_GAP));
        let asked = std::pin::pin!(refresh.notified());
        futures::future::select(asked, rest).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: &str) -> String {
        format!(
            "{}.{}.c2ln",
            URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims)
        )
    }

    #[test]
    fn claims_are_read_from_a_jwt_and_nothing_else() {
        let claims = unverified_claims(&token(
            r#"{"iss":"https://issuer.example","sub":"user-1","azp":"client-9","tid":"acme","exp":1}"#,
        ))
        .unwrap();
        let identity = identity_of(&claims, IdentitySource::TokenClaims);
        assert_eq!(identity.source, Some(IdentitySource::TokenClaims));
        assert_eq!(identity.issuer, "https://issuer.example");
        assert_eq!(identity.subject, "user-1");
        assert_eq!(identity.client_id, "client-9");
        assert_eq!(identity.tenant, "acme");
        for opaque in ["opaque-token", "a.b", "a.b.c.d", "a.!!!.c", &token("[1]")] {
            assert_eq!(unverified_claims(opaque), None, "{opaque}");
        }
        let long = "é".repeat(600);
        assert_eq!(cut(&long, 512).len(), 512);
    }
}
