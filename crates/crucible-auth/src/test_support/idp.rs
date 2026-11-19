//! A fake sign-in provider for tests, served over an in-memory [`Net`]: an OpenID Connect issuer at
//! `http://idp.test` (discovery, an empty key set, a token endpoint that signs ID tokens with HS256 under the
//! client secret) and GitHub's endpoints at `github.com` and `api.github.com` (whose TLS handshake the fake
//! network skips). [`FakeIdp::authorize`] plays the browser at the authorization endpoint.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use crucible_env::{Env, Instant, Listener, Net, NetError, Stream, TlsRoots};
use hmac::{Hmac, KeyInit as _, Mac as _};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The issuer of the fake OpenID Connect provider.
pub const ISSUER: &str = "http://idp.test";
/// The client id the fake providers accept.
pub const CLIENT_ID: &str = "crucible-test";
/// The client secret the fake providers accept.
pub const CLIENT_SECRET: &str = "fake-client-secret-0123456789";

/// The person the fake providers sign in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FakeUser {
    /// The provider's subject.
    pub subject: String,
    /// The email.
    pub email: String,
    /// Whether the provider says the email is verified.
    pub email_verified: bool,
    /// The name.
    pub name: String,
}

#[derive(Debug)]
struct Grant {
    challenge: String,
    nonce: Option<String>,
    redirect_uri: String,
    user: FakeUser,
}

#[derive(Debug)]
struct State {
    user: FakeUser,
    codes: BTreeMap<String, Grant>,
    tokens: BTreeMap<String, FakeUser>,
    issued: u64,
    /// Overrides the `nonce` of the next ID token.
    forged_nonce: Option<String>,
}

/// The fake providers.
#[derive(Clone, Debug)]
pub struct FakeIdp {
    env: Env,
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The query parameters of a URL.
fn query(url: &str) -> BTreeMap<String, String> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    form(query)
}

/// A form body or a query string.
fn form(text: &str) -> BTreeMap<String, String> {
    text.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

/// Percent-decoding, with `+` as a space.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let byte = bytes
                    .get(i + 1..i + 3)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match byte {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl FakeIdp {
    /// The providers, signing `user` in until [`FakeIdp::set_user`] changes it. `env` runs the connections.
    pub fn new(env: &Env, user: FakeUser) -> Self {
        Self {
            env: env.clone(),
            state: Arc::new(Mutex::new(State {
                user,
                codes: BTreeMap::new(),
                tokens: BTreeMap::new(),
                issued: 0,
                forged_nonce: None,
            })),
        }
    }

    /// Who signs in next.
    pub fn set_user(&self, user: FakeUser) {
        lock(&self.state).user = user;
    }

    /// Makes the next ID token carry `nonce` instead of the one the flow asked for.
    pub fn forge_nonce(&self, nonce: &str) {
        lock(&self.state).forged_nonce = Some(nonce.to_owned());
    }

    /// The `Env` of `env` with this fake's network.
    pub fn env(&self, env: &Env) -> Env {
        Env::new(
            Arc::clone(&env.clock),
            Arc::clone(&env.rng),
            Arc::clone(&env.spawner),
            Arc::clone(&env.disk),
            Arc::new(FakeNet { idp: self.clone() }),
            Arc::clone(&env.metrics),
        )
    }

    /// Plays the browser at the authorization endpoint: checks the request and answers the callback's
    /// `(code, state)`.
    pub fn authorize(&self, url: &str) -> Result<(String, String), String> {
        let params = query(url);
        let get = |name: &str| {
            params
                .get(name)
                .cloned()
                .ok_or_else(|| format!("the authorization request has no {name}"))
        };
        if get("client_id")? != CLIENT_ID || get("response_type")? != "code" {
            return Err("unknown client or response type".to_owned());
        }
        if get("code_challenge_method")? != "S256" {
            return Err("PKCE must be S256".to_owned());
        }
        let oidc = url.starts_with(ISSUER);
        let nonce = if oidc { Some(get("nonce")?) } else { None };
        let mut state = lock(&self.state);
        state.issued += 1;
        let code = format!("code-{}", state.issued);
        let grant = Grant {
            challenge: get("code_challenge")?,
            nonce,
            redirect_uri: get("redirect_uri")?,
            user: state.user.clone(),
        };
        state.codes.insert(code.clone(), grant);
        Ok((code, get("state")?))
    }

    fn id_token(&self, grant: &Grant, nonce: Option<String>) -> String {
        let now = self.env.clock.wall_now().micros() / 1_000_000;
        let claims = json!({
            "iss": ISSUER,
            "sub": grant.user.subject,
            "aud": CLIENT_ID,
            "iat": now,
            "exp": now + 300,
            "nonce": nonce,
            "email": grant.user.email,
            "email_verified": grant.user.email_verified,
            "name": grant.user.name,
        });
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{body}");
        let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(CLIENT_SECRET.as_bytes())
            .unwrap_or_else(|_| Hmac::<Sha256>::new(&[0; 64].into()));
        mac.update(signing_input.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{signing_input}.{signature}")
    }

    fn token(&self, authorization: Option<&str>, body: &str) -> (StatusCode, Value) {
        let params = form(body);
        let basic = format!(
            "Basic {}",
            STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
        );
        let by_body = params.get("client_id").map(String::as_str) == Some(CLIENT_ID)
            && params.get("client_secret").map(String::as_str) == Some(CLIENT_SECRET);
        if authorization != Some(basic.as_str()) && !by_body {
            return (StatusCode::UNAUTHORIZED, json!({"error": "invalid_client"}));
        }
        let mut state = lock(&self.state);
        let Some(grant) = params.get("code").and_then(|code| state.codes.remove(code)) else {
            return (StatusCode::BAD_REQUEST, json!({"error": "invalid_grant"}));
        };
        let verifier = params.get("code_verifier").cloned().unwrap_or_default();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        if challenge != grant.challenge || params.get("redirect_uri") != Some(&grant.redirect_uri) {
            return (StatusCode::BAD_REQUEST, json!({"error": "invalid_grant"}));
        }
        state.issued += 1;
        let access = format!("access-{}", state.issued);
        state.tokens.insert(access.clone(), grant.user.clone());
        let forged = state.forged_nonce.take();
        drop(state);
        let mut response = json!({
            "access_token": access,
            "token_type": "bearer",
            "expires_in": 3600,
        });
        if grant.nonce.is_some() {
            let nonce = forged.or_else(|| grant.nonce.clone());
            response["id_token"] = Value::String(self.id_token(&grant, nonce));
        }
        (StatusCode::OK, response)
    }

    fn github_api(&self, authorization: Option<&str>, path: &str) -> (StatusCode, Value) {
        let user = authorization
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| lock(&self.state).tokens.get(token).cloned());
        let Some(user) = user else {
            return (
                StatusCode::UNAUTHORIZED,
                json!({"message": "Bad credentials"}),
            );
        };
        match path {
            "/user" => (
                StatusCode::OK,
                json!({"id": user.subject.parse::<u64>().unwrap_or(1), "login": "octo", "name": user.name}),
            ),
            "/user/emails" => (
                StatusCode::OK,
                json!([
                    {"email": "noreply@users.example", "primary": false, "verified": true},
                    {"email": user.email, "primary": true, "verified": user.email_verified},
                ]),
            ),
            _ => (StatusCode::NOT_FOUND, json!({"message": "Not Found"})),
        }
    }

    async fn handle(self, request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
        let host = request
            .headers()
            .get(http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let path = request.uri().path().to_owned();
        let authorization = request
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .map(str::to_owned);
        let body = request
            .into_body()
            .collect()
            .await
            .map(|b| String::from_utf8_lossy(&b.to_bytes()).into_owned())
            .unwrap_or_default();
        let (status, value) = match (host.as_str(), path.as_str()) {
            ("idp.test", "/.well-known/openid-configuration") => (
                StatusCode::OK,
                json!({
                    "issuer": ISSUER,
                    "authorization_endpoint": format!("{ISSUER}/authorize"),
                    "token_endpoint": format!("{ISSUER}/token"),
                    "jwks_uri": format!("{ISSUER}/jwks"),
                    "response_types_supported": ["code"],
                    "subject_types_supported": ["public"],
                    "id_token_signing_alg_values_supported": ["HS256"],
                }),
            ),
            ("idp.test", "/jwks") => (StatusCode::OK, json!({"keys": []})),
            ("idp.test", "/token") | ("github.com", "/login/oauth/access_token") => {
                self.token(authorization.as_deref(), &body)
            }
            ("api.github.com", path) => self.github_api(authorization.as_deref(), path),
            _ => (StatusCode::NOT_FOUND, json!({"error": "not_found"})),
        };
        let mut response = Response::new(Full::new(Bytes::from(value.to_string())));
        *response.status_mut() = status;
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Ok(response)
    }
}

/// The hosts of the fake network, by address.
const HOSTS: [(&str, Ipv4Addr); 3] = [
    ("idp.test", Ipv4Addr::new(10, 9, 0, 1)),
    ("github.com", Ipv4Addr::new(10, 9, 0, 2)),
    ("api.github.com", Ipv4Addr::new(10, 9, 0, 3)),
];

/// A network that reaches only the fake providers.
#[derive(Debug)]
struct FakeNet {
    idp: FakeIdp,
}

fn refused(what: &str) -> NetError {
    NetError::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        what.to_owned(),
    ))
}

#[async_trait]
impl Net for FakeNet {
    async fn listen(&self, _addr: SocketAddr) -> Result<Box<dyn Listener>, NetError> {
        Err(refused("the fake network does not listen"))
    }

    async fn connect(&self, addr: SocketAddr, _deadline: Instant) -> Result<Stream, NetError> {
        if !HOSTS.iter().any(|(_, ip)| IpAddr::V4(*ip) == addr.ip()) {
            return Err(refused("no fake host at this address"));
        }
        let (client, server) = tokio::io::duplex(1 << 16);
        let idp = self.idp.clone();
        self.idp.env.spawn("fake-idp", async move {
            let service = hyper::service::service_fn(move |request| idp.clone().handle(request));
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(server), service)
                .await;
        });
        Ok(Stream::new(client))
    }

    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        HOSTS
            .iter()
            .find(|(name, _)| *name == host)
            .map(|(_, ip)| vec![IpAddr::V4(*ip)])
            .ok_or_else(|| refused("no fake host of this name"))
    }

    async fn tls_connect(
        &self,
        stream: Stream,
        _server_name: &str,
        _roots: &TlsRoots,
    ) -> Result<Stream, NetError> {
        Ok(stream)
    }
}
