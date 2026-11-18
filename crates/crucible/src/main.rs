//! The Crucible server binary. Subcommands are wired as their crates land.

mod cursor_keys;
mod keys;
mod recent;
mod serve;
mod store;

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use crucible_env_prod::WalBackend;

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(
    name = "crucible",
    version,
    about = "Observability and analytics for AI agents and MCP servers"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server.
    Serve(Box<ServeArgs>),
    /// Print the OpenAPI 3.1 document of the REST API (committed at specs/openapi/crucible-v1.json).
    Openapi,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Address of the main listener (API, UI, ingest and health paths).
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Address of the OTLP/HTTP ingest listener.
    #[arg(long, default_value = "0.0.0.0:4318")]
    ingest_listen: SocketAddr,
    /// Address of the OTLP/gRPC ingest listener.
    #[arg(long, default_value = "0.0.0.0:4317")]
    grpc_listen: SocketAddr,
    /// TOML file of the project keys that ingest accepts: `[[project_key]]` tables with `project_id`,
    /// `public_key` and `secret`. Without it, every ingest request is refused.
    #[arg(long)]
    keys_file: Option<PathBuf>,
    /// Address of the Prometheus metrics listener.
    #[arg(long, default_value = "127.0.0.1:9464")]
    metrics_listen: SocketAddr,
    /// Directory that holds the `wal`, `quarantine` and `exports` roots.
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    /// Disk backend for the WAL: `auto`, `portable` or `uring`.
    #[arg(long, default_value = "auto")]
    wal_backend: WalBackend,
    /// Serve the UI from this directory (a built export such as `ui/out`) instead of the one compiled in.
    /// The files are read once at startup.
    #[arg(long)]
    ui_dir: Option<PathBuf>,
    /// HTTP endpoint of the ClickHouse telemetry store, `http://host:8123` or `https://host:8443`. Without
    /// it the server only ingests: requests are made durable and wait in the WAL, and the API answers 503.
    #[arg(long, env = "CRUCIBLE_CLICKHOUSE_URL")]
    clickhouse_url: Option<String>,
    /// The ClickHouse admin user that the operator created; it runs the migrations.
    #[arg(
        long,
        env = "CRUCIBLE_CLICKHOUSE_USER",
        default_value = "crucible_admin"
    )]
    clickhouse_user: String,
    /// File that holds the admin user's password. Without it the password is read from the environment
    /// variable CRUCIBLE_CLICKHOUSE_PASSWORD.
    #[arg(long, env = "CRUCIBLE_CLICKHOUSE_PASSWORD_FILE")]
    clickhouse_password_file: Option<PathBuf>,
    /// File that holds the master key as 64 hexadecimal characters. The passwords of the four service
    /// identities are derived from it. Without it the key is read from the environment variable
    /// CRUCIBLE_MASTER_KEY.
    #[arg(long, env = "CRUCIBLE_MASTER_KEY_FILE")]
    master_key_file: Option<PathBuf>,
    /// Apply the ClickHouse schema and create the service identities before the startup checks. The steps
    /// are idempotent.
    #[arg(long)]
    migrate: bool,
    /// Bound on the memory of the recent-span index, in bytes. The default is 10% of RAM, at most 1 GiB.
    #[arg(long)]
    recent_max_bytes: Option<u64>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve(args) => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .init();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("crucible-worker")
                .build()?;
            runtime.block_on(serve::run(*args))
        }
        Command::Openapi => {
            let json = crucible_api::openapi_json()?;
            std::io::stdout().lock().write_all(json.as_bytes())?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_well_formed_and_prints_the_version() {
        Cli::command().debug_assert();
        let version = Cli::command().render_version();
        assert_eq!(
            version.trim(),
            format!("crucible {}", env!("CARGO_PKG_VERSION"))
        );
    }

    fn serve_args(argv: &[&str]) -> ServeArgs {
        match Cli::parse_from(argv).command {
            Command::Serve(args) => *args,
            other => panic!("{argv:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn serve_defaults_match_the_contract_ports() {
        let args = serve_args(&["crucible", "serve"]);
        assert_eq!(args.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(args.ingest_listen.to_string(), "0.0.0.0:4318");
        assert_eq!(args.grpc_listen.to_string(), "0.0.0.0:4317");
        assert_eq!(args.metrics_listen.to_string(), "127.0.0.1:9464");
        assert_eq!(args.wal_backend, WalBackend::Auto);
        assert_eq!(args.keys_file, None);
        assert_eq!(args.ui_dir, None);
        assert!(Cli::try_parse_from(["crucible", "serve", "--wal-backend", "floppy"]).is_err());
        let args = serve_args(&["crucible", "serve", "--ui-dir", "ui/out"]);
        assert_eq!(args.ui_dir, Some(PathBuf::from("ui/out")));
    }

    #[test]
    fn serve_takes_the_telemetry_store_from_flags() {
        let args = serve_args(&[
            "crucible",
            "serve",
            "--clickhouse-url",
            "http://clickhouse:8123",
            "--clickhouse-password-file",
            "/run/secrets/clickhouse",
            "--master-key-file",
            "/run/secrets/master",
            "--migrate",
            "--recent-max-bytes",
            "1048576",
        ]);
        assert_eq!(
            args.clickhouse_url.as_deref(),
            Some("http://clickhouse:8123")
        );
        assert_eq!(args.clickhouse_user, "crucible_admin");
        assert_eq!(
            args.clickhouse_password_file,
            Some(PathBuf::from("/run/secrets/clickhouse"))
        );
        assert_eq!(
            args.master_key_file,
            Some(PathBuf::from("/run/secrets/master"))
        );
        assert!(args.migrate);
        assert_eq!(args.recent_max_bytes, Some(1 << 20));
    }

    #[test]
    fn openapi_takes_no_arguments() {
        assert!(matches!(
            Cli::parse_from(["crucible", "openapi"]).command,
            Command::Openapi
        ));
        assert!(Cli::try_parse_from(["crucible", "openapi", "--yaml"]).is_err());
    }
}
