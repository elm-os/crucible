//! The driver loop: every case of the suite through one runner, evaluated against its golden, collected into a
//! matrix report (JSON lines and Markdown).

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::time::Duration;

use async_trait::async_trait;
use crucible_env::{ClockExt as _, Env, NetError};
use serde::Serialize;

use crate::collector::Collector;
use crate::golden::{CaseInput, Finding, evaluate};
use crate::spec::{Endpoints, ResponseEvent, RunSpec, run_spec};
use crate::suite::{Manifest, Suite, Variant};

/// How a runner played one case.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOutcome {
    /// The response events it reported.
    pub responses: Vec<ResponseEvent>,
    /// Its exit code; `None` when it did not exit by itself.
    pub exit_code: Option<i32>,
    /// Why it failed to start, crashed or was killed.
    pub error: Option<String>,
    /// The end of its stdout, other than events.
    pub stdout_tail: String,
    /// The end of its stderr.
    pub stderr_tail: String,
}

/// Plays one run spec: the runner contract of `docs/testing/sdk-conformance.md`.
#[async_trait]
pub trait Runner: fmt::Debug + Send + Sync {
    /// Plays `spec` to the end, including the SDK's shutdown.
    async fn run(&self, spec: &RunSpec) -> RunOutcome;
}

/// Driver settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Run only cases whose id equals one of these or starts with `<value>@`; empty runs every case.
    pub cases: Vec<String>,
    /// Per-case limit on the runner.
    pub timeout: Duration,
    /// How long the collector must stay quiet after the runner exits.
    pub settle: Duration,
    /// The longest wait for quiet.
    pub settle_cap: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cases: Vec::new(),
            timeout: Duration::from_secs(60),
            settle: Duration::from_millis(200),
            settle_cap: Duration::from_secs(2),
        }
    }
}

/// A case's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Every check passed.
    Pass,
    /// A check failed.
    Fail,
    /// The manifest skips the case.
    Skip,
    /// The runner failed or timed out.
    Error,
}

impl Status {
    /// The report name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
            Self::Error => "error",
        }
    }
}

/// How the runner ended, for the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunnerSummary {
    /// Exit code.
    pub exit_code: Option<i32>,
    /// Wall time of the case in milliseconds.
    pub millis: u64,
    /// Why the runner failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The end of its stdout.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stdout_tail: String,
    /// The end of its stderr.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stderr_tail: String,
}

/// One line of the JSON-lines report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaseResult {
    /// The manifest's `sdk`.
    pub sdk: String,
    /// The case id.
    pub case: String,
    /// The scenario id.
    pub scenario: String,
    /// The variant of an MCP case.
    pub variant: Option<String>,
    /// The result.
    pub status: Status,
    /// Checks evaluated.
    pub checks: u32,
    /// Checks failed.
    pub failed: usize,
    /// The failed checks.
    pub failures: Vec<Finding>,
    /// Why the case was skipped.
    pub skip_reason: Option<String>,
    /// How the runner ended.
    pub runner: Option<RunnerSummary>,
}

/// Why the driver could not run.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The collector could not bind a loopback port.
    #[error("the collector could not listen on 127.0.0.1: {0}")]
    Collector(#[from] NetError),
    /// A case could not be resolved.
    #[error("case {case}: {message}")]
    Case {
        /// The case id.
        case: String,
        /// Why.
        message: String,
    },
    /// The `--case` filters matched nothing.
    #[error("no case matches {0:?}; case ids look like mcp-echo@legacy-stdio or agent-context")]
    NoCase(Vec<String>),
}

/// The report of one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The manifest's `sdk`.
    pub sdk: String,
    /// One result per case, in suite order.
    pub results: Vec<CaseResult>,
}

/// Runs the suite's cases through `runner` and evaluates each.
pub async fn run_suite(
    env: &Env,
    suite: &Suite,
    manifest: &Manifest,
    runner: &dyn Runner,
    options: &Options,
) -> Result<Report, DriverError> {
    let cases: Vec<_> = suite
        .cases()
        .into_iter()
        .filter(|case| {
            options.cases.is_empty()
                || options.cases.iter().any(|f| {
                    case.id == *f
                        || case
                            .id
                            .strip_prefix(f.as_str())
                            .is_some_and(|rest| rest.starts_with('@'))
                })
        })
        .collect();
    if cases.is_empty() {
        return Err(DriverError::NoCase(options.cases.clone()));
    }
    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        let mut result = CaseResult {
            sdk: manifest.sdk.clone(),
            case: case.id.clone(),
            scenario: case.scenario.id.clone(),
            variant: case.variant.map(|v| v.name().to_owned()),
            status: Status::Skip,
            checks: 0,
            failed: 0,
            failures: Vec::new(),
            skip_reason: manifest.skip_reason(case),
            runner: None,
        };
        if result.skip_reason.is_some() {
            results.push(result);
            continue;
        }
        // A collector per case, so spans a slow sender exports after its case has ended cannot reach the next one.
        let collector = Collector::start(env).await?;
        let endpoints = Endpoints {
            endpoint: collector.endpoint(),
            traces_url: collector.traces_url(),
            checkpoint_url: collector.checkpoint_url(),
            api_url: collector.endpoint(),
        };
        let spec =
            run_spec(&suite.constants, case, &endpoints).map_err(|message| DriverError::Case {
                case: case.id.clone(),
                message,
            })?;
        let started = env.clock.now();
        let outcome = env
            .clock
            .timeout(options.timeout, runner.run(&spec))
            .await
            .unwrap_or_else(|_| RunOutcome {
                error: Some(format!(
                    "the runner did not finish within {} s and was killed",
                    options.timeout.as_secs()
                )),
                ..RunOutcome::default()
            });
        settle(env, &collector, options).await;
        let millis = env
            .clock
            .now()
            .saturating_duration_since(started)
            .as_millis();
        let events = collector.take();
        let evaluation = evaluate(&CaseInput {
            suite,
            case,
            manifest,
            events: &events,
            responses: &outcome.responses,
        });
        let runner_failed = outcome.exit_code != Some(0) || outcome.error.is_some();
        result.status = if runner_failed {
            Status::Error
        } else if evaluation.failures.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        };
        result.checks = evaluation.checks;
        result.failed = evaluation.failures.len();
        result.failures = evaluation.failures;
        result.runner = Some(RunnerSummary {
            exit_code: outcome.exit_code,
            millis: u64::try_from(millis).unwrap_or(u64::MAX),
            error: outcome.error,
            stdout_tail: outcome.stdout_tail,
            stderr_tail: outcome.stderr_tail,
        });
        results.push(result);
    }
    Ok(Report {
        sdk: manifest.sdk.clone(),
        results,
    })
}

/// Waits until the collector's log has not grown for `options.settle`, at most `options.settle_cap`.
async fn settle(env: &Env, collector: &Collector, options: &Options) {
    let start = env.clock.now();
    let mut quiet_since = start;
    let mut seen = collector.len();
    let tick = options
        .settle
        .min(Duration::from_millis(50))
        .max(Duration::from_millis(1));
    loop {
        let now = env.clock.now();
        if now.saturating_duration_since(quiet_since) >= options.settle
            || now.saturating_duration_since(start) >= options.settle_cap
        {
            return;
        }
        env.clock.sleep(tick).await;
        let len = collector.len();
        if len != seen {
            seen = len;
            quiet_since = env.clock.now();
        }
    }
}

impl Report {
    /// Whether no case failed or errored.
    pub fn passed(&self) -> bool {
        self.results
            .iter()
            .all(|r| matches!(r.status, Status::Pass | Status::Skip))
    }

    /// Count of results with `status`.
    pub fn count(&self, status: Status) -> usize {
        self.results.iter().filter(|r| r.status == status).count()
    }

    /// One JSON object per line.
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for result in &self.results {
            if let Ok(line) = serde_json::to_string(result) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        out
    }

    /// The Markdown matrix and the failure list.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let checks: u32 = self.results.iter().map(|r| r.checks).sum();
        let _ = writeln!(out, "# SDK conformance: {}\n", self.sdk);
        let _ = writeln!(
            out,
            "{} cases: {} passed, {} failed, {} errors, {} skipped ({checks} checks).\n",
            self.results.len(),
            self.count(Status::Pass),
            self.count(Status::Fail),
            self.count(Status::Error),
            self.count(Status::Skip),
        );
        let mut mcp: BTreeMap<&str, BTreeMap<&str, &CaseResult>> = BTreeMap::new();
        let mut agent: Vec<&CaseResult> = Vec::new();
        for r in &self.results {
            match &r.variant {
                Some(v) => {
                    mcp.entry(&r.scenario).or_default().insert(v, r);
                }
                None => agent.push(r),
            }
        }
        if !mcp.is_empty() {
            out.push_str("| MCP scenario |");
            for v in Variant::ALL {
                let _ = write!(out, " {} |", v.name());
            }
            out.push_str("\n|---|");
            out.push_str(&"---|".repeat(Variant::ALL.len()));
            out.push('\n');
            for (scenario, cells) in &mcp {
                let _ = write!(out, "| {scenario} |");
                for v in Variant::ALL {
                    let cell = cells.get(v.name()).map_or("", |r| cell(r));
                    let _ = write!(out, " {cell} |");
                }
                out.push('\n');
            }
            out.push('\n');
        }
        if !agent.is_empty() {
            out.push_str("| Agent scenario | result |\n|---|---|\n");
            for r in &agent {
                let _ = writeln!(out, "| {} | {} |", r.scenario, cell(r));
            }
            out.push('\n');
        }
        let failing: Vec<&CaseResult> = self
            .results
            .iter()
            .filter(|r| matches!(r.status, Status::Fail | Status::Error))
            .collect();
        if !failing.is_empty() {
            out.push_str("## Failures\n\n");
            for r in failing {
                let _ = writeln!(out, "### {}\n", r.case);
                if let Some(runner) = &r.runner {
                    if let Some(error) = &runner.error {
                        let _ = writeln!(out, "- runner: {error}");
                    } else if runner.exit_code != Some(0) {
                        let _ = writeln!(out, "- runner exited with {:?}", runner.exit_code);
                    }
                    if !runner.stderr_tail.is_empty() {
                        let _ = writeln!(
                            out,
                            "- runner stderr ends with: `{}`",
                            one_line(&runner.stderr_tail)
                        );
                    }
                }
                for f in &r.failures {
                    let span = f
                        .span
                        .as_deref()
                        .map_or(String::new(), |s| format!("`{s}` "));
                    let contract = f
                        .contract
                        .as_deref()
                        .map_or(String::new(), |c| format!(" (contract {c})"));
                    let _ = writeln!(
                        out,
                        "- {span}{}: expected {}, found {}{contract}",
                        f.check,
                        one_line(&f.expected),
                        one_line(&f.actual)
                    );
                }
                out.push('\n');
            }
        }
        let skipped: Vec<&CaseResult> = self
            .results
            .iter()
            .filter(|r| r.status == Status::Skip)
            .collect();
        if !skipped.is_empty() {
            out.push_str("## Skipped\n\n");
            for r in skipped {
                let _ = writeln!(
                    out,
                    "- {}: {}",
                    r.case,
                    r.skip_reason.as_deref().unwrap_or("")
                );
            }
        }
        out
    }
}

fn cell(r: &CaseResult) -> &'static str {
    match r.status {
        Status::Pass => "pass",
        Status::Fail => "**fail**",
        Status::Skip => "skip",
        Status::Error => "**error**",
    }
}

fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.replace('|', "\\|");
    match flat.char_indices().nth(240) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}
