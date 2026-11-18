//! `crucible-sdk-driver`: runs the SDK conformance suite (`docs/testing/sdk-conformance.md`). This file builds the
//! production `Env`, reads the suite and manifest files, spawns runner processes and writes the reports; it is the
//! only place in the crate that touches those ambient APIs.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use clap::{Args, Parser, Subcommand};
use crucible_env_prod::ProdConfig;
use crucible_fake_collector::suite::{ProgressCount, ThrowOutcome};
use crucible_fake_collector::{
    Manifest, Mutant, Options, RunOutcome, RunSpec, Runner, RunnerEvent, SelfTestOptions,
    SelfTestRunner, Suite, run_suite,
};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

/// The most stdout and stderr bytes kept for the report.
const TAIL_BYTES: usize = 4096;
/// The program name that means this executable in a manifest's `runner`.
const SELF: &str = "crucible-sdk-driver";

#[derive(Debug, Parser)]
#[command(
    name = "crucible-sdk-driver",
    version,
    about = "Runs the SDK conformance suite against an SDK's runner"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run every case through the manifest's runner and write <out>/<sdk>.jsonl and <out>/<sdk>.md.
    Run(RunArgs),
    /// The self-test runner: read a run spec on stdin and emit its spans directly.
    SelftestRunner(SelftestArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// The SDK's capability manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// The suite directory.
    #[arg(long, default_value = "conformance")]
    conformance: PathBuf,
    /// Where the reports go.
    #[arg(long, default_value = "target/sdk-conformance")]
    out: PathBuf,
    /// Run only this case, or every variant of this scenario; repeatable.
    #[arg(long = "case")]
    cases: Vec<String>,
    /// Per-case limit in seconds; the runner is killed when it passes.
    #[arg(long, default_value_t = 60)]
    timeout_secs: u64,
    /// A runner command that replaces the manifest's `runner`.
    #[arg(last = true)]
    runner: Vec<String>,
}

#[derive(Debug, Args)]
struct SelftestArgs {
    /// What a tool handler's exception becomes.
    #[arg(long, default_value = "internal", value_parser = ["internal", "tool_error"])]
    throw_outcome: String,
    /// Whether progress notifications are counted.
    #[arg(long, default_value = "observed", value_parser = ["observed", "unobserved"])]
    progress_count: String,
    /// A seeded defect the driver must flag: unscrubbed-secret.
    #[cfg(feature = "test-mutants")]
    #[arg(long)]
    mutant: Option<String>,
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Run(args) => run(args),
        Command::SelftestRunner(args) => selftest(&args),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "crucible-sdk-driver: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("sdk-driver")
        .build()?)
}

fn run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let suite = Suite::from_files(read_suite(&args.conformance)?)?;
    let manifest_name = args.manifest.display().to_string();
    let manifest = Manifest::parse(&manifest_name, &read(&args.manifest)?)?;
    manifest.check_against(&manifest_name, &suite)?;
    let argv = if args.runner.is_empty() {
        manifest.runner.clone()
    } else {
        args.runner.clone()
    };
    let runtime = runtime()?;
    let prod = crucible_env_prod::build(runtime.handle().clone(), &ProdConfig::default())?;
    let runner = ProcessRunner { argv };
    let options = Options {
        cases: args.cases.clone(),
        timeout: Duration::from_secs(args.timeout_secs),
        ..Options::default()
    };
    let report = runtime.block_on(run_suite(&prod.env, &suite, &manifest, &runner, &options))?;
    let markdown = report.to_markdown();
    write(
        &args.out,
        &format!("{}.jsonl", report.sdk),
        &report.to_jsonl(),
    )?;
    write(&args.out, &format!("{}.md", report.sdk), &markdown)?;
    std::io::stdout().write_all(markdown.as_bytes())?;
    Ok(if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn selftest(args: &SelftestArgs) -> anyhow::Result<ExitCode> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let spec: RunSpec = serde_json::from_str(&input)
        .map_err(|e| anyhow::anyhow!("stdin is not a run spec: {e}"))?;
    #[cfg(feature = "test-mutants")]
    let mutant = match &args.mutant {
        None => None,
        Some(name) => Some(
            Mutant::parse(name)
                .ok_or_else(|| anyhow::anyhow!("unknown mutant {name:?}; try unscrubbed-secret"))?,
        ),
    };
    #[cfg(not(feature = "test-mutants"))]
    let mutant: Option<Mutant> = None;
    let options = SelfTestOptions {
        throw_outcome: if args.throw_outcome == "tool_error" {
            ThrowOutcome::ToolError
        } else {
            ThrowOutcome::Internal
        },
        progress_count: if args.progress_count == "unobserved" {
            ProgressCount::Unobserved
        } else {
            ProgressCount::Observed
        },
        mutant,
    };
    let runtime = runtime()?;
    let prod = crucible_env_prod::build(runtime.handle().clone(), &ProdConfig::default())?;
    let runner = SelfTestRunner::new(prod.env, options)?;
    match runtime.block_on(runner.play(&spec)) {
        Ok(events) => {
            let mut stdout = std::io::stdout().lock();
            for event in events {
                writeln!(
                    stdout,
                    "{}",
                    serde_json::to_string(&RunnerEvent::Response(event))?
                )?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            writeln!(std::io::stderr(), "selftest-runner: {error}")?;
            Ok(ExitCode::from(1))
        }
    }
}

#[allow(clippy::disallowed_methods)]
fn read(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// Every JSON file under `dir`, with its path relative to `dir` using '/' separators.
#[allow(clippy::disallowed_methods)]
fn read_suite(dir: &Path) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current).map_err(|e| {
            anyhow::anyhow!(
                "{}: {e}; pass --conformance with the suite directory",
                current.display()
            )
        })?;
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "json") {
                let relative = path
                    .strip_prefix(dir)?
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                files.push((relative, read(&path)?));
            }
        }
    }
    files.sort();
    Ok(files)
}

#[allow(clippy::disallowed_methods)]
fn write(dir: &Path, name: &str, contents: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("{}: {e}", dir.display()))?;
    let path = dir.join(name);
    std::fs::write(&path, contents).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// Runs a runner command once per case (the runner contract's invocation).
#[derive(Debug)]
struct ProcessRunner {
    argv: Vec<String>,
}

/// Keeps the last [`TAIL_BYTES`] of `text`, cut on a character boundary.
fn tail(text: &str) -> String {
    if text.len() <= TAIL_BYTES {
        return text.to_owned();
    }
    let mut start = text.len() - TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[async_trait]
impl Runner for ProcessRunner {
    #[allow(clippy::disallowed_methods)]
    async fn run(&self, spec: &RunSpec) -> RunOutcome {
        let Some((program, args)) = self.argv.split_first() else {
            return RunOutcome {
                error: Some("the runner command is empty".to_owned()),
                ..RunOutcome::default()
            };
        };
        let program = if program == SELF {
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from(program))
        } else {
            PathBuf::from(program)
        };
        let input = match serde_json::to_vec(spec) {
            Ok(input) => input,
            Err(e) => {
                return RunOutcome {
                    error: Some(format!("the run spec does not serialize: {e}")),
                    ..RunOutcome::default()
                };
            }
        };
        let mut child = match tokio::process::Command::new(&program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                return RunOutcome {
                    error: Some(format!("cannot start {}: {e}", program.display())),
                    ..RunOutcome::default()
                };
            }
        };
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let feed = async move {
            if let Some(mut stdin) = stdin {
                // A runner that exits without reading its stdin closes the pipe; that is its own error.
                let _ = stdin.write_all(&input).await;
                let _ = stdin.shutdown().await;
            }
        };
        let events = async move {
            let mut responses = Vec::new();
            let mut noise = String::new();
            if let Some(stdout) = stdout {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    match serde_json::from_str::<RunnerEvent>(&line) {
                        Ok(RunnerEvent::Response(event)) => responses.push(event),
                        Err(_) => {
                            noise.push_str(&line);
                            noise.push('\n');
                            if noise.len() > 2 * TAIL_BYTES {
                                noise = tail(&noise);
                            }
                        }
                    }
                }
            }
            (responses, tail(&noise))
        };
        let errors = async move {
            let mut bytes = Vec::new();
            if let Some(mut stderr) = stderr {
                let _ = stderr.read_to_end(&mut bytes).await;
            }
            tail(&String::from_utf8_lossy(&bytes))
        };
        let ((), (responses, stdout_tail), stderr_tail) = tokio::join!(feed, events, errors);
        let (exit_code, error) = match child.wait().await {
            Ok(status) => match status.code() {
                Some(code) => (Some(code), None),
                None => (
                    None,
                    Some("the runner was terminated by a signal".to_owned()),
                ),
            },
            Err(e) => (None, Some(format!("waiting for the runner failed: {e}"))),
        };
        RunOutcome {
            responses,
            exit_code,
            error,
            stdout_tail,
            stderr_tail,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_runner_override_takes_everything_after_the_separator() {
        let cli = Cli::parse_from([
            "crucible-sdk-driver",
            "run",
            "--manifest",
            "m.json",
            "--case",
            "mcp-echo",
            "--",
            "node",
            "runner.mjs",
            "--flag",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(args.runner, ["node", "runner.mjs", "--flag"]);
        assert_eq!(args.cases, ["mcp-echo"]);
        assert_eq!(args.conformance, PathBuf::from("conformance"));
    }

    #[test]
    fn tails_keep_the_end_on_a_character_boundary() {
        let text = format!("{}\u{00e9}{}", "a".repeat(10), "b".repeat(TAIL_BYTES - 1));
        let kept = tail(&text);
        assert!(kept.len() <= TAIL_BYTES && kept.ends_with('b'));
        assert_eq!(tail("short"), "short");
    }
}
