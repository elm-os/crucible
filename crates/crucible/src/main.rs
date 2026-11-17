//! The Crucible server binary. Subcommands are wired as their crates land.

mod serve;

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
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Address of the main listener (API, UI, ingest and health paths).
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// Address of the Prometheus metrics listener.
    #[arg(long, default_value = "127.0.0.1:9464")]
    metrics_listen: SocketAddr,
    /// Directory that holds the `wal`, `quarantine` and `exports` roots.
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    /// Disk backend for the WAL: `auto`, `portable` or `uring`.
    #[arg(long, default_value = "auto")]
    wal_backend: WalBackend,
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

    #[test]
    fn serve_defaults_match_the_contract_ports() {
        let Command::Serve(args) = Cli::parse_from(["crucible", "serve"]).command;
        assert_eq!(args.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(args.metrics_listen.to_string(), "127.0.0.1:9464");
        assert_eq!(args.wal_backend, WalBackend::Auto);
        assert!(Cli::try_parse_from(["crucible", "serve", "--wal-backend", "floppy"]).is_err());
    }
}
