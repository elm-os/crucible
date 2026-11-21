//! The outbound call (contract 12.6): one HTTP/1.1 POST per connection, through `Net`.
//!
//! The host name is resolved once, every address it resolves to is checked against the policy, and the
//! connection goes to the checked address literal, with the name kept only for TLS and the `Host` header. A
//! resolver that answers differently a moment later therefore cannot move the call. A redirect is an answer
//! like any other: its status is returned and its `Location` is never followed.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{ClockExt as _, Env, Instant, NetError, TlsRoots};
use http::header::{CONTENT_LENGTH, HOST, RETRY_AFTER, USER_AGENT};
use http::{HeaderName, HeaderValue, Method, Request};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;

use crate::policy::{EgressPolicy, Mode, Refusal, Target};

/// How long resolving, connecting and the TLS handshake may take together.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the exchange may take once connected.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of the answer's body is read before the connection is dropped. The body is never kept.
pub const BODY_LIMIT: usize = 64 << 10;
const AGENT: &str = concat!("crucible/", env!("CARGO_PKG_VERSION"));

/// Why a call did not get an answer.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum EgressError {
    /// The policy refuses the target.
    #[error("{0}")]
    Refused(Refusal),
    /// The host name did not resolve.
    #[error("the host name did not resolve: {0}")]
    Resolve(String),
    /// The connection could not be made.
    #[error("the connection failed: {0}")]
    Connect(String),
    /// The TLS handshake failed.
    #[error("the TLS handshake failed: {0}")]
    Tls(String),
    /// A time limit passed: `connect` (5 s) or `response` (10 s).
    #[error("no answer within the {0} time limit")]
    Timeout(&'static str),
    /// The peer did not speak HTTP, or a header of the request is not valid.
    #[error("the exchange failed: {0}")]
    Protocol(String),
}

impl EgressError {
    /// The stable code, for `error_code` fields. It never carries text from the peer.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Refused(refusal) => refusal.reason(),
            Self::Resolve(_) => "dns_failed",
            Self::Connect(_) => "connect_failed",
            Self::Tls(_) => "tls_failed",
            Self::Timeout(_) => "timeout",
            Self::Protocol(_) => "protocol_error",
        }
    }
}

impl From<Refusal> for EgressError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// The answer to a call: its status and nothing of its body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reply {
    /// The HTTP status. A redirect status is returned as it is.
    pub status: u16,
    /// The seconds of a `Retry-After` header, when it is a number.
    pub retry_after_secs: Option<u64>,
}

/// Outbound calls under the deployment's egress policy.
#[derive(Clone, Debug)]
pub struct Egress {
    env: Env,
    policy: EgressPolicy,
    roots: TlsRoots,
}

impl Egress {
    /// Calls through `env.net` under `policy`, verifying TLS peers against `roots`.
    pub fn new(env: &Env, policy: EgressPolicy, roots: TlsRoots) -> Self {
        Self {
            env: env.clone(),
            policy,
            roots,
        }
    }

    /// The policy.
    pub const fn policy(&self) -> &EgressPolicy {
        &self.policy
    }

    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, EgressError> {
        match host.parse::<IpAddr>() {
            Ok(ip) => Ok(vec![ip]),
            Err(_) => self
                .env
                .net
                .resolve(host)
                .await
                .map_err(|error| EgressError::Resolve(error.to_string())),
        }
    }

    /// The address to connect to: the first one the host resolves to, once every one of them has passed the
    /// policy. One refused address refuses the target, so a name cannot hide a refused address among others.
    pub async fn vet(&self, target: &Target) -> Result<SocketAddr, EgressError> {
        let addresses = self.resolve(&target.host).await?;
        let first = *addresses.first().ok_or(Refusal::NoAddress)?;
        for ip in &addresses {
            self.policy.check_addr(*ip)?;
        }
        if self.policy.mode == Mode::Strict {
            for own in &self.policy.infrastructure {
                // An infrastructure host that does not resolve here cannot be the target either.
                let theirs = self.resolve(own).await.unwrap_or_default();
                if theirs.iter().any(|ip| addresses.contains(ip)) {
                    return Err(Refusal::Infrastructure(target.host.clone()).into());
                }
            }
        }
        Ok(SocketAddr::new(first, target.port))
    }

    /// POSTs `body` to `url` with `headers`, and returns the status of the answer. The body of the answer is
    /// read up to [`BODY_LIMIT`] and dropped.
    pub async fn post(
        &self,
        url: &str,
        headers: &[(HeaderName, String)],
        body: Bytes,
    ) -> Result<Reply, EgressError> {
        let target = self.policy.check_url(url)?;
        let deadline = self.env.clock.now().saturating_add(CONNECT_TIMEOUT);
        let stream = match self
            .env
            .clock
            .timeout_at(deadline, self.connect(&target, deadline))
            .await
        {
            Ok(stream) => stream?,
            Err(_) => return Err(EgressError::Timeout("connect")),
        };
        match self
            .env
            .timeout(
                RESPONSE_TIMEOUT,
                self.exchange(&target, stream, headers, body),
            )
            .await
        {
            Ok(reply) => reply,
            Err(_) => Err(EgressError::Timeout("response")),
        }
    }

    async fn connect(
        &self,
        target: &Target,
        deadline: Instant,
    ) -> Result<crucible_env::Stream, EgressError> {
        let addr = self.vet(target).await?;
        let stream = self
            .env
            .net
            .connect(addr, deadline)
            .await
            .map_err(|error| match error {
                NetError::DeadlineElapsed => EgressError::Timeout("connect"),
                other => EgressError::Connect(other.to_string()),
            })?;
        if !target.https {
            return Ok(stream);
        }
        self.env
            .net
            .tls_connect(stream, &target.host, &self.roots)
            .await
            .map_err(|error| EgressError::Tls(error.to_string()))
    }

    async fn exchange(
        &self,
        target: &Target,
        stream: crucible_env::Stream,
        headers: &[(HeaderName, String)],
        body: Bytes,
    ) -> Result<Reply, EgressError> {
        let protocol = |error: &dyn std::fmt::Display| EgressError::Protocol(error.to_string());
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(target.path.as_str())
            .header(HOST, target.authority.as_str())
            .header(USER_AGENT, AGENT)
            .header(CONTENT_LENGTH, body.len());
        for (name, value) in headers {
            let value = HeaderValue::from_str(value).map_err(|e| protocol(&e))?;
            request = request.header(name, value);
        }
        let request = request.body(Full::new(body)).map_err(|e| protocol(&e))?;
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .map_err(|e| protocol(&e))?;
        let driver = self.env.spawn("egress-http", async move {
            // The connection ends with the exchange; its error, if any, is the request's.
            let _ = connection.await;
        });
        let reply = match sender.send_request(request).await {
            Ok(response) => {
                let (parts, body) = response.into_parts();
                // Read and dropped: enough for the peer to finish a small answer, never kept or shown.
                let _ = Limited::new(body, BODY_LIMIT).collect().await;
                Ok(Reply {
                    status: parts.status.as_u16(),
                    retry_after_secs: parts
                        .headers
                        .get(RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|text| text.trim().parse().ok()),
                })
            }
            Err(error) => Err(protocol(&error)),
        };
        driver.abort();
        reply
    }
}
