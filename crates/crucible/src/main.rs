//! The Crucible server binary. Subcommands are wired as their crates land.

mod keys;
mod serve;

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
    Serve(ServeArgs),
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
    /// TOML file of the project keys that ingest accepts: `[[project_key]]` tables with `project_id` and
    /// `secret`. Without it, every ingest request is refused.
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
            runtime.block_on(serve::run(args))
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
            Command::Serve(args) => args,
            other => panic!("{argv:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn serve_defaults_match_the_contract_ports() {
        let args = serve_args(&["crucible", "serve"]);
        assert_eq!(args.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(args.ingest_listen.to_string(), "0.0.0.0:4318");
        assert_eq!(args.metrics_listen.to_string(), "127.0.0.1:9464");
        assert_eq!(args.wal_backend, WalBackend::Auto);
        assert_eq!(args.keys_file, None);
        assert_eq!(args.ui_dir, None);
        assert!(Cli::try_parse_from(["crucible", "serve", "--wal-backend", "floppy"]).is_err());
        let args = serve_args(&["crucible", "serve", "--ui-dir", "ui/out"]);
        assert_eq!(args.ui_dir, Some(PathBuf::from("ui/out")));
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
