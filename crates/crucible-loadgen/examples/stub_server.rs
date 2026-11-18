//! The stub OTLP/HTTP server for the generator accuracy baseline (`docs/testing/perf.md`): every valid
//! `POST` gets 200 and an empty `ExportTraceServiceResponse`.
//!
//! `cargo run --release -p crucible-loadgen --example stub_server -- --listen 127.0.0.1:14318 --threads 2`

use std::io::Write as _;
use std::net::SocketAddr;

use clap::Parser;
use crucible_env_prod::ProdConfig;
use crucible_loadgen::stub::{StubConfig, serve};

#[derive(Debug, Parser)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:14318")]
    listen: SocketAddr,
    /// Runtime worker threads; the default is the CPUs available to the process.
    #[arg(long)]
    threads: Option<usize>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(threads) = args.threads {
        builder.worker_threads(threads);
    }
    let runtime = builder.enable_all().build()?;
    let env = crucible_env_prod::build(runtime.handle().clone(), &ProdConfig::default())?.env;
    runtime.block_on(async move {
        let listener = env.net.listen(args.listen).await?;
        let mut stdout = std::io::stdout();
        writeln!(stdout, "listening on {}", listener.local_addr()?)?;
        stdout.flush()?;
        serve(env, listener, StubConfig::default()).await?;
        anyhow::Ok(())
    })
}
