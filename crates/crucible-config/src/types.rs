//! The typed configuration.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// A secret value. It never prints: `Debug` and `Display` both write a placeholder.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Never log or print it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The disk backend of the WAL.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WalBackend {
    /// io_uring where the build and the kernel offer it, the portable backend otherwise.
    Auto,
    /// The portable backend.
    Portable,
    /// io_uring, or a startup error.
    Uring,
}

/// Where a setting's value came from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// No layer set it.
    Default,
    /// The configuration file.
    File,
    /// This environment variable.
    Env(String),
    /// A command-line flag.
    Flag,
}

impl Source {
    /// Later layers override earlier ones.
    pub(crate) const fn rank(&self) -> u8 {
        match self {
            Self::Default => 0,
            Self::File => 1,
            Self::Env(_) => 2,
            Self::Flag => 3,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::File => f.write_str("file"),
            Self::Env(name) => write!(f, "env {name}"),
            Self::Flag => f.write_str("flag"),
        }
    }
}

/// The `[server]` section.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Server {
    /// Address of the main listener.
    pub listen: SocketAddr,
    /// Address of the OTLP/HTTP ingest listener.
    pub ingest_listen: SocketAddr,
    /// Address of the OTLP/gRPC ingest listener.
    pub grpc_listen: SocketAddr,
    /// Address of the Prometheus metrics listener.
    pub metrics_listen: SocketAddr,
    /// Serve the UI from this directory instead of the one compiled in.
    pub ui_dir: Option<PathBuf>,
    /// TOML file of the project keys, for a server without Postgres.
    pub keys_file: Option<PathBuf>,
    /// The URL people reach the server at.
    pub public_url: Option<String>,
    /// Origins besides the server's own that may send cookie-authenticated writes.
    pub allowed_origins: Vec<String>,
}

/// `auth.pg_outage_mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PgOutageMode {
    /// Known keys keep working for ingest.
    FailOpen,
    /// Every secret key is refused after 5 s without a successful poll.
    FailClosed,
}

/// The `[auth]` section.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Auth {
    /// API keys during a Postgres outage.
    pub pg_outage_mode: PgOutageMode,
    /// Seconds after which a fail-open outage behaves as fail-closed; `None` is unlimited.
    pub fail_open_max: Option<u64>,
    /// Seconds that cached sessions stay valid during a Postgres outage.
    pub session_outage_grace: u64,
    /// `security.require_https`: refuse sign-in unless the public URL is HTTPS.
    pub require_https: bool,
}

/// The `[data]` section.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Data {
    /// Directory that holds the `wal`, `quarantine` and `exports` roots.
    pub dir: PathBuf,
    /// Disk backend of the WAL.
    pub wal_backend: WalBackend,
}

/// The `[mcp]` section: the per-key limit on the tool calls of the MCP server at `/mcp`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mcp {
    /// Tool calls one key may make per second, sustained; 0 turns the limit off.
    pub calls_per_second: u32,
    /// Tool calls one key may make at once after a quiet spell.
    pub burst: u32,
}

/// The `[sql]` section: the SQL workspace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sql {
    /// Statements one project's workspace identity may send to ClickHouse per hour. A run through the API is
    /// three: two checks and the query.
    pub statements_per_hour: u64,
}

/// The `[egress]` section: which notification targets may be called (contract 12.6).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Egress {
    /// `egress.mode = strict`: public `https` targets only. The default, `open`, allows any network except
    /// cloud metadata addresses.
    pub strict: bool,
    /// `egress.allow_cidrs`: ranges that strict mode lets through, each as `address/prefix`.
    pub allow_cidrs: Vec<String>,
}

/// `smtp.tls`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SmtpTls {
    /// Upgrade a plain connection with STARTTLS.
    StartTls,
    /// TLS from the first byte.
    Tls,
    /// No TLS: for a relay on the same host or a trusted network.
    None,
}

/// The `[smtp]` section, present when `smtp.host` is set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Smtp {
    /// The mail server's host.
    pub host: String,
    /// The mail server's port.
    pub port: u16,
    /// How the connection is secured.
    pub tls: SmtpTls,
    /// The user name, when the server asks for one.
    pub username: Option<String>,
    /// The password.
    pub password: Option<Secret>,
    /// The sender address.
    pub from: String,
}

/// The `[clickhouse]` section, present when `clickhouse.url` is set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClickHouse {
    /// HTTP endpoint of the telemetry store.
    pub url: String,
    /// The admin user.
    pub user: String,
    /// The admin user's password.
    pub password: Secret,
    /// The master key, 64 hexadecimal characters: the service passwords derive from it.
    pub master_key: Secret,
    /// How long an idle connection is kept; `None` derives it from the server's keep-alive timeout.
    pub idle_timeout: Option<Duration>,
}

/// The `[postgres]` section, present when `postgres.url` is set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Postgres {
    /// `postgres://user[:password]@host[:port]/database`. It may carry a password, so it is a secret.
    pub url: Secret,
    /// The password, when the URL carries none.
    pub password: Option<Secret>,
    /// Connections of the main pool.
    pub pool_size: usize,
}

/// The whole configuration.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    /// Listeners and what they serve.
    pub server: Server,
    /// The data directory.
    pub data: Data,
    /// Bound on the memory of the recent-span index, in bytes; `None` for the default.
    pub recent_max_bytes: Option<u64>,
    /// The MCP server's rate limit.
    pub mcp: Mcp,
    /// The SQL workspace.
    pub sql: Sql,
    /// The master key, 64 hexadecimal characters.
    pub master_key: Option<Secret>,
    /// The telemetry store; `None` for the ingest-only mode.
    pub clickhouse: Option<ClickHouse>,
    /// The control plane; `None` to run without one.
    pub postgres: Option<Postgres>,
    /// Sign-in and API keys.
    pub auth: Auth,
    /// The egress policy of notification calls.
    pub egress: Egress,
    /// The mail server; `None` when email channels are off.
    pub smtp: Option<Smtp>,
}

/// Everything that is wrong with a configuration, one message per problem.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub struct ConfigErrors(pub Vec<String>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.0.len();
        let noun = if count == 1 { "problem" } else { "problems" };
        write!(f, "the configuration has {count} {noun}:")?;
        for error in &self.0 {
            write!(f, "\n  - {error}")?;
        }
        Ok(())
    }
}
