//! Connections: tokio-postgres run over Crucible's `Net`, so the adapter opens no socket, resolves no name
//! and spawns no task outside the `Env` (contract 2.5), and a pool whose manager connects that way.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;

use crucible_control::TxError;
use crucible_env::{Env, NetError, Stream, TlsRoots};
use deadpool::managed::{self, Metrics, RecycleError, RecycleResult};
use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::tls::{ChannelBinding, NoTls, TlsConnect, TlsStream};
use tokio_postgres::{Client, Connection};

/// The statement timeout and the idle-in-transaction timeout of every connection (contract 2.7).
pub(crate) const STATEMENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections of the reserved pool: the key feed, leases and session lookups (contract 2.7).
pub const RESERVED_POOL: usize = 2;

/// How to reach Postgres.
#[derive(Clone)]
pub struct PgConfig {
    /// `postgres://user[:password]@host[:port]/database`. `?sslmode=require` turns TLS on, verified against
    /// `tls_roots`; without it the connection is plain, which is the default for an internal network.
    pub url: String,
    /// The password, when the URL carries none.
    pub password: Option<String>,
    /// Connections of the main pool.
    pub main_pool: usize,
    /// How long opening a connection may take.
    pub connect_timeout: Duration,
    /// How long a call waits for a pooled connection before it fails as unavailable.
    pub acquire_timeout: Duration,
    /// Send the two 10 s timeouts as startup options. A pooler that refuses startup options needs this off;
    /// transactions set the timeouts themselves either way.
    pub startup_timeouts: bool,
    /// The trust anchors for `sslmode=require`.
    pub tls_roots: TlsRoots,
}

impl PgConfig {
    /// The defaults around a connection URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            password: None,
            main_pool: 8,
            connect_timeout: Duration::from_secs(10),
            acquire_timeout: Duration::from_secs(5),
            startup_timeouts: true,
            tls_roots: TlsRoots::WebPki,
        }
    }
}

impl fmt::Debug for PgConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The URL and the password are secrets.
        f.debug_struct("PgConfig")
            .field("main_pool", &self.main_pool)
            .field("connect_timeout", &self.connect_timeout)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("startup_timeouts", &self.startup_timeouts)
            .finish_non_exhaustive()
    }
}

fn config_error(what: &str) -> TxError {
    // The URL is never echoed: it can hold the password.
    TxError::Invalid(format!(
        "postgres.url {what}; expected postgres://user[:password]@host[:port]/database"
    ))
}

/// Opens connections through `env.net`.
#[derive(Clone)]
pub(crate) struct Connector {
    env: Env,
    pg: tokio_postgres::Config,
    host: String,
    port: u16,
    tls: bool,
    roots: TlsRoots,
    timeout: Duration,
}

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connector")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl Connector {
    pub(crate) fn new(env: &Env, config: &PgConfig) -> Result<Self, TxError> {
        let mut pg = tokio_postgres::Config::from_str(&config.url)
            .map_err(|_| config_error("is not a valid connection URL"))?;
        if let Some(password) = &config.password {
            pg.password(password);
        }
        if pg.get_user().is_none() {
            return Err(config_error("names no user"));
        }
        if pg.get_dbname().is_none() {
            return Err(config_error("names no database"));
        }
        let host = match pg.get_hosts() {
            [Host::Tcp(host)] => host.clone(),
            [] => return Err(config_error("names no host")),
            _ => return Err(config_error("must name exactly one TCP host")),
        };
        let port = pg.get_ports().first().copied().unwrap_or(5432);
        let tls = pg.get_ssl_mode() == SslMode::Require;
        pg.ssl_mode(if tls {
            SslMode::Require
        } else {
            SslMode::Disable
        });
        pg.application_name("crucible");
        if config.startup_timeouts {
            let millis = STATEMENT_TIMEOUT.as_millis();
            pg.options(format!(
                "-c statement_timeout={millis} -c idle_in_transaction_session_timeout={millis}"
            ));
        }
        Ok(Self {
            env: env.clone(),
            pg,
            host,
            port,
            tls,
            roots: config.tls_roots.clone(),
            timeout: config.connect_timeout,
        })
    }

    async fn stream(&self) -> Result<Stream, NetError> {
        let deadline = self.env.clock.now().saturating_add(self.timeout);
        let addresses = match self.host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => self.env.net.resolve(&self.host).await?,
        };
        let mut last = NetError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "the host resolves to no address",
        ));
        for ip in addresses {
            match self
                .env
                .net
                .connect(SocketAddr::new(ip, self.port), deadline)
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    /// Opens one connection and runs its protocol task on the `Env` spawner.
    pub(crate) async fn connect(&self) -> Result<Client, TxError> {
        let unavailable = |error: &dyn fmt::Display| {
            TxError::Unavailable(format!(
                "cannot connect to Postgres at {}:{}: {error}",
                self.host, self.port
            ))
        };
        let handshake = Box::pin(async {
            let stream = self.stream().await.map_err(|error| unavailable(&error))?;
            if self.tls {
                let tls = NetTls {
                    env: self.env.clone(),
                    host: self.host.clone(),
                    roots: self.roots.clone(),
                };
                let (client, connection) = self
                    .pg
                    .connect_raw(stream, tls)
                    .await
                    .map_err(|error| unavailable(&error))?;
                self.drive(connection);
                Ok(client)
            } else {
                let (client, connection) = self
                    .pg
                    .connect_raw(stream, NoTls)
                    .await
                    .map_err(|error| unavailable(&error))?;
                self.drive(connection);
                Ok(client)
            }
        });
        match self.env.timeout(self.timeout, handshake).await {
            Ok(result) => result,
            Err(elapsed) => Err(unavailable(&elapsed)),
        }
    }

    fn drive<T>(&self, connection: Connection<Stream, T>)
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // Dropping the handle detaches the task; it ends when the client is dropped or the socket closes.
        drop(self.env.spawn("postgres-connection", async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, "a postgres connection ended");
            }
        }));
    }
}

/// The TLS upgrade of one connection, through `Net::tls_connect`.
struct NetTls {
    env: Env,
    host: String,
    roots: TlsRoots,
}

impl TlsConnect<Stream> for NetTls {
    type Stream = TlsIo;
    type Error = NetError;
    type Future = BoxFuture<'static, Result<TlsIo, NetError>>;

    fn connect(self, stream: Stream) -> Self::Future {
        Box::pin(async move {
            let stream = self
                .env
                .net
                .tls_connect(stream, &self.host, &self.roots)
                .await?;
            Ok(TlsIo(stream))
        })
    }
}

/// A TLS stream from `Net`. It offers no channel binding, so SCRAM runs without it.
struct TlsIo(Stream);

impl TlsStream for TlsIo {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

impl AsyncRead for TlsIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsIo {
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
}

/// The pool manager: connections come from the [`Connector`]; a closed one is not reused.
#[derive(Debug)]
pub(crate) struct Manager(pub(crate) Connector);

impl managed::Manager for Manager {
    type Type = Client;
    type Error = TxError;

    async fn create(&self) -> Result<Client, TxError> {
        self.0.connect().await
    }

    async fn recycle(&self, client: &mut Client, _: &Metrics) -> RecycleResult<TxError> {
        if client.is_closed() {
            Err(RecycleError::Message("the connection is closed".into()))
        } else {
            Ok(())
        }
    }
}

pub(crate) type Pool = managed::Pool<Manager>;
pub(crate) type Pooled = managed::Object<Manager>;

/// A pool of at most `size` connections. No connection is opened until one is needed.
pub(crate) fn pool(connector: Connector, size: usize) -> Result<Pool, TxError> {
    Pool::builder(Manager(connector))
        .max_size(size.max(1))
        .build()
        .map_err(|error| TxError::Invalid(format!("building the Postgres pool: {error}")))
}
