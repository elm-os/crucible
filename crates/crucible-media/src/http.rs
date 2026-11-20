//! One HTTP/1.1 exchange per connection with the object store's internal endpoint, through `Net`.
//!
//! The host is resolved through `Net`, the connection made to the first address, and TLS run with the
//! deployment's roots when the endpoint is `https`. The request carries a presigned URL, so it needs no
//! `Authorization` header.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{ClockExt as _, Env, Instant, NetError, ObjectError, Stream, TlsRoots};
use http::header::{CONTENT_LENGTH, HOST};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use url::{Host, Position, Url};

/// How long resolving, connecting and the TLS handshake may take together.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the store may take to answer a request once connected.
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// The most bytes of an answer that is read whole: listings, batch deletes and error documents.
pub(crate) const ANSWER_LIMIT: usize = 8 << 20;

/// The head of an answer and its body, still on the wire.
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Incoming,
}

impl Answer {
    /// The whole body, up to [`ANSWER_LIMIT`].
    pub(crate) async fn bytes(self, env: &Env) -> Result<Bytes, ObjectError> {
        match env
            .timeout(
                RESPONSE_TIMEOUT,
                Limited::new(self.body, ANSWER_LIMIT).collect(),
            )
            .await
        {
            Ok(Ok(collected)) => Ok(collected.to_bytes()),
            Ok(Err(error)) => Err(unavailable(format!("reading the answer failed: {error}"))),
            Err(_) => Err(unavailable("the answer did not arrive within 30 s")),
        }
    }

    /// The error an answer that is not a success stands for: [`ObjectError::NotFound`] for 404, unavailable
    /// for a server error or a request to slow down, and otherwise the refusal with the store's error code.
    pub(crate) async fn failure(self, env: &Env) -> ObjectError {
        let status = self.status;
        if status == StatusCode::NOT_FOUND {
            return ObjectError::NotFound;
        }
        let body = self.bytes(env).await.unwrap_or_default();
        let code = error_code(&body).unwrap_or_default();
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || code == "SlowDown"
        {
            return unavailable(format!("the store answered {status} {code}"));
        }
        ObjectError::Rejected {
            status: status.as_u16(),
            code,
        }
    }
}

pub(crate) fn unavailable(why: impl Into<String>) -> ObjectError {
    ObjectError::Unavailable(why.into())
}

/// The `<Code>` of an S3 error document.
pub(crate) fn error_code(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    let start = text.find("<Code>")? + "<Code>".len();
    let end = text[start..].find("</Code>")? + start;
    Some(text[start..end].trim().to_owned())
}

async fn connect(
    env: &Env,
    roots: &TlsRoots,
    url: &Url,
    deadline: Instant,
) -> Result<Stream, ObjectError> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ObjectError::Invalid(format!("{url} has no port")))?;
    let (ip, name) = match url.host() {
        Some(Host::Ipv4(ip)) => (IpAddr::V4(ip), ip.to_string()),
        Some(Host::Ipv6(ip)) => (IpAddr::V6(ip), ip.to_string()),
        Some(Host::Domain(domain)) => {
            let addresses = env
                .net
                .resolve(domain)
                .await
                .map_err(|error| unavailable(format!("{domain} did not resolve: {error}")))?;
            let first = addresses
                .first()
                .copied()
                .ok_or_else(|| unavailable(format!("{domain} resolved to no address")))?;
            (first, domain.to_owned())
        }
        None => return Err(ObjectError::Invalid(format!("{url} has no host"))),
    };
    let stream = env
        .net
        .connect(SocketAddr::new(ip, port), deadline)
        .await
        .map_err(|error| match error {
            NetError::DeadlineElapsed => unavailable("no connection within 5 s"),
            other => unavailable(format!("the connection to {name}:{port} failed: {other}")),
        })?;
    if url.scheme() != "https" {
        return Ok(stream);
    }
    env.net
        .tls_connect(stream, &name, roots)
        .await
        .map_err(|error| unavailable(format!("the TLS handshake with {name} failed: {error}")))
}

/// Sends one request to `url` with `headers` and `body`, and returns the answer's head with its body still to
/// read. The connection closes once the body is read or dropped.
pub(crate) async fn send(
    env: &Env,
    roots: &TlsRoots,
    method: Method,
    url: &Url,
    headers: &[(HeaderName, HeaderValue)],
    body: Bytes,
) -> Result<Answer, ObjectError> {
    let deadline = env.clock.now().saturating_add(CONNECT_TIMEOUT);
    let stream = match env
        .clock
        .timeout_at(deadline, connect(env, roots, url, deadline))
        .await
    {
        Ok(stream) => stream?,
        Err(_) => return Err(unavailable("no connection within 5 s")),
    };
    let invalid = |error: &dyn std::fmt::Display| ObjectError::Invalid(error.to_string());
    let len = body.len();
    let mut request = Request::builder()
        .method(method)
        .uri(&url[Position::BeforePath..])
        .body(Full::new(body))
        .map_err(|e| invalid(&e))?;
    let host = HeaderValue::from_str(&url[Position::BeforeHost..Position::AfterPort])
        .map_err(|e| invalid(&e))?;
    let fields = request.headers_mut();
    fields.insert(HOST, host);
    fields.insert(CONTENT_LENGTH, HeaderValue::from(len));
    // A signed header replaces the default of the same name: a field sent twice is a malformed request.
    for (name, value) in headers {
        fields.insert(name, value.clone());
    }
    let exchange = async move {
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .map_err(|error| unavailable(format!("the HTTP handshake failed: {error}")))?;
        // The connection's driver ends once the answer's body is read or dropped, as the sender is gone by then.
        let _driver = env.spawn("object-store-http", async move {
            let _ = connection.await;
        });
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| unavailable(format!("the request failed: {error}")))?;
        let (parts, body) = response.into_parts();
        Ok(Answer {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    };
    match env.timeout(RESPONSE_TIMEOUT, exchange).await {
        Ok(answer) => answer,
        Err(_) => Err(unavailable("the store did not answer within 30 s")),
    }
}

/// One request to `url` through `Net`, its answer read whole: how a test plays a client of presigned URLs.
#[cfg(feature = "test-support")]
pub async fn fetch(
    env: &Env,
    roots: &TlsRoots,
    method: Method,
    url: &str,
    headers: &[(String, String)],
    body: Bytes,
) -> Result<(StatusCode, HeaderMap, Bytes), ObjectError> {
    let url = Url::parse(url).map_err(|error| ObjectError::Invalid(error.to_string()))?;
    let invalid = |error: &dyn std::fmt::Display| ObjectError::Invalid(error.to_string());
    let mut typed = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        typed.push((
            HeaderName::from_bytes(name.as_bytes()).map_err(|e| invalid(&e))?,
            HeaderValue::from_str(value).map_err(|e| invalid(&e))?,
        ));
    }
    let answer = send(env, roots, method, &url, &typed, body).await?;
    let (status, head) = (answer.status, answer.headers.clone());
    Ok((status, head, answer.bytes(env).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_read_from_s3_error_documents() {
        let body = b"<?xml version=\"1.0\"?><Error><Code>SignatureDoesNotMatch</Code><Message>no</Message></Error>";
        assert_eq!(error_code(body).as_deref(), Some("SignatureDoesNotMatch"));
        assert_eq!(error_code(b"<Error></Error>"), None);
        assert_eq!(error_code(b"\xff\xfe"), None);
    }
}
