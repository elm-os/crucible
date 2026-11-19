//! `crucible-loadgen`: the open-loop OTLP load generator and the fsync-honesty probe
//! (`docs/testing/perf.md`). This file builds the production `Env`; it is the only place in the crate that
//! touches ambient APIs (the process CPU clock and the report file).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use crucible_env::Root;
use crucible_env_prod::{ProdConfig, WalBackend};
use crucible_loadgen::{GeneratorCpu, Pool, PoolConfig, RunConfig, SpanSize};
use crucible_records::Uuid;
use http::{HeaderName, HeaderValue};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "crucible-loadgen",
    version,
    about = "Open-loop OTLP load generator and fsync-honesty probe"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Send OTLP/HTTP trace requests on a fixed schedule and report latency, statuses and rates as JSON.
    Run(RunArgs),
    /// Measure write-plus-fdatasync latency on a volume and classify its flushes, as JSON.
    FsyncProbe(ProbeArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// OTLP/HTTP listener as host:port; requests go to /v1/traces.
    #[arg(long, default_value = "127.0.0.1:4318")]
    target: String,
    /// Requests per second.
    #[arg(long)]
    rate: u64,
    /// Schedule length in seconds.
    #[arg(long, default_value_t = 10)]
    duration_secs: u64,
    /// Encoded size of each span: 1k or 10k.
    #[arg(long, default_value = "1k")]
    span_size: SpanSize,
    /// Spans per request.
    #[arg(long, default_value_t = 100)]
    spans_per_request: u32,
    /// Traces per request: the request's spans split into this many traces of consecutive spans.
    #[arg(long, default_value_t = 1)]
    traces_per_request: u32,
    /// Give no trace a root: the first span of every trace names a parent that no span carries.
    #[arg(long)]
    rootless: bool,
    /// Gzip every request body.
    #[arg(long)]
    gzip: bool,
    /// With --gzip, the most bodies compressed at a time, each on its own blocking thread; the default is half
    /// the CPUs available to the process, at least one.
    #[arg(long)]
    gzip_threads: Option<usize>,
    /// Connections, which bound the requests in flight.
    #[arg(long, default_value_t = 64)]
    concurrency: usize,
    /// Seed for span content.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Run id carried by every span; a fresh UUIDv7 when absent.
    #[arg(long)]
    run_id: Option<Uuid>,
    /// Distinct pre-encoded request templates.
    #[arg(long, default_value_t = 16)]
    templates: usize,
    /// Deadline for connecting and for each request, in milliseconds.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,
    /// Extra request header as `Name: value`, for example an Authorization header; repeatable.
    #[arg(long = "header", value_parser = parse_header)]
    headers: Vec<(HeaderName, HeaderValue)>,
    /// Runtime worker threads; the default is the CPUs available to the process, less the gzip threads with
    /// --gzip, so that compression never competes with the pacer for a CPU.
    #[arg(long)]
    threads: Option<usize>,
    /// Write the report here instead of to stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

impl RunArgs {
    /// The compression threads and the runtime's worker threads.
    fn threads(&self) -> (usize, usize) {
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        if !self.gzip {
            return (0, self.threads.unwrap_or(cpus));
        }
        let gzip = self.gzip_threads.unwrap_or((cpus / 2).max(1));
        (
            gzip,
            self.threads.unwrap_or(cpus.saturating_sub(gzip).max(1)),
        )
    }
}

#[derive(Debug, Args)]
struct ProbeArgs {
    /// Directory on the volume to probe; the probe works in a fresh subdirectory and removes it.
    #[arg(long)]
    dir: PathBuf,
    /// Measured iterations per phase (the largest write sizes use fewer to cap the bytes written).
    #[arg(long, default_value_t = 200)]
    iterations: u32,
    /// Write the report here instead of to stdout.
    #[arg(long)]
    out: Option<PathBuf>,
}

fn parse_header(raw: &str) -> Result<(HeaderName, HeaderValue), String> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| "expected `Name: value`".to_owned())?;
    let name = HeaderName::try_from(name.trim()).map_err(|e| e.to_string())?;
    let mut value = HeaderValue::try_from(value.trim()).map_err(|e| e.to_string())?;
    value.set_sensitive(true);
    Ok((name, value))
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Run(args) => run(args),
        Command::FsyncProbe(args) => probe(args),
    }
}

fn runtime(threads: Option<usize>) -> anyhow::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(threads) = threads {
        builder.worker_threads(threads);
    }
    Ok(builder.enable_all().thread_name("loadgen").build()?)
}

fn run(args: RunArgs) -> anyhow::Result<()> {
    let (gzip_threads, threads) = args.threads();
    let runtime = runtime(Some(threads))?;
    let prod = crucible_env_prod::build(runtime.handle().clone(), &ProdConfig::default())?;
    let env = prod.env;
    let report = runtime.block_on(async {
        let target = crucible_loadgen::resolve(&env, &args.target).await?;
        let run_id = args.run_id.unwrap_or_else(|| env.new_id());
        let pool = Pool::build(
            PoolConfig {
                span_size: args.span_size,
                spans_per_request: args.spans_per_request,
                traces_per_request: args.traces_per_request,
                rootless: args.rootless,
                templates: args.templates,
                seed: args.seed,
            },
            run_id,
        )?;
        let config = RunConfig {
            target,
            authority: args.target.clone(),
            rate: args.rate,
            duration: Duration::from_secs(args.duration_secs),
            concurrency: args.concurrency,
            gzip: args.gzip,
            gzip_threads,
            timeout: Duration::from_millis(args.timeout_ms),
            headers: args.headers.clone(),
        };
        let before = process_cpu();
        let outcome = crucible_loadgen::run(&env, Arc::new(pool), &config).await?;
        let after = process_cpu();
        let mut report = outcome.report;
        if let (Some(before), Some(after)) = (before, after) {
            let (user_s, system_s) = (after.0 - before.0, after.1 - before.1);
            let sent = report.outcome.sent.max(1) as f64;
            report.generator_cpu = Some(GeneratorCpu {
                user_s,
                system_s,
                us_per_request: (user_s + system_s) * 1e6 / sent,
            });
        }
        anyhow::Ok(report)
    })?;
    write_report(args.out.as_deref(), &report)
}

fn probe(args: ProbeArgs) -> anyhow::Result<()> {
    let runtime = runtime(None)?;
    let config = ProdConfig {
        roots: BTreeMap::from([(Root::Wal, args.dir.clone())]),
        wal_backend: WalBackend::Portable,
    };
    let prod = crucible_env_prod::build(runtime.handle().clone(), &config)?;
    let mut report = runtime.block_on(crucible_loadgen::probe(&prod.env, args.iterations))?;
    report.dir = args.dir.display().to_string();
    write_report(args.out.as_deref(), &report)
}

#[allow(clippy::disallowed_methods)]
fn write_report(out: Option<&Path>, report: &impl Serialize) -> anyhow::Result<()> {
    let mut json = serde_json::to_vec_pretty(report)?;
    json.push(b'\n');
    match out {
        Some(path) => std::fs::write(path, json)?,
        None => std::io::stdout().write_all(&json)?,
    }
    Ok(())
}

/// User and system CPU seconds of this process so far, from `/proc/self/stat` (fields 14 and 15, in clock
/// ticks of 1/100 s, the fixed USER_HZ of the Linux ABI).
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)]
fn process_cpu() -> Option<(f64, f64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name (field 2) is parenthesized and may hold spaces; field 3 follows the last ')'.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    let ticks = |field: usize| fields.get(field - 3)?.parse::<u64>().ok();
    Some((ticks(14)? as f64 / 100.0, ticks(15)? as f64 / 100.0))
}

#[cfg(not(target_os = "linux"))]
fn process_cpu() -> Option<(f64, f64)> {
    None
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_defaults_follow_the_contract_port() {
        let Command::Run(args) =
            Cli::parse_from(["crucible-loadgen", "run", "--rate", "10"]).command
        else {
            panic!("expected run");
        };
        assert_eq!(args.target, "127.0.0.1:4318");
        assert_eq!(args.span_size, SpanSize::Kib1);
        assert!(args.headers.is_empty() && !args.gzip);
        let parsed = Cli::try_parse_from([
            "crucible-loadgen",
            "run",
            "--rate",
            "1",
            "--span-size",
            "10k",
            "--header",
            "Authorization: Bearer csk_test",
        ])
        .unwrap();
        let Command::Run(args) = parsed.command else {
            panic!("expected run");
        };
        assert_eq!(args.span_size, SpanSize::Kib10);
        assert_eq!(args.headers[0].0, http::header::AUTHORIZATION);
        assert_eq!(args.headers[0].1, "Bearer csk_test");
        assert!(
            Cli::try_parse_from([
                "crucible-loadgen",
                "run",
                "--rate",
                "1",
                "--header",
                "nocolon"
            ])
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_cpu_reads_this_process() {
        let (user, system) = process_cpu().unwrap();
        assert!(user >= 0.0 && system >= 0.0);
    }
}
