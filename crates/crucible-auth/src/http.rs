//! Outbound HTTP for sign-in providers (contract 2.5): one HTTP/1.1 request per connection, through `Net`
//! (`resolve`, `connect` and, for `https`, `tls_connect`), with a deadline on the `Env` clock, no redirects and a
//! cap on the response body. Sign-in calls are rare, so connections are not pooled.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Env, TlsRoots};
use http::header::{HOST, USER_AGENT};
use http::{HeaderValue, Request, Response, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::rt::TokioIo;

/// How long one request may take, connection included.
const DEADLINE: Duration = Duration::from_secs(10);
/// The largest response body read.
const BODY_LIMIT: usize = 1 << 20;
/// The `User-Agent` of every request (GitHub's API requires one).
const AGENT: &str = concat!("crucible/", env!("CARGO_PKG_VERSION"));

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(pub String);

/// An HTTP client over the `Env`'s network.
#[derive(Clone, Debug)]
pub struct NetHttp {
    env: Env,
    roots: TlsRoots,
}

impl NetHttp {
    /// A client that verifies TLS peers against `roots`.
    pub fn new(env: &Env, roots: TlsRoots) -> Self {
        Self {
            env: env.clone(),
            roots,
        }
    }

    /// Sends `request` and reads the whole response.
    pub async fn send(&self, request: Request<Vec<u8>>) -> Result<Response<Vec<u8>>, HttpError> {
        match self.env.timeout(DEADLINE, self.exchange(request)).await {
            Ok(result) => result,
            Err(_) => Err(HttpError(format!(
                "no response within {} s",
                DEADLINE.as_secs()
            ))),
        }
    }

    async fn exchange(&self, request: Request<Vec<u8>>) -> Result<Response<Vec<u8>>, HttpError> {
        let fail =
            |what: &str, error: &dyn std::fmt::Display| HttpError(format!("{what}: {error}"));
        let uri = request.uri().clone();
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            _ => return Err(HttpError(format!("{uri} is not an http or https URL"))),
        };
        let host = uri
            .host()
            .ok_or_else(|| HttpError(format!("{uri} has no host")))?;
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let ip = match bare.parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => *self
                .env
                .net
                .resolve(host)
                .await
                .map_err(|e| fail("resolving the host", &e))?
                .first()
                .ok_or_else(|| HttpError(format!("{host} has no address")))?,
        };
        let deadline = self.env.clock.now().saturating_add(DEADLINE);
        let mut stream = self
            .env
            .net
            .connect(SocketAddr::new(ip, port), deadline)
            .await
            .map_err(|e| fail("connecting", &e))?;
        if https {
            stream = self
                .env
                .net
                .tls_connect(stream, bare, &self.roots)
                .await
                .map_err(|e| fail("the TLS handshake", &e))?;
        }
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .map_err(|e| fail("the HTTP handshake", &e))?;
        let driver = self.env.spawn("oauth-http", async move {
            // The connection ends with the exchange; its error, if any, is the request's.
            let _ = connection.await;
        });
        let (mut parts, body) = request.into_parts();
        let authority = uri
            .authority()
            .map(|a| a.as_str().to_owned())
            .unwrap_or_default();
        parts.uri = uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .parse::<Uri>()
            .map_err(|e| fail("the request path", &e))?;
        if let Ok(value) = HeaderValue::from_str(&authority) {
            parts.headers.insert(HOST, value);
        }
        parts
            .headers
            .insert(USER_AGENT, HeaderValue::from_static(AGENT));
        let response = sender
            .send_request(Request::from_parts(parts, Full::new(Bytes::from(body))))
            .await
            .map_err(|e| fail("sending the request", &e));
        let result = match response {
            Ok(response) => {
                let (parts, body) = response.into_parts();
                match Limited::new(body, BODY_LIMIT).collect().await {
                    Ok(collected) => Ok(Response::from_parts(parts, collected.to_bytes().to_vec())),
                    Err(e) => Err(HttpError(format!("reading the response: {e}"))),
                }
            }
            Err(e) => Err(e),
        };
        driver.abort();
        result
    }
}

/// The client of the OAuth and OpenID Connect crates.
impl<'c> oauth2::AsyncHttpClient<'c> for NetHttp {
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Vec<u8>>, HttpError>> + Send + 'c>>;

    fn call(&'c self, request: Request<Vec<u8>>) -> Self::Future {
        Box::pin(self.send(request))
    }
}
