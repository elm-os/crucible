//! Outbound connections over `Env::net`: URL parsing, name resolution, TCP, and TLS for `https`.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Env, NetError, Stream, TlsRoots};
use http::header::{HOST, HeaderValue};
use http::{Request, Response, Uri};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;

/// An `http` or `https` origin with an optional path prefix.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Target {
    /// Whether the scheme is `https`.
    pub tls: bool,
    /// The host: a name or an IP literal (IPv6 without brackets).
    pub host: String,
    /// The port, defaulted from the scheme.
    pub port: u16,
    /// The path of the URL without a trailing slash; empty for the root.
    pub path: String,
}

impl Target {
    /// Parses `http://host[:port][/path]` or the `https` form. The message of a refusal says what is expected.
    pub fn parse(url: &str) -> Result<Self, String> {
        let expected =
            "expected a URL such as http://127.0.0.1:8080 or https://crucible.example.com";
        let uri: Uri = url
            .parse()
            .map_err(|_| format!("{url:?} is not a URL; {expected}"))?;
        let tls = match uri.scheme_str() {
            Some("http") => false,
            Some("https") => true,
            _ => return Err(format!("{url:?} has no http or https scheme; {expected}")),
        };
        let host = uri
            .host()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| format!("{url:?} has no host; {expected}"))?;
        if uri.query().is_some() {
            return Err(format!(
                "{url:?} has a query string; give the URL without one"
            ));
        }
        Ok(Self {
            tls,
            host: host.trim_matches(['[', ']']).to_owned(),
            port: uri.port_u16().unwrap_or(if tls { 443 } else { 80 }),
            path: uri.path().trim_end_matches('/').to_owned(),
        })
    }

    /// The `Host` header value for this origin.
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let default_port = if self.tls { 443 } else { 80 };
        if self.port == default_port {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// Connects to `target` within `timeout`: resolves the name unless the host is an address, tries each address
/// in order, and runs the TLS handshake for `https` against the bundled root program.
pub async fn connect(env: &Env, target: &Target, timeout: Duration) -> Result<Stream, NetError> {
    let deadline = env.clock.now().saturating_add(timeout);
    let addresses = match target.host.parse::<IpAddr>() {
        Ok(address) => vec![address],
        Err(_) => env.net.resolve(&target.host).await?,
    };
    let mut last = NetError::Io(std::io::Error::other(format!(
        "{} resolved to no address",
        target.host
    )));
    for address in addresses {
        match env
            .net
            .connect(SocketAddr::new(address, target.port), deadline)
            .await
        {
            Ok(stream) if target.tls => {
                return env
                    .net
                    .tls_connect(stream, &target.host, &TlsRoots::WebPki)
                    .await;
            }
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Why a one-shot request failed.
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// The connection could not be opened.
    #[error("connecting: {0}")]
    Connect(#[from] NetError),
    /// The HTTP exchange failed.
    #[error("http: {0}")]
    Http(#[from] hyper::Error),
    /// The response body was larger than the caller allows.
    #[error("the response body is larger than {0} bytes")]
    TooLarge(usize),
    /// The deadline passed.
    #[error("no answer within {0:?}")]
    Timeout(Duration),
}

/// Sends one request on a new connection and reads the whole response, at most `max_body` bytes, all within
/// `timeout`. The request's URI must be in origin form; the `Host` header is set from `target`.
pub async fn request_once(
    env: &Env,
    target: &Target,
    mut request: Request<Full<Bytes>>,
    max_body: usize,
    timeout: Duration,
) -> Result<Response<Bytes>, RequestError> {
    if let Ok(host) = HeaderValue::from_str(&target.authority()) {
        request.headers_mut().insert(HOST, host);
    }
    let exchange = async {
        let stream = connect(env, target, timeout).await?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream)).await?;
        let response = async {
            let response: Response<Incoming> = sender.send_request(request).await?;
            let (parts, body) = response.into_parts();
            let body = Limited::new(body, max_body)
                .collect()
                .await
                .map_err(|error| match error.downcast::<hyper::Error>() {
                    Ok(http) => RequestError::Http(*http),
                    Err(_) => RequestError::TooLarge(max_body),
                })?
                .to_bytes();
            Ok::<_, RequestError>(Response::from_parts(parts, body))
        };
        // The connection is driven here, next to the exchange, so nothing is spawned.
        let response = std::pin::pin!(response);
        let connection = std::pin::pin!(connection);
        match futures::future::select(response, connection).await {
            futures::future::Either::Left((response, _)) => response,
            futures::future::Either::Right((_closed, response)) => response.await,
        }
    };
    match env.timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_elapsed) => Err(RequestError::Timeout(timeout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_into_targets() {
        let target = Target::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            (
                target.tls,
                target.host.as_str(),
                target.port,
                target.path.as_str()
            ),
            (false, "127.0.0.1", 8080, "")
        );
        assert_eq!(target.authority(), "127.0.0.1:8080");
        let target = Target::parse("https://crucible.example.com/base/").unwrap();
        assert_eq!(
            (
                target.tls,
                target.host.as_str(),
                target.port,
                target.path.as_str()
            ),
            (true, "crucible.example.com", 443, "/base")
        );
        assert_eq!(target.authority(), "crucible.example.com");
        let target = Target::parse("http://[::1]:9/").unwrap();
        assert_eq!((target.host.as_str(), target.port), ("::1", 9));
        assert_eq!(target.authority(), "[::1]:9");
        for bad in [
            "localhost:8080",
            "ftp://host",
            "http://",
            "http://host/?q=1",
            "not a url",
        ] {
            let error = Target::parse(bad).unwrap_err();
            assert!(error.contains("URL"), "{bad}: {error}");
        }
    }
}
