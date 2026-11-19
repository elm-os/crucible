//! The A/B gate's pure parts: the interleaving order, the judgement of one arm, the decision rule, the
//! JSON report (schema [`GATE_SCHEMA`]) and its markdown summary. `docs/testing/perf.md` documents them.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::rig::{Context, Stored};
use super::stats::{SignedRank, Tail, median, signed_rank};

/// The `schema` value of [`GateReport`].
pub const GATE_SCHEMA: &str = "crucible-perf.gate.v1";
/// The label every gate result carries.
pub const LABEL: &str = "measured on a laptop, not reference hardware";
/// A throughput drop beyond this share of the base median can block.
pub const THROUGHPUT_DROP: f64 = 0.05;
/// A p99 rise beyond this share of the base median can block.
pub const P99_RISE: f64 = 0.10;
/// A change blocks only with a one-sided p-value below this.
pub const ALPHA: f64 = 0.05;
/// A throughput arm measures capacity only when it acknowledged less than this share of the offered spans
/// per second; otherwise the offered load did not exceed what the build can take.
pub const SATURATED_BELOW: f64 = 0.9;
/// The `context` of the commit status.
pub const STATUS_CONTEXT: &str = "perf-gate";

/// One of the two builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Arm {
    /// The base build.
    A,
    /// The candidate build.
    B,
}

/// The order of the arms in pair `pair` (from 0): AB, BA, AB, ..., so that slow drift does not favour an arm.
pub fn order(pair: usize) -> [Arm; 2] {
    if pair.is_multiple_of(2) {
        [Arm::A, Arm::B]
    } else {
        [Arm::B, Arm::A]
    }
}

/// The order of every pair as text, such as `["AB", "BA"]`.
pub fn order_names(pairs: usize) -> Vec<String> {
    (0..pairs)
        .map(|pair| order(pair).iter().map(|arm| format!("{arm:?}")).collect())
        .collect()
}

/// One arm of a session, each on a fresh rig.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A discarded latency arm of a build, before the pairs.
    Warmup(Arm),
    /// Measurement `kind` of a build in pair `pair` (from 0).
    Measure {
        /// The pair, from 0.
        pair: usize,
        /// The measurement.
        kind: Kind,
        /// The build.
        arm: Arm,
    },
}

/// The arms of a session in run order: a discarded latency arm of each build, then each pair build by build, the
/// path's capacity measurement before its latency one. So every capacity arm follows a latency arm and every
/// latency arm follows the capacity arm of its own build, whichever build runs first in the pair: neither
/// position follows heavier work than the other (a capacity arm writes gigabytes, and its rig's removal follows),
/// and neither build's image starts cold in the first measured arm.
pub fn schedule(pairs: usize, kinds: [Kind; 2]) -> Vec<Step> {
    let mut steps: Vec<Step> = order(0).into_iter().map(Step::Warmup).collect();
    for pair in 0..pairs {
        for arm in order(pair) {
            for kind in kinds {
                steps.push(Step::Measure { pair, kind, arm });
            }
        }
    }
    steps
}

/// The measurements of the gate: two per path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Ack path: offered load above capacity; the metric is acknowledged spans per second.
    Throughput,
    /// Ack path: a fixed rate below capacity; the metric is `latency_us.p99`.
    Latency,
    /// End to end: offered load above what the store path keeps up with for a fixed time, then the drain; the
    /// metric is acknowledged spans per second over the run and its drain.
    Sustained,
    /// End to end: a fixed rate below capacity; the metric is the freshness p95 of the run's spans.
    Freshness,
}

impl Kind {
    /// Whether the measurement reads the store after the run.
    pub fn end_to_end(self) -> bool {
        matches!(self, Self::Sustained | Self::Freshness)
    }

    /// Whether the metric measures capacity (higher is better) rather than latency.
    pub fn capacity(self) -> bool {
        matches!(self, Self::Throughput | Self::Sustained)
    }

    /// The name in the report and the status.
    pub fn name(self) -> &'static str {
        match self {
            Self::Throughput => "throughput",
            Self::Latency => "latency",
            Self::Sustained => "sustained",
            Self::Freshness => "freshness",
        }
    }
}

/// The fields of a `crucible-loadgen.run.v1` report that the gate reads.
#[derive(Clone, Debug, Deserialize)]
pub struct LoadgenReport {
    /// `crucible-loadgen.run.v1`.
    pub schema: String,
    /// What happened.
    pub outcome: LoadgenOutcome,
    /// Intended send time to response, over 2xx responses.
    pub latency_us: LoadgenSummary,
}

/// `outcome` of a load generator report.
#[derive(Clone, Debug, Deserialize)]
pub struct LoadgenOutcome {
    /// Acknowledged spans per second over the elapsed time.
    pub acked_spans_per_s: f64,
    /// Responses by HTTP status code.
    pub status_counts: BTreeMap<String, u64>,
    /// Requests without a usable response.
    pub errors: LoadgenErrors,
}

/// `outcome.errors` of a load generator report.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct LoadgenErrors {
    /// Not connected, so not sent.
    pub connect: u64,
    /// No response within the timeout.
    pub timeout: u64,
    /// The connection failed.
    pub transport: u64,
    /// A 2xx body that does not decode.
    pub bad_response: u64,
}

/// A latency summary of a load generator report.
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct LoadgenSummary {
    /// 99th percentile, in microseconds.
    pub p99: u64,
}

/// The load generator's report schema the gate reads.
pub const RUN_SCHEMA: &str = "crucible-loadgen.run.v1";

impl LoadgenReport {
    /// Requests that got no usable response.
    pub fn errors(&self) -> u64 {
        let e = self.outcome.errors;
        e.connect + e.timeout + e.transport + e.bad_response
    }

    /// Responses outside 2xx.
    pub fn non_2xx(&self) -> u64 {
        self.outcome
            .status_counts
            .iter()
            .filter(|(code, _)| !code.starts_with('2') || code.len() != 3)
            .map(|(_, count)| count)
            .sum()
    }
}

/// One measured arm.
#[derive(Clone, Debug, Serialize)]
pub struct Run {
    /// The pair, from 1.
    pub pair: usize,
    /// Which measurement.
    pub kind: Kind,
    /// Which build.
    pub arm: Arm,
    /// The load generator's report, relative to the results directory.
    pub report: String,
    /// The crucible log, relative to the results directory.
    pub log: String,
    /// The spans per second the schedule offered.
    pub offered_spans_per_s: f64,
    /// `outcome.acked_spans_per_s`.
    pub acked_spans_per_s: f64,
    /// `latency_us.p99`.
    pub latency_p99_us: u64,
    /// Requests without a usable response.
    pub errors: u64,
    /// Responses outside 2xx.
    pub non_2xx: u64,
    /// Why this arm cannot be compared; `None` when it can.
    pub invalid: Option<String>,
    /// What else the machine did during the measurement.
    pub context: Option<Context>,
    /// What the store held after an end-to-end measurement.
    pub stored: Option<Stored>,
}

impl Run {
    /// The arm's value of its metric.
    pub fn value(&self) -> f64 {
        match self.kind {
            Kind::Throughput => self.acked_spans_per_s,
            Kind::Latency => self.latency_p99_us as f64,
            Kind::Sustained => self.stored.as_ref().map_or(0.0, |s| s.shipped_spans_per_s),
            Kind::Freshness => self
                .stored
                .as_ref()
                .map_or(0.0, |s| s.freshness_p95_us as f64),
        }
    }
}

/// Of an end-to-end arm's time (run plus drain), the least share the drain after the last response must take:
/// below it the store kept up with what was acknowledged, so the ack path or the generator set the rate.
pub const DRAIN_SHARE_AT_LEAST: f64 = 0.1;

/// Why an arm's measurement cannot be compared, or `None`. `stopped` lists the rig containers that were
/// not running after the measurement; `stored` is what the store held after an end-to-end measurement.
pub fn judge(
    kind: Kind,
    report: &LoadgenReport,
    offered_spans_per_s: f64,
    stopped: &[String],
    stored: Option<&Stored>,
) -> Option<String> {
    let mut reasons = Vec::new();
    if !stopped.is_empty() {
        reasons.push(format!("stopped during the run: {}", stopped.join(", ")));
    }
    let not_clean = || {
        let (errors, non_2xx) = (report.errors(), report.non_2xx());
        (errors > 0 || non_2xx > 0).then(|| {
            format!(
                "not clean: {errors} requests failed and {non_2xx} responses were not 2xx, so the measurement \
                 does not cover them"
            )
        })
    };
    let saturated = |value: f64, what: &str| {
        (value >= SATURATED_BELOW * offered_spans_per_s).then(|| {
            format!(
                "not saturated: {what} {} of {} offered spans/s, so the offered load did not exceed the build's \
                 capacity",
                thousands(value),
                thousands(offered_spans_per_s)
            )
        })
    };
    match kind {
        Kind::Throughput => {
            reasons.extend(saturated(report.outcome.acked_spans_per_s, "acknowledged"))
        }
        Kind::Latency => reasons.extend(not_clean()),
        Kind::Sustained | Kind::Freshness => match stored {
            None => reasons.push("the store was not read after the run".to_owned()),
            Some(stored) => {
                if stored.lost_spans > 0 || stored.duplicate_spans > 0 {
                    reasons.push(format!(
                        "zero loss failed (invariant I1): {} acknowledged spans missing, {} stored twice",
                        stored.lost_spans, stored.duplicate_spans
                    ));
                }
                match (stored.drained_s, stored.drain_after_run_s) {
                    (Some(total), Some(after)) => {
                        if kind == Kind::Sustained {
                            reasons.extend(saturated(stored.shipped_spans_per_s, "shipped"));
                            if after < DRAIN_SHARE_AT_LEAST * total {
                                reasons.push(format!(
                                    "the store kept up: the drain took {after:.1} s of {total:.1} s after the \
                                     last response, so the ack path or the generator set the rate"
                                ));
                            }
                        }
                    }
                    _ => reasons.push(format!(
                        "not drained: the store did not hold all {} acknowledged spans within the drain limit",
                        stored.acked_spans
                    )),
                }
                if kind == Kind::Freshness {
                    reasons.extend(not_clean());
                    if stored.freshness_spans < stored.acked_spans {
                        reasons.push(format!(
                            "freshness covers {} of {} acknowledged spans",
                            stored.freshness_spans, stored.acked_spans
                        ));
                    }
                }
            }
        },
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

/// Which way a metric improves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Better {
    /// Throughput.
    Higher,
    /// Latency.
    Lower,
}

/// The comparison of one metric across the pairs.
#[derive(Clone, Debug, Serialize)]
pub struct Comparison {
    /// `throughput` or `latency_p99`.
    pub metric: Kind,
    /// The unit of the values.
    pub unit: &'static str,
    /// Which way the metric improves.
    pub better: Better,
    /// The base build's value in each pair.
    pub base: Vec<f64>,
    /// The candidate's value in each pair.
    pub candidate: Vec<f64>,
    /// Medians of the two lists.
    pub median_base: f64,
    /// See `median_base`.
    pub median_candidate: f64,
    /// `median_candidate / median_base - 1`; `None` when the base median is 0.
    pub change: Option<f64>,
    /// `candidate - base` in each pair.
    pub differences: Vec<f64>,
    /// The exact one-sided signed-rank test of the differences, in the direction of a regression.
    pub wilcoxon: SignedRank,
    /// The signed change beyond which, with p below [`ALPHA`], the metric blocks.
    pub threshold: f64,
    /// In each pair, the value of the arm that ran second minus the value of the arm that ran first.
    pub order_differences: Vec<f64>,
    /// The order effect relative to the median of every value: the mean of the median second-minus-first
    /// difference of the AB pairs and that of the BA pairs, in which the builds' difference cancels. `None`
    /// without pairs of both orders, or when the median is 0.
    pub order_change: Option<f64>,
    /// Whether this metric may block.
    pub gated: bool,
    /// Whether every arm could be compared.
    pub valid: bool,
    /// Why the metric is not gated or not valid.
    pub notes: Vec<String>,
    /// Whether the metric blocks the merge.
    pub blocks: bool,
}

/// Whether a valid, gated metric with relative `change` and one-sided `p` blocks.
pub fn blocks(better: Better, change: Option<f64>, p: f64) -> bool {
    let Some(change) = change else { return false };
    p < ALPHA
        && match better {
            Better::Higher => change < -THROUGHPUT_DROP,
            Better::Lower => change > P99_RISE,
        }
}

/// Compares the runs of `kind`: the base's and the candidate's value per pair, their medians, the per-pair
/// differences, the exact one-sided signed-rank test, and the decision rule. `not_gated` says why the
/// metric may not block, when it may not.
pub fn compare(kind: Kind, runs: &[Run], not_gated: Option<String>) -> Comparison {
    let of = |arm: Arm| -> Vec<&Run> {
        let mut runs: Vec<&Run> = runs
            .iter()
            .filter(|r| r.kind == kind && r.arm == arm)
            .collect();
        runs.sort_by_key(|r| r.pair);
        runs
    };
    let (a, b) = (of(Arm::A), of(Arm::B));
    let base: Vec<f64> = a.iter().map(|r| r.value()).collect();
    let candidate: Vec<f64> = b.iter().map(|r| r.value()).collect();
    let differences: Vec<f64> = base.iter().zip(&candidate).map(|(a, b)| b - a).collect();
    let (better, unit, tail, threshold) = if kind.capacity() {
        (Better::Higher, "spans/s", Tail::Lower, -THROUGHPUT_DROP)
    } else {
        (Better::Lower, "us", Tail::Upper, P99_RISE)
    };
    let wilcoxon = signed_rank(&differences, tail);
    let median_base = median(&base).unwrap_or(0.0);
    let median_candidate = median(&candidate).unwrap_or(0.0);
    let change = (median_base != 0.0).then(|| median_candidate / median_base - 1.0);
    // In an AB pair, second minus first is the builds' difference plus the order effect; in a BA pair it is the
    // order effect minus the builds' difference. Averaging the two orders cancels the builds' difference.
    let (mut order_differences, mut ab, mut ba) = (Vec::new(), Vec::new(), Vec::new());
    for (ra, rb) in a.iter().zip(&b) {
        let (first, second, group) = match order(ra.pair.saturating_sub(1)) {
            [Arm::A, _] => (ra, rb, &mut ab),
            _ => (rb, ra, &mut ba),
        };
        let difference = second.value() - first.value();
        order_differences.push(difference);
        group.push(difference);
    }
    let all: Vec<f64> = base.iter().chain(&candidate).copied().collect();
    let order_change = match (median(&ab), median(&ba), median(&all)) {
        (Some(ab), Some(ba), Some(level)) if level != 0.0 => Some((ab + ba) / 2.0 / level),
        _ => None,
    };
    let mut notes: Vec<String> = not_gated.iter().cloned().collect();
    if order_change.is_some_and(|c| c.abs() > threshold.abs()) {
        notes.push(format!(
            "order effect: the arms that ran second measured {} against those that ran first; an order effect \
             of this size can hide or fake a change of the same size",
            percent(order_change)
        ));
    }
    for run in a.iter().chain(&b) {
        if let Some(reason) = &run.invalid {
            notes.push(format!("pair {} arm {:?}: {reason}", run.pair, run.arm));
        }
    }
    let valid = a.iter().chain(&b).all(|r| r.invalid.is_none()) && base.len() == candidate.len();
    let gated = not_gated.is_none();
    Comparison {
        metric: kind,
        unit,
        better,
        base,
        candidate,
        median_base,
        median_candidate,
        change,
        differences,
        wilcoxon,
        threshold,
        order_differences,
        order_change,
        gated,
        valid,
        notes,
        blocks: gated && valid && blocks(better, change, wilcoxon.p),
    }
}

/// The result of a gate run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// No metric blocks and every comparison held.
    Pass,
    /// A valid, gated metric blocks.
    Regression,
    /// No metric blocks, but some comparison did not hold, so the run proves nothing.
    Invalid,
}

/// The verdict of a set of comparisons.
pub fn verdict(comparisons: &[Comparison]) -> Verdict {
    if comparisons.iter().any(|c| c.blocks) {
        Verdict::Regression
    } else if comparisons.iter().any(|c| !c.valid) {
        Verdict::Invalid
    } else {
        Verdict::Pass
    }
}

/// A build under test.
#[derive(Clone, Debug, Serialize)]
pub struct Build {
    /// The ref as given.
    pub reference: String,
    /// The commit it resolved to.
    pub sha: String,
    /// The image tag.
    pub image: String,
    /// The image id.
    pub image_id: String,
}

/// The AC-power state of the machine, checked before the run.
#[derive(Clone, Debug, Serialize)]
pub struct Power {
    /// `online`, `battery` or `unknown`.
    pub state: String,
    /// Where the state came from, or why it is unknown.
    pub source: String,
}

/// One container's share of the rig.
#[derive(Clone, Debug, Serialize)]
pub struct Container {
    /// `crucible`, `clickhouse`, `postgres` or `loadgen`.
    pub role: &'static str,
    /// The image.
    pub image: String,
    /// `--cpuset-cpus`.
    pub cpuset: &'static str,
    /// `--memory`.
    pub memory: &'static str,
}

/// The rig as run.
#[derive(Clone, Debug, Serialize)]
pub struct Rig {
    /// `full` or `small`.
    pub name: &'static str,
    /// The containers, each started fresh for every arm except the load generator.
    pub containers: Vec<Container>,
    /// `docker info`: server version, CPUs, memory, kernel and operating system.
    pub docker: String,
}

/// One lane of the run.
#[derive(Clone, Debug, Serialize)]
pub struct LaneReport {
    /// `1kb`, `10kb`, `1kb-gzip` or `10kb-gzip`.
    pub name: &'static str,
    /// `ack` or `e2e`.
    pub path: &'static str,
    /// `--span-size`.
    pub span_size: &'static str,
    /// `--spans-per-request`.
    pub spans_per_request: u32,
    /// `--gzip`.
    pub gzip: bool,
    /// `--concurrency`.
    pub concurrency: u32,
    /// Seconds of discarded load at the latency rate before each measurement.
    pub warmup_s: u64,
    /// The throughput measurement's requests per second and seconds.
    pub throughput_requests_per_s: u64,
    /// See `throughput_requests_per_s`.
    pub throughput_s: u64,
    /// The latency measurement's requests per second and seconds.
    pub latency_requests_per_s: u64,
    /// See `latency_requests_per_s`.
    pub latency_s: u64,
    /// The sustained measurement's requests per second and seconds.
    pub sustained_requests_per_s: u64,
    /// See `sustained_requests_per_s`.
    pub sustained_s: u64,
    /// The freshness measurement's requests per second and seconds.
    pub freshness_requests_per_s: u64,
    /// See `freshness_requests_per_s`.
    pub freshness_s: u64,
    /// The longest an end-to-end arm waits for its drain.
    pub drain_s: u64,
}

/// The fsync-honesty probe of this session.
#[derive(Clone, Debug, Serialize)]
pub struct Probe {
    /// `honest`, `suspicious`, or `failed` when the probe did not run.
    pub verdict: String,
    /// The probe's report, relative to the results directory, when it ran.
    pub report: Option<String>,
    /// Why the probe failed.
    pub error: Option<String>,
}

/// The JSON report of a gate run (schema [`GATE_SCHEMA`]).
#[derive(Clone, Debug, Serialize)]
pub struct GateReport {
    /// Always [`GATE_SCHEMA`].
    pub schema: &'static str,
    /// Always [`LABEL`].
    pub label: &'static str,
    /// A small-rig run: never posted, never gates a merge.
    pub smoke: bool,
    /// When the run started and finished, UTC.
    pub started_at: String,
    /// See `started_at`.
    pub finished_at: String,
    /// The AC-power state.
    pub ac_power: Power,
    /// The rig.
    pub rig: Rig,
    /// The lane.
    pub lane: LaneReport,
    /// Build A.
    pub base: Build,
    /// Build B.
    pub candidate: Build,
    /// SHA-256 of the load generator binary that measured both arms.
    pub loadgen_sha256: String,
    /// Interleaved pairs.
    pub pairs: usize,
    /// Each pair's order of arms.
    pub order: Vec<String>,
    /// The WAL volume's classification.
    pub probe: Probe,
    /// Throughput, then latency.
    pub comparisons: Vec<Comparison>,
    /// Every measured arm, in the order run.
    pub runs: Vec<Run>,
    /// The result.
    pub verdict: Verdict,
}

/// The `gh` arguments that set the commit status of `report` on the candidate commit.
pub fn status_args(report: &GateReport) -> Vec<String> {
    let state = match report.verdict {
        Verdict::Pass => "success",
        Verdict::Regression => "failure",
        Verdict::Invalid => "error",
    };
    vec![
        "api".to_owned(),
        "--method".to_owned(),
        "POST".to_owned(),
        format!("repos/{{owner}}/{{repo}}/statuses/{}", report.candidate.sha),
        "-f".to_owned(),
        format!("state={state}"),
        "-f".to_owned(),
        format!("context={STATUS_CONTEXT}"),
        "-f".to_owned(),
        format!("description={}", status_description(report)),
    ]
}

/// At most 140 characters (the commit status limit): the verdict and each metric's change and p-value.
pub fn status_description(report: &GateReport) -> String {
    let verdict = match report.verdict {
        Verdict::Pass => "no regression",
        Verdict::Regression => "regression",
        Verdict::Invalid => "invalid comparison",
    };
    let metrics: Vec<String> = report
        .comparisons
        .iter()
        .map(|c| {
            let name = match c.metric {
                Kind::Latency => "p99",
                Kind::Freshness => "freshness p95",
                other => other.name(),
            };
            let gated = if c.gated { "" } else { " not gated" };
            format!("{name} {} p={:.3}{gated}", percent(c.change), c.wilcoxon.p)
        })
        .collect();
    let text = format!(
        "{verdict}: {}; {} pairs, {} {}",
        metrics.join(", "),
        report.pairs,
        report.lane.path,
        report.lane.name
    );
    text.chars().take(140).collect()
}

/// `x` rounded to an integer with thousands separators.
pub fn thousands(x: f64) -> String {
    let digits = format!("{:.0}", x.abs());
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if x < 0.0 && digits != "0" {
        out.insert(0, '-');
    }
    out
}

/// A signed percentage with one decimal, or `n/a`.
pub fn percent(change: Option<f64>) -> String {
    change.map_or_else(|| "n/a".to_owned(), |c| format!("{:+.1}%", c * 100.0))
}

fn milliseconds(us: f64) -> String {
    format!("{:.2}", us / 1000.0)
}

fn metric_value(kind: Kind, x: f64) -> String {
    if kind.capacity() {
        thousands(x)
    } else {
        milliseconds(x)
    }
}

/// A metric's column heading and, in the metric table, its name and the rule that blocks it.
fn heading(kind: Kind) -> (&'static str, &'static str, &'static str) {
    match kind {
        Kind::Throughput => (
            "Throughput",
            "Throughput, acknowledged spans/s",
            "it drops more than 5% with p < 0.05",
        ),
        Kind::Latency => (
            "p99, ms",
            "Durable-ack p99, ms",
            "it rises more than 10% with p < 0.05",
        ),
        Kind::Sustained => (
            "Sustained",
            "Sustained, shipped spans/s",
            "it drops more than 5% with p < 0.05",
        ),
        Kind::Freshness => (
            "Freshness p95, ms",
            "Freshness p95, ms",
            "it rises more than 10% with p < 0.05",
        ),
    }
}

fn signed_metric_value(kind: Kind, x: f64) -> String {
    let text = metric_value(kind, x.abs());
    if x < 0.0 {
        format!("-{text}")
    } else {
        format!("+{text}")
    }
}

/// The markdown summary of `report`.
pub fn summary(report: &GateReport) -> String {
    let mut out = String::new();
    let title = match report.verdict {
        Verdict::Pass => "no regression",
        Verdict::Regression => "regression",
        Verdict::Invalid => "invalid comparison",
    };
    let smoke = if report.smoke { " (smoke)" } else { "" };
    let _ = writeln!(out, "# Perf gate: {title}{smoke}\n");
    let _ = writeln!(out, "Measured on a laptop, not reference hardware.");
    if report.smoke {
        let floor = 1.0 / (report.pairs as f64).exp2();
        let _ = writeln!(
            out,
            "\nSmoke run on the small rig: labelled smoke, never posted, never gates a merge. It checks the \
             plumbing only: with {} pairs the smallest possible one-sided p-value is {floor:.4}{}.",
            report.pairs,
            if floor >= ALPHA {
                ", so this run cannot block"
            } else {
                ""
            }
        );
    }
    let lane = &report.lane;
    let spans = |rps: u64| thousands((rps * u64::from(lane.spans_per_request)) as f64);
    let containers: Vec<String> = report
        .rig
        .containers
        .iter()
        .map(|c| format!("{} CPUs {} {}", c.role, c.cpuset, c.memory))
        .collect();
    let _ = writeln!(out, "\n| Setting | Value |\n|---|---|");
    for (name, value) in [
        (
            "Base (A)".to_owned(),
            format!("`{}` at `{}`", report.base.reference, report.base.sha),
        ),
        (
            "Candidate (B)".to_owned(),
            format!(
                "`{}` at `{}`",
                report.candidate.reference, report.candidate.sha
            ),
        ),
        (
            "Lane".to_owned(),
            format!(
                "{}: spans of {}, {} per request, {} protobuf, {} connections",
                lane.name,
                lane.span_size.replace('k', " KiB"),
                lane.spans_per_request,
                if lane.gzip { "gzipped" } else { "uncompressed" },
                lane.concurrency
            ),
        ),
        ("Path".to_owned(), lane.path.to_owned()),
        (
            "Measurements".to_owned(),
            match lane.path {
                "e2e" => format!(
                    "sustained: {} requests/s offered ({} spans/s) for {} s, then the drain (at most {} s); \
                     acknowledged spans/s over the run and its drain. Freshness: {} requests/s ({} spans/s) for {} \
                     s; p95 from `received_at` to readable. Both check zero loss",
                    thousands(lane.sustained_requests_per_s as f64),
                    spans(lane.sustained_requests_per_s),
                    lane.sustained_s,
                    lane.drain_s,
                    thousands(lane.freshness_requests_per_s as f64),
                    spans(lane.freshness_requests_per_s),
                    lane.freshness_s
                ),
                _ => format!(
                    "throughput: {} requests/s offered ({} spans/s) for {} s; acknowledged spans/s. Latency: {} \
                     requests/s ({} spans/s) for {} s; `latency_us.p99`",
                    thousands(lane.throughput_requests_per_s as f64),
                    spans(lane.throughput_requests_per_s),
                    lane.throughput_s,
                    thousands(lane.latency_requests_per_s as f64),
                    spans(lane.latency_requests_per_s),
                    lane.latency_s
                ),
            },
        ),
        (
            "Pairs".to_owned(),
            format!(
                "{} in the order {}, after a discarded latency arm of each build; in a pair each build runs its \
                 capacity measurement, then its latency one; every arm on a fresh rig after {} s of warm-up",
                report.pairs,
                report.order.join(", "),
                lane.warmup_s
            ),
        ),
        (
            "Rig".to_owned(),
            format!("{}: {}", report.rig.name, containers.join("; ")),
        ),
        ("Docker".to_owned(), report.rig.docker.clone()),
        (
            "AC power".to_owned(),
            format!("{} ({})", report.ac_power.state, report.ac_power.source),
        ),
        (
            "WAL volume".to_owned(),
            match &report.probe.error {
                None => format!("{} (fsync probe in this session)", report.probe.verdict),
                Some(error) => format!("unknown: the fsync probe failed ({error})"),
            },
        ),
        (
            "Load generator".to_owned(),
            format!("sha256 `{}`", report.loadgen_sha256),
        ),
        (
            "Run".to_owned(),
            format!("{} to {}", report.started_at, report.finished_at),
        ),
    ] {
        let _ = writeln!(out, "| {name} | {value} |");
    }

    let _ = writeln!(
        out,
        "\n| Metric | Base median | Candidate median | Change | One-sided p | Second vs first | Blocks when | Gated | \
         Result |\n|---|---|---|---|---|---|---|---|---|"
    );
    for c in &report.comparisons {
        let (_, name, rule) = heading(c.metric);
        let result = if c.blocks {
            "regression"
        } else if !c.valid {
            "invalid"
        } else if !c.gated {
            "reported, not gated"
        } else {
            "no regression"
        };
        let _ = writeln!(
            out,
            "| {name} | {} | {} | {} | {:.4} | {} | {rule} | {} | {result} |",
            metric_value(c.metric, c.median_base),
            metric_value(c.metric, c.median_candidate),
            percent(c.change),
            c.wilcoxon.p,
            percent(c.order_change),
            if c.gated { "yes" } else { "no" }
        );
    }
    let notes: Vec<&String> = report.comparisons.iter().flat_map(|c| &c.notes).collect();
    if !notes.is_empty() {
        let _ = writeln!(out);
        for note in notes {
            let _ = writeln!(out, "- {note}");
        }
    }

    // The path's two measurements, capacity first.
    let kinds: Vec<Kind> = report.comparisons.iter().map(|c| c.metric).collect();
    let (first, second) = match kinds[..] {
        [first, second] => (first, second),
        _ => (Kind::Throughput, Kind::Latency),
    };
    let (h1, h2) = (heading(first).0, heading(second).0);
    let _ = writeln!(
        out,
        "\n## Pairs\n\n| Pair | Order | {h1} A | {h1} B | Difference | {h2} A | {h2} B | Difference |\n\
         |---|---|---|---|---|---|---|---|"
    );
    let row = |kind: Kind, pair: usize| -> [String; 3] {
        let value = |arm: Arm| {
            report
                .runs
                .iter()
                .find(|r| r.kind == kind && r.arm == arm && r.pair == pair)
                .map(Run::value)
        };
        match (value(Arm::A), value(Arm::B)) {
            (Some(a), Some(b)) => [
                metric_value(kind, a),
                metric_value(kind, b),
                signed_metric_value(kind, b - a),
            ],
            _ => ["n/a".to_owned(), "n/a".to_owned(), "n/a".to_owned()],
        }
    };
    for (pair, order) in (1..).zip(&report.order) {
        let [ta, tb, td] = row(first, pair);
        let [la, lb, ld] = row(second, pair);
        let _ = writeln!(
            out,
            "| {pair} | {order} | {ta} | {tb} | {td} | {la} | {lb} | {ld} |"
        );
    }
    let _ = writeln!(
        out,
        "\n## Runs in order\n\nBusy cores are averaged over each measurement. Other jobs' containers have no cpuset, \
         so they can also run on the rig's CPUs, and they share its disk: disk busy is the share of the measurement \
         during which the busiest block device had I/O in flight. After a page-cache drop on the host, reads go to the \
         disk.\n\n| Run | Pair | Measurement | Arm | Value | Rig CPUs busy | Other CPUs busy | Disk busy | Page-cache \
         drops | Load before, after | Other containers |\n\
         |---|---|---|---|---|---|---|---|---|---|---|"
    );
    for (index, run) in (1..).zip(&report.runs) {
        let (kind, value) = (heading(run.kind).0, metric_value(run.kind, run.value()));
        let context = run.context.as_ref().map_or_else(
            || "n/a | n/a | n/a | n/a | n/a | n/a".to_owned(),
            |c| {
                format!(
                    "{:.1} | {:.1} | {:.0}% | {} | {:.1}, {:.1} | {}",
                    c.rig_cpus_busy,
                    c.other_cpus_busy,
                    c.disk_busy * 100.0,
                    c.page_cache_drops,
                    c.load_before,
                    c.load_after,
                    c.others.len()
                )
            },
        );
        let _ = writeln!(
            out,
            "| {index} | {} | {kind} | {:?} | {value} | {context} |",
            run.pair, run.arm
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn run(pair: usize, kind: Kind, arm: Arm, value: f64) -> Run {
        Run {
            pair,
            kind,
            arm,
            report: format!("runs/pair-{pair}-{kind:?}-{arm:?}.json"),
            log: String::new(),
            offered_spans_per_s: 1_000_000.0,
            acked_spans_per_s: if kind == Kind::Throughput { value } else { 1.0 },
            latency_p99_us: if kind == Kind::Latency {
                value as u64
            } else {
                1
            },
            errors: 0,
            non_2xx: 0,
            invalid: None,
            context: None,
            stored: None,
        }
    }

    /// Runs of `kind` with the base's and the candidate's values per pair.
    fn runs(kind: Kind, base: &[f64], candidate: &[f64]) -> Vec<Run> {
        let mut runs = Vec::new();
        for (pair, (a, b)) in (1..).zip(base.iter().zip(candidate)) {
            runs.push(run(pair, kind, Arm::A, *a));
            runs.push(run(pair, kind, Arm::B, *b));
        }
        runs
    }

    fn report(base: &[f64], candidate: &[f64]) -> GateReport {
        let mut all = runs(Kind::Throughput, base, candidate);
        all.extend(runs(Kind::Latency, &[8000.0; 5], &[8100.0; 5]));
        let comparisons = vec![
            compare(Kind::Throughput, &all, None),
            compare(
                Kind::Latency,
                &all,
                Some("the probe said suspicious".to_owned()),
            ),
        ];
        GateReport {
            schema: GATE_SCHEMA,
            label: LABEL,
            smoke: false,
            started_at: "2026-11-18T21:00:00Z".to_owned(),
            finished_at: "2026-11-18T21:40:00Z".to_owned(),
            ac_power: Power {
                state: "online".to_owned(),
                source: "test".to_owned(),
            },
            rig: Rig {
                name: "full",
                containers: vec![Container {
                    role: "crucible",
                    image: "crucible-gate:b".to_owned(),
                    cpuset: "0-5",
                    memory: "3g",
                }],
                docker: "29.8.2".to_owned(),
            },
            lane: LaneReport {
                name: "1kb",
                path: "ack",
                span_size: "1k",
                spans_per_request: 100,
                gzip: false,
                concurrency: 128,
                warmup_s: 3,
                throughput_requests_per_s: 20_000,
                throughput_s: 10,
                latency_requests_per_s: 1_000,
                latency_s: 30,
                sustained_requests_per_s: 1_500,
                sustained_s: 20,
                freshness_requests_per_s: 300,
                freshness_s: 30,
                drain_s: 300,
            },
            base: Build {
                reference: "main".to_owned(),
                sha: "a".repeat(40),
                image: "crucible-gate:a".to_owned(),
                image_id: "sha256:1".to_owned(),
            },
            candidate: Build {
                reference: "HEAD".to_owned(),
                sha: "b".repeat(40),
                image: "crucible-gate:b".to_owned(),
                image_id: "sha256:2".to_owned(),
            },
            loadgen_sha256: "c".repeat(64),
            pairs: base.len(),
            order: order_names(base.len()),
            probe: Probe {
                verdict: "suspicious".to_owned(),
                report: Some("probe.json".to_owned()),
                error: None,
            },
            verdict: verdict(&comparisons),
            comparisons,
            runs: all,
        }
    }

    #[test]
    fn pairs_alternate_ab_ba() {
        assert_eq!(order_names(5), ["AB", "BA", "AB", "BA", "AB"]);
        assert_eq!(order(0), [Arm::A, Arm::B]);
        assert_eq!(order(1), [Arm::B, Arm::A]);
        let measure = |pair, kind, arm| Step::Measure { pair, kind, arm };
        assert_eq!(
            schedule(2, [Kind::Throughput, Kind::Latency]),
            [
                Step::Warmup(Arm::A),
                Step::Warmup(Arm::B),
                measure(0, Kind::Throughput, Arm::A),
                measure(0, Kind::Latency, Arm::A),
                measure(0, Kind::Throughput, Arm::B),
                measure(0, Kind::Latency, Arm::B),
                measure(1, Kind::Throughput, Arm::B),
                measure(1, Kind::Latency, Arm::B),
                measure(1, Kind::Throughput, Arm::A),
                measure(1, Kind::Latency, Arm::A),
            ]
        );
    }

    proptest! {
        /// Every pair runs each arm once; consecutive pairs swap the order, so over an even count each arm
        /// goes first equally often and over an odd count A goes first once more.
        #[test]
        fn interleaving_is_balanced(pairs in 1usize..200) {
            let mut a_first = 0;
            for pair in 0..pairs {
                let [first, second] = order(pair);
                prop_assert_ne!(first, second);
                if pair > 0 {
                    prop_assert_eq!(order(pair - 1)[0], second);
                }
                a_first += usize::from(first == Arm::A);
            }
            prop_assert_eq!(a_first, pairs.div_ceil(2));
        }

        /// Each build warms up once before any measurement, every measurement of every pair runs once, every
        /// capacity arm follows a latency arm (or a warm-up, which measures latency), and every latency arm
        /// follows the capacity arm of its own build and pair: what precedes an arm never depends on its position.
        #[test]
        fn every_arm_follows_the_same_kind_of_work(pairs in 1usize..60, e2e: bool) {
            let kinds = if e2e { [Kind::Sustained, Kind::Freshness] } else { [Kind::Throughput, Kind::Latency] };
            let steps = schedule(pairs, kinds);
            prop_assert_eq!(&steps[..2], &[Step::Warmup(Arm::A), Step::Warmup(Arm::B)]);
            prop_assert_eq!(steps.len(), 2 + 4 * pairs);
            let mut seen = std::collections::BTreeSet::new();
            for (index, step) in steps.iter().enumerate().skip(2) {
                let Step::Measure { pair, kind, arm } = *step else {
                    return Err(TestCaseError::fail("a warm-up after the first measurement"));
                };
                let first_time = seen.insert((pair, kind.name(), arm == Arm::A));
                prop_assert!(first_time);
                let previous = steps[index - 1];
                if kind == kinds[0] {
                    let after_latency = match previous {
                        Step::Warmup(_) => true,
                        Step::Measure { kind: before, .. } => before == kinds[1],
                    };
                    prop_assert!(after_latency, "{:?} follows {:?}", step, previous);
                } else {
                    prop_assert_eq!(previous, Step::Measure { pair, kind: kinds[0], arm });
                }
            }
            prop_assert_eq!(seen.len(), 4 * pairs);
        }
    }

    #[test]
    fn the_decision_rule_is_strict_at_every_boundary() {
        // Throughput blocks on a drop of more than 5% with p below 0.05.
        assert!(blocks(Better::Higher, Some(-0.0501), 0.0499));
        assert!(!blocks(Better::Higher, Some(-0.05), 0.01));
        assert!(!blocks(Better::Higher, Some(-0.20), 0.05));
        assert!(!blocks(Better::Higher, Some(0.20), 0.01));
        // p99 blocks on a rise of more than 10% with p below 0.05.
        assert!(blocks(Better::Lower, Some(0.1001), 0.03125));
        assert!(!blocks(Better::Lower, Some(0.10), 0.01));
        assert!(!blocks(Better::Lower, Some(0.50), 0.0625));
        assert!(!blocks(Better::Lower, Some(-0.50), 0.01));
        // No relative change without a base median.
        assert!(!blocks(Better::Higher, None, 0.0));
    }

    #[test]
    fn a_consistent_throughput_drop_over_five_pairs_blocks() {
        let base = [100.0, 102.0, 98.0, 101.0, 99.0];
        let down: Vec<f64> = base.iter().map(|x| x * 0.9).collect();
        let c = compare(
            Kind::Throughput,
            &runs(Kind::Throughput, &base, &down),
            None,
        );
        assert_eq!(c.wilcoxon.p, 1.0 / 32.0);
        assert!((c.change.unwrap() + 0.1).abs() < 1e-9);
        assert!(c.blocks && c.valid && c.gated);
        assert_eq!(verdict(&[c]), Verdict::Regression);
        // The same drop in four of five pairs does not reach p < 0.05.
        let mut mixed = down.clone();
        mixed[2] = base[2] + 1.0;
        let c = compare(
            Kind::Throughput,
            &runs(Kind::Throughput, &base, &mixed),
            None,
        );
        assert!(c.wilcoxon.p >= ALPHA && !c.blocks);
        assert_eq!(verdict(&[c]), Verdict::Pass);
    }

    #[test]
    fn latency_gates_only_when_allowed_and_clean() {
        let base = [8000.0, 8100.0, 7900.0, 8050.0, 7950.0];
        let up: Vec<f64> = base.iter().map(|x| x * 1.2).collect();
        let all = runs(Kind::Latency, &base, &up);
        let gated = compare(Kind::Latency, &all, None);
        assert!(gated.blocks);
        let suspicious = compare(
            Kind::Latency,
            &all,
            Some("the probe classified the WAL volume suspicious".to_owned()),
        );
        assert!(!suspicious.blocks && !suspicious.gated && suspicious.valid);
        assert_eq!(verdict(&[suspicious]), Verdict::Pass);
        let mut unclean = all.clone();
        unclean[3].invalid = Some("not clean".to_owned());
        let c = compare(Kind::Latency, &unclean, None);
        assert!(!c.valid && !c.blocks);
        assert_eq!(c.notes, ["pair 2 arm B: not clean"]);
        assert_eq!(verdict(&[c]), Verdict::Invalid);
    }

    #[test]
    fn a_valid_regression_outranks_an_invalid_comparison() {
        let base = [100.0; 5];
        let down = [80.0, 81.0, 79.0, 82.0, 78.0];
        let thr = compare(
            Kind::Throughput,
            &runs(Kind::Throughput, &base, &down),
            None,
        );
        let mut lat = runs(Kind::Latency, &[1.0; 5], &[1.0; 5]);
        lat[0].invalid = Some("not clean".to_owned());
        let lat = compare(Kind::Latency, &lat, None);
        assert_eq!(verdict(&[thr, lat]), Verdict::Regression);
    }

    #[test]
    fn the_order_effect_compares_the_arms_that_ran_second_with_those_that_ran_first() {
        // One build in both arms, and in every pair the arm that ran second measured about 10% more. Pairs run
        // AB, BA, AB, BA, so A ran first in pairs 1 and 3.
        let base = [100.0, 112.2, 98.0, 111.1];
        let candidate = [110.0, 102.0, 107.8, 101.0];
        let c = compare(
            Kind::Throughput,
            &runs(Kind::Throughput, &base, &candidate),
            None,
        );
        assert_eq!(c.order_differences.len(), 4);
        for (difference, expected) in c.order_differences.iter().zip([10.0, 10.2, 9.8, 10.1]) {
            assert!((difference - expected).abs() < 1e-9, "{difference}");
        }
        // The AB pairs' median difference is 9.9 and the BA pairs' 10.15; the median of every value is 104.9.
        let order = c.order_change.unwrap();
        assert!((order - 10.025 / 104.9).abs() < 1e-9, "{order}");
        // The arms' medians barely differ, so nothing blocks, but the note names the order effect.
        assert!(c.change.unwrap().abs() < 0.01 && !c.blocks);
        assert!(
            c.notes.iter().any(|n| n.starts_with("order effect: ")),
            "{:?}",
            c.notes
        );
        // Without an order effect there is no note.
        let flat = compare(
            Kind::Throughput,
            &runs(Kind::Throughput, &[100.0; 4], &[100.0; 4]),
            None,
        );
        assert_eq!(flat.order_change, Some(0.0));
        assert!(flat.notes.is_empty());
    }

    #[test]
    fn a_zero_base_median_never_blocks() {
        let c = compare(
            Kind::Throughput,
            &runs(Kind::Throughput, &[0.0; 5], &[0.0; 5]),
            None,
        );
        assert_eq!((c.change, c.blocks, c.wilcoxon.p), (None, false, 1.0));
    }

    fn loadgen(acked: f64, statuses: &[(&str, u64)], timeouts: u64) -> LoadgenReport {
        LoadgenReport {
            schema: RUN_SCHEMA.to_owned(),
            outcome: LoadgenOutcome {
                acked_spans_per_s: acked,
                status_counts: statuses
                    .iter()
                    .map(|(code, n)| ((*code).to_owned(), *n))
                    .collect(),
                errors: LoadgenErrors {
                    connect: 0,
                    timeout: timeouts,
                    transport: 0,
                    bad_response: 0,
                },
            },
            latency_us: LoadgenSummary { p99: 9000 },
        }
    }

    #[test]
    fn arms_are_judged_by_their_measurement() {
        let offered = 1_000_000.0;
        // Throughput: refusals and timeouts are expected; only saturation matters.
        let busy = loadgen(600_000.0, &[("200", 900), ("429", 100)], 3);
        assert_eq!(judge(Kind::Throughput, &busy, offered, &[], None), None);
        let idle = loadgen(899_999.0, &[("200", 1000)], 0);
        assert_eq!(judge(Kind::Throughput, &idle, offered, &[], None), None);
        let kept_up = loadgen(900_000.0, &[("200", 1000)], 0);
        assert!(
            judge(Kind::Throughput, &kept_up, offered, &[], None)
                .unwrap()
                .starts_with("not saturated")
        );
        // Latency: every request must succeed with 2xx.
        let clean = loadgen(10.0, &[("200", 1000)], 0);
        assert_eq!(judge(Kind::Latency, &clean, offered, &[], None), None);
        assert_eq!((busy.errors(), busy.non_2xx()), (3, 100));
        assert!(judge(Kind::Latency, &busy, offered, &[], None).is_some());
        let refused = loadgen(10.0, &[("200", 999), ("503", 1)], 0);
        assert!(judge(Kind::Latency, &refused, offered, &[], None).is_some());
        // A rig container that stopped invalidates any arm.
        let stopped = ["crucible (OOM killed)".to_owned()];
        assert_eq!(
            judge(Kind::Latency, &clean, offered, &stopped, None).unwrap(),
            "stopped during the run: crucible (OOM killed)"
        );
    }

    #[test]
    fn end_to_end_arms_are_judged_by_what_the_store_held() {
        let offered = 100_000.0;
        let clean = loadgen(0.0, &[("200", 1000)], 0);
        let stored = |shipped: f64, drained: Option<f64>, after: f64, lost: u64| Stored {
            acked_spans: 1_000_000,
            drained_s: drained,
            drain_after_run_s: drained.map(|_| after),
            shipped_spans_per_s: shipped,
            freshness_p95_us: 900_000,
            freshness_spans: 1_000_000,
            lost_spans: lost,
            duplicate_spans: 0,
        };
        let judged = |kind, report: &LoadgenReport, s: &Stored| {
            judge(kind, report, offered, &[], Some(s)).unwrap_or_default()
        };
        // Half the offered rate, with half the time spent draining the backlog: comparable either way.
        let good = stored(50_000.0, Some(20.0), 10.0, 0);
        assert_eq!(judged(Kind::Sustained, &clean, &good), "");
        assert_eq!(judged(Kind::Freshness, &clean, &good), "");
        // The store kept pace with the acknowledgements, so something else set the rate.
        let kept_up = stored(50_000.0, Some(20.0), 1.0, 0);
        assert!(judged(Kind::Sustained, &clean, &kept_up).starts_with("the store kept up"));
        // The offered load was within the end-to-end capacity.
        let idle = stored(95_000.0, Some(10.5), 5.0, 0);
        assert!(judged(Kind::Sustained, &clean, &idle).starts_with("not saturated"));
        // No drain within the limit, or a lost span.
        let stuck = stored(0.0, None, 0.0, 0);
        assert!(judged(Kind::Freshness, &clean, &stuck).starts_with("not drained"));
        let lost = stored(50_000.0, Some(20.0), 10.0, 3);
        assert!(judged(Kind::Sustained, &clean, &lost).starts_with("zero loss failed"));
        // Freshness must cover every acknowledged span of a clean run.
        let partial = Stored {
            freshness_spans: 10,
            ..good.clone()
        };
        assert!(judged(Kind::Freshness, &clean, &partial).contains("freshness covers 10 of"));
        let refused = loadgen(0.0, &[("200", 999), ("503", 1)], 0);
        assert!(judged(Kind::Freshness, &refused, &good).starts_with("not clean"));
        assert!(
            judge(Kind::Sustained, &clean, offered, &[], None)
                .unwrap()
                .contains("not read")
        );
        // The value of an end-to-end arm comes from the store.
        let mut arm = run(1, Kind::Sustained, Arm::A, 0.0);
        arm.stored = Some(good);
        assert_eq!(arm.value(), 50_000.0);
        arm.kind = Kind::Freshness;
        assert_eq!(arm.value(), 900_000.0);
    }

    #[test]
    fn the_load_generator_report_parses_from_its_json() {
        let json = r#"{"schema":"crucible-loadgen.run.v1","run_id":"x","seed":1,
            "outcome":{"sent":10,"responses":10,"acked":9,"acked_spans":900,"acked_spans_per_s":1234.5,
            "status_counts":{"200":9,"503":1},"errors":{"connect":0,"timeout":2,"transport":1,"bad_response":0}},
            "latency_us":{"count":9,"p50":1,"p90":2,"p99":3,"p99_9":4,"max":5}}"#;
        let report: LoadgenReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.schema, RUN_SCHEMA);
        assert_eq!(
            (report.errors(), report.non_2xx(), report.latency_us.p99),
            (3, 1, 3)
        );
    }

    #[test]
    fn the_report_has_the_documented_fields() {
        let base = [100.0, 102.0, 98.0, 101.0, 99.0];
        let value = serde_json::to_value(report(&base, &base)).unwrap();
        assert_eq!(value["schema"], GATE_SCHEMA);
        assert_eq!(value["label"], LABEL);
        assert_eq!(value["verdict"], "pass");
        for field in [
            "smoke",
            "started_at",
            "finished_at",
            "ac_power",
            "rig",
            "lane",
            "base",
            "candidate",
            "loadgen_sha256",
            "pairs",
            "order",
            "probe",
            "comparisons",
            "runs",
        ] {
            assert!(value.get(field).is_some(), "missing {field}");
        }
        let throughput = &value["comparisons"][0];
        assert_eq!(throughput["metric"], "throughput");
        assert_eq!(throughput["better"], "higher");
        assert_eq!(throughput["wilcoxon"]["p"], 1.0);
        for field in [
            "unit",
            "base",
            "candidate",
            "median_base",
            "median_candidate",
            "change",
            "differences",
            "threshold",
            "gated",
            "valid",
            "notes",
            "blocks",
        ] {
            assert!(
                throughput.get(field).is_some(),
                "missing comparisons[].{field}"
            );
        }
        assert_eq!(value["comparisons"][1]["metric"], "latency");
        assert_eq!(value["comparisons"][1]["gated"], false);
        let run = &value["runs"][0];
        assert_eq!(
            (run["kind"].clone(), run["arm"].clone()),
            ("throughput".into(), "A".into())
        );
    }

    #[test]
    fn the_summary_carries_the_label_the_stamps_and_no_em_dash() {
        let base = [100.0, 102.0, 98.0, 101.0, 99.0];
        let mut r = report(&base, &base);
        let text = summary(&r);
        assert!(text.starts_with("# Perf gate: no regression\n"));
        assert!(text.contains("Measured on a laptop, not reference hardware."));
        assert!(text.contains("| AC power | online (test) |"));
        assert!(text.contains("| WAL volume | suspicious (fsync probe in this session) |"));
        assert!(text.contains("reported, not gated"));
        assert!(text.contains("| 5 | AB | 99 | 99 | +0 |"));
        assert!(!text.contains('\u{2014}'));
        r.smoke = true;
        r.pairs = 2;
        let text = summary(&r);
        assert!(text.starts_with("# Perf gate: no regression (smoke)\n"));
        assert!(
            text.contains(
                "smallest possible one-sided p-value is 0.2500, so this run cannot block"
            )
        );
    }

    #[test]
    fn the_status_names_the_candidate_and_fits_its_limit() {
        let base = [100.0, 102.0, 98.0, 101.0, 99.0];
        let down: Vec<f64> = base.iter().map(|x| x * 0.9).collect();
        let r = report(&base, &down);
        assert_eq!(r.verdict, Verdict::Regression);
        let args = status_args(&r);
        let path = format!("repos/{{owner}}/{{repo}}/statuses/{}", "b".repeat(40));
        assert_eq!(
            args[..8],
            [
                "api",
                "--method",
                "POST",
                path.as_str(),
                "-f",
                "state=failure",
                "-f",
                "context=perf-gate",
            ]
        );
        assert_eq!(
            args[9],
            "description=regression: throughput -10.0% p=0.031, p99 +1.2% p=0.031 not gated; 5 pairs, ack 1kb"
        );
        assert!(status_description(&r).chars().count() <= 140);
    }

    #[test]
    fn numbers_format_for_people() {
        assert_eq!(thousands(1_234_567.4), "1,234,567");
        assert_eq!(thousands(999.6), "1,000");
        assert_eq!(thousands(-12_345.0), "-12,345");
        assert_eq!(thousands(0.2), "0");
        assert_eq!(percent(Some(-0.0512)), "-5.1%");
        assert_eq!(percent(None), "n/a");
    }
}
