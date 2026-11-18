//! The connection layer: the official client's HTTP stack run over Crucible's `Net`, so the store opens no
//! socket, resolves no name and spawns no task outside the `Env` (contract 2.5).

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crucible_env::{Env, NetError, Stream, TlsRoots};
use futures::future::BoxFuture;
use http::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;

/// The pool drops a connection idle for this long, a little under the server's keep-alive timeout.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a connection could not be opened.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConnectError {
    #[error("the url has no host")]
    NoHost,
    #[error("unsupported url scheme {0:?}")]
    Scheme(String),
    #[error("{0} resolves to no address")]
    NoAddress(String),
    #[error(transparent)]
    Net(#[from] NetError),
}

/// A tower `Service<Uri>` that connects through `Env::net`: `resolve`, `connect` and, for `https`, `tls_connect`.
#[derive(Clone, Debug)]
pub(crate) struct NetConnector {
    env: Env,
    roots: TlsRoots,
    timeout: Duration,
}

impl NetConnector {
    async fn connect(self, uri: Uri) -> Result<Conn, ConnectError> {
        let tls = match uri.scheme_str() {
            Some("http") => false,
            Some("https") => true,
            other => return Err(ConnectError::Scheme(other.unwrap_or_default().to_owned())),
        };
        let host = uri.host().ok_or(ConnectError::NoHost)?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = uri.port_u16().unwrap_or(if tls { 8443 } else { 8123 });
        let deadline = self.env.clock.now().saturating_add(self.timeout);
        let addresses = match host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => self.env.net.resolve(host).await?,
        };
        let mut last = ConnectError::NoAddress(host.to_owned());
        for ip in addresses {
            match self
                .env
                .net
                .connect(SocketAddr::new(ip, port), deadline)
                .await
            {
                Ok(stream) if tls => {
                    let stream = self.env.net.tls_connect(stream, host, &self.roots).await?;
                    return Ok(Conn(TokioIo::new(stream)));
                }
                Ok(stream) => return Ok(Conn(TokioIo::new(stream))),
                Err(error) => last = ConnectError::Net(error),
            }
        }
        Err(last)
    }
}

impl tower::Service<Uri> for NetConnector {
    type Response = Conn;
    type Error = ConnectError;
    type Future = BoxFuture<'static, Result<Conn, ConnectError>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        Box::pin(self.clone().connect(uri))
    }
}

/// One connection: a `Net` stream behind hyper's IO traits.
#[derive(Debug)]
pub(crate) struct Conn(TokioIo<Stream>);

impl Connection for Conn {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl Read for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl Write for Conn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write_vectored(cx, bufs)
    }
}

/// Runs hyper's connection tasks on the `Env` spawner, inside the host's shutdown scope.
#[derive(Clone, Debug)]
struct EnvExecutor(Env);

impl<F> hyper::rt::Executor<F> for EnvExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        // Dropping the handle detaches the task; the pool owns the connection's lifetime.
        drop(self.0.spawn("clickhouse-http", async move {
            future.await;
        }));
    }
}

/// A ClickHouse client whose every connection goes through `env.net`. Clients cloned from it share one pool.
pub(crate) fn client(env: &Env, roots: TlsRoots, connect_timeout: Duration) -> clickhouse::Client {
    let connector = NetConnector {
        env: env.clone(),
        roots,
        timeout: connect_timeout,
    };
    let http = hyper_util::client::legacy::Client::builder(EnvExecutor(env.clone()))
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build(connector);
    clickhouse::Client::with_http_client(http).with_compression(clickhouse::Compression::None)
}
