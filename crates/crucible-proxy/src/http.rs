//! `crucible proxy http --listen <addr> --upstream <url>`: a reverse proxy for Streamable HTTP MCP servers of
//! both eras.
//!
//! Every request is sent upstream with its method, path, query and headers as they came, `Authorization`
//! included, and the response comes back with its status and headers as the upstream sent them. Bodies are
//! streamed frame by frame in both directions, so an SSE response reaches the client event by event, with its
//! keep-alive comments, and the proxy adds no timeout of its own beyond the connect timeout. Body bytes are
//! identical; HTTP/1.1 framing (chunk boundaries) is the transport's and may differ.
//!
//! Each client connection has its own upstream connection, so closing the client connection closes the
//! upstream one and with it any request in flight. The analyser sees a copy of the bodies and an allowlisted
//! view of the headers ([`HeaderView`]); the token stays in this module.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Env, JoinHandle, Listener, Stream};
use crucible_records::Direction;
use http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, HOST, HeaderMap, USER_AGENT};
use http::{HeaderValue, Request, Response, StatusCode, Version};
use http_body_util::{Either, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1::SendRequest;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use sha2::{Digest as _, Sha256};

use crate::core::{BodyKind, EndReason, HeaderView, StreamId};
use crate::identity::IdentityResolver;
use crate::live::LiveTap;
use crate::net::{Target, connect};

/// Which `Host` header the upstream sees.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum HostHeader {
    /// The one the client sent, so a token audience bound to the public name stays valid.
    #[default]
    Preserve,
    /// The upstream's own authority, for upstreams that route by host name.
    Upstream,
}

/// The settings of the HTTP proxy.
#[derive(Clone, Debug)]
pub struct HttpProxyConfig {
    /// Where requests go. A path in the URL is put in front of every request path.
    pub upstream: Target,
    /// Which `Host` header the upstream sees.
    pub host_header: HostHeader,
    /// The only timeout the proxy adds.
    pub connect_timeout: Duration,
    /// Identity from the bearer token; off unless the operator switches it on.
    pub identity: Arc<IdentityResolver>,
}

#[derive(Debug)]
struct Shared {
    env: Env,
    config: HttpProxyConfig,
    tap: LiveTap,
    next_stream: AtomicU64,
}

/// The upstream connection of one client connection. Dropping it closes the connection.
#[derive(Debug)]
struct Upstream {
    sender: SendRequest<TapBody>,
    driver: JoinHandle<()>,
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// Tells the analyser how an exchange ended, exactly once. Dropped before the end, the client went away.
#[derive(Debug)]
struct ExchangeGuard {
    tap: LiveTap,
    stream: StreamId,
    done: bool,
}

impl ExchangeGuard {
    fn finish(&mut self, reason: EndReason) {
        if !std::mem::replace(&mut self.done, true) {
            self.tap.close(self.stream, reason);
        }
    }
}

impl Drop for ExchangeGuard {
    fn drop(&mut self) {
        self.finish(EndReason::ClientClosed);
    }
}

/// A body that passes every frame on unchanged and hands a copy of the data to the tap.
#[derive(Debug)]
struct TapBody {
    inner: Incoming,
    tap: LiveTap,
    stream: StreamId,
    direction: Direction,
    /// Set on the response body: its end is the end of the exchange.
    guard: Option<ExchangeGuard>,
}

impl Body for TapBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let this = &mut *self;
        match std::task::ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
            Some(Ok(frame)) => {
                let (tap, stream, direction) = (&this.tap, this.stream, this.direction);
                Poll::Ready(Some(Ok(
                    frame.map_data(|data| tap.forward(stream, direction, data))
                )))
            }
            Some(Err(error)) => {
                if let Some(guard) = &mut this.guard {
                    guard.finish(EndReason::UpstreamFailed);
                }
                Poll::Ready(Some(Err(error)))
            }
            None => {
                if let Some(guard) = &mut this.guard {
                    guard.finish(EndReason::Complete);
                }
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TapBody {
    fn drop(&mut self) {
        // A body with nothing left is never polled to its end: it is complete all the same.
        if self.inner.is_end_stream()
            && let Some(guard) = &mut self.guard
        {
            guard.finish(EndReason::Complete);
        }
    }
}

type ProxyBody = Either<TapBody, Full<Bytes>>;

/// The client address cut to its /24 (IPv4) or /48 (IPv6) network (contract 9.1).
pub fn ip_prefix(address: IpAddr) -> String {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            format!("{a}.{b}.{c}.0/24")
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => ip_prefix(IpAddr::V4(v4)),
            None => {
                let [a, b, c, ..] = v6.segments();
                format!("{}/48", std::net::Ipv6Addr::new(a, b, c, 0, 0, 0, 0, 0))
            }
        },
    }
}

/// The stored form of a legacy session id: the first 16 bytes of its SHA-256, in hex (contract 9.4).
pub fn session_key(raw: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(raw);
    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        let _infallible = write!(hex, "{byte:02x}");
    }
    hex
}

fn header_text(headers: &HeaderMap, name: &str, max: usize) -> Option<String> {
    let text = headers.get(name)?.to_str().ok()?;
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_owned())
}

fn is_encoded(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_ENCODING)
        .is_some_and(|encoding| !encoding.as_bytes().eq_ignore_ascii_case(b"identity"))
}

fn media_type(headers: &HeaderMap) -> &str {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map_or("", str::trim)
}

impl Shared {
    /// What the analyser may know about a request. The token is used here and goes no further.
    fn view(&self, headers: &HeaderMap, peer: SocketAddr) -> HeaderView {
        HeaderView {
            user_agent: header_text(headers, USER_AGENT.as_str(), 512).unwrap_or_default(),
            mcp_method: header_text(headers, "mcp-method", 128),
            mcp_name: header_text(headers, "mcp-name", 512),
            protocol_version: header_text(headers, "mcp-protocol-version", 32),
            session_key: headers
                .get("mcp-session-id")
                .map(|raw| session_key(raw.as_bytes())),
            ip_prefix: ip_prefix(peer.ip()),
            body_readable: !is_encoded(headers),
            identity: self.config.identity.resolve(headers.get(AUTHORIZATION)),
        }
    }

    async fn connect(&self) -> Result<Upstream, String> {
        let stream: Stream = connect(
            &self.env,
            &self.config.upstream,
            self.config.connect_timeout,
        )
        .await
        .map_err(|e| e.to_string())?;
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        let driver = self.env.spawn("proxy-http-upstream", async move {
            // Ends when the sender is dropped or the upstream closes.
            let _closed = connection.await;
        });
        Ok(Upstream { sender, driver })
    }

    async fn exchange(
        &self,
        upstream: &tokio::sync::Mutex<Option<Upstream>>,
        peer: SocketAddr,
        request: Request<Incoming>,
    ) -> Response<ProxyBody> {
        let stream = StreamId(self.next_stream.fetch_add(1, Ordering::Relaxed));
        let (parts, body) = request.into_parts();
        self.tap.open(stream, self.view(&parts.headers, peer));
        let mut guard = ExchangeGuard {
            tap: self.tap.clone(),
            stream,
            done: false,
        };

        let path = parts
            .uri
            .path_and_query()
            .map_or("/", http::uri::PathAndQuery::as_str);
        let mut outbound = Request::new(TapBody {
            inner: body,
            tap: self.tap.clone(),
            stream,
            direction: Direction::ClientToServer,
            guard: None,
        });
        *outbound.method_mut() = parts.method;
        *outbound.version_mut() = Version::HTTP_11;
        *outbound.headers_mut() = parts.headers;
        let uri = format!("{}{path}", self.config.upstream.path).parse();
        let host = HeaderValue::from_str(&self.config.upstream.authority());
        let sent = async {
            *outbound.uri_mut() = uri.map_err(|e: http::uri::InvalidUri| e.to_string())?;
            if self.config.host_header == HostHeader::Upstream
                || !outbound.headers().contains_key(HOST)
            {
                outbound
                    .headers_mut()
                    .insert(HOST, host.map_err(|e| e.to_string())?);
            }
            let mut slot = upstream.lock().await;
            if slot.as_ref().is_none_or(|open| open.sender.is_closed()) {
                *slot = Some(self.connect().await?);
            }
            let Some(open) = slot.as_mut() else {
                return Err("no upstream connection".to_owned());
            };
            open.sender.ready().await.map_err(|e| e.to_string())?;
            open.sender
                .send_request(outbound)
                .await
                .map_err(|e| e.to_string())
        };
        match sent.await {
            Ok(response) => {
                let (parts, body) = response.into_parts();
                let kind = if is_encoded(&parts.headers) {
                    BodyKind::Opaque
                } else {
                    match media_type(&parts.headers) {
                        "application/json" => BodyKind::Json,
                        "text/event-stream" => BodyKind::Sse,
                        _ => BodyKind::Opaque,
                    }
                };
                let session = parts
                    .headers
                    .get("mcp-session-id")
                    .map(|raw| session_key(raw.as_bytes()));
                self.tap.head(stream, parts.status.as_u16(), kind, session);
                Response::from_parts(
                    parts,
                    Either::Left(TapBody {
                        inner: body,
                        tap: self.tap.clone(),
                        stream,
                        direction: Direction::ServerToClient,
                        guard: Some(guard),
                    }),
                )
            }
            Err(error) => {
                guard.finish(EndReason::UpstreamFailed);
                tracing::warn!(%error, upstream = %self.config.upstream.authority(), "the upstream request failed");
                // The one response the proxy writes itself: there is no upstream answer to pass on.
                let body = format!(
                    "crucible proxy: the upstream MCP server at {} could not be reached: {error}\n",
                    self.config.upstream.authority()
                );
                let mut response = Response::new(Either::Right(Full::new(Bytes::from(body))));
                *response.status_mut() = StatusCode::BAD_GATEWAY;
                response.headers_mut().insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                );
                response
            }
        }
    }
}

async fn connection(shared: Arc<Shared>, stream: Stream, peer: SocketAddr) {
    let upstream = Arc::new(tokio::sync::Mutex::new(None));
    let service = service_fn(move |request| {
        let (shared, upstream) = (Arc::clone(&shared), Arc::clone(&upstream));
        async move { Ok::<_, Infallible>(shared.exchange(&upstream, peer, request).await) }
    });
    let served = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
    if let Err(error) = served {
        tracing::debug!(%error, %peer, "a client connection ended with an error");
    }
}

/// Serves `listener` until the `Env`'s shutdown scope is asked to stop.
pub async fn serve(
    env: Env,
    mut listener: Box<dyn Listener>,
    config: HttpProxyConfig,
    tap: LiveTap,
) {
    let shared = Arc::new(Shared {
        env: env.clone(),
        config,
        tap,
        // Stream 0 is the stdio session.
        next_stream: AtomicU64::new(1),
    });
    let mut shutdown = env.shutdown();
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    drop(env.spawn("proxy-http-connection", connection(Arc::clone(&shared), stream, peer)));
                }
                Err(error) => {
                    tracing::warn!(%error, "accepting a connection failed");
                    // A failing accept (for example no file descriptors left) must not spin.
                    env.clock.sleep(Duration::from_millis(50)).await;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_cut_to_their_network_and_session_ids_are_hashed() {
        assert_eq!(ip_prefix("203.0.113.77".parse().unwrap()), "203.0.113.0/24");
        assert_eq!(ip_prefix("127.0.0.1".parse().unwrap()), "127.0.0.0/24");
        assert_eq!(
            ip_prefix("2001:db8:abcd:1234:5678::1".parse().unwrap()),
            "2001:db8:abcd::/48"
        );
        assert_eq!(
            ip_prefix("::ffff:192.0.2.9".parse().unwrap()),
            "192.0.2.0/24"
        );
        // The vectors of conformance/registry.json (contract 9.4).
        assert_eq!(
            session_key(b"3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5"),
            "84757436d5e0791bd4a8eaa6f1d05986"
        );
        assert_eq!(
            session_key(b"conformance-session"),
            "b1427bda7462603423a9e4a7b73fbdd0"
        );
    }
}
