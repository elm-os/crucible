//! The A/B gate's pure parts: the interleaving order, the judgement of one arm, the decision rule, the
//! JSON report (schema [`GATE_SCHEMA`]) and its markdown summary. `docs/testing/perf.md` documents them.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::rig::Context;
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

/// The two measurements of the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Offered load above capacity; the metric is acknowledged spans per second.
    Throughput,
    /// A fixed rate below capacity; the metric is `latency_us.p99`.
    Latency,
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
}

impl Run {
    /// The arm's value of its metric.
    pub fn value(&self) -> f64 {
        match self.kind {
            Kind::Throughput => self.acked_spans_per_s,
            Kind::Latency => self.latency_p99_us as f64,
        }
    }
}

/// Why an arm's measurement cannot be compared, or `None`. `stopped` lists the rig containers that were
/// not running after the measurement.
pub fn judge(
    kind: Kind,
    report: &LoadgenReport,
    offered_spans_per_s: f64,
    stopped: &[String],
) -> Option<String> {
    let mut reasons = Vec::new();
    if !stopped.is_empty() {
        reasons.push(format!("stopped during the run: {}", stopped.join(", ")));
    }
    match kind {
        Kind::Throughput => {
            if report.outcome.acked_spans_per_s >= SATURATED_BELOW * offered_spans_per_s {
                reasons.push(format!(
                    "not saturated: acknowledged {} of {} offered spans/s, so the offered load did not exceed \
                     the build's capacity",
                    thousands(report.outcome.acked_spans_per_s),
                    thousands(offered_spans_per_s)
                ));
            }
        }
        Kind::Latency => {
            let (errors, non_2xx) = (report.errors(), report.non_2xx());
            if errors > 0 || non_2xx > 0 {
                reasons.push(format!(
                    "not clean: {errors} requests failed and {non_2xx} responses were not 2xx, so latency_us \
                     does not cover them"
                ));
            }
        }
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
    let (better, unit, tail, threshold) = match kind {
        Kind::Throughput => (Better::Higher, "spans/s", Tail::Lower, -THROUGHPUT_DROP),
        Kind::Latency => (Better::Lower, "us", Tail::Upper, P99_RISE),
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
    /// `1kb` or `10kb`.
    pub name: &'static str,
    /// `--span-size`.
    pub span_size: &'static str,
    /// `--spans-per-request`.
    pub spans_per_request: u32,
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
                Kind::Throughput => "throughput",
                Kind::Latency => "p99",
            };
            let gated = if c.gated { "" } else { " not gated" };
            format!("{name} {} p={:.3}{gated}", percent(c.change), c.wilcoxon.p)
        })
        .collect();
    let text = format!(
        "{verdict}: {}; {} pairs, {}",
        metrics.join(", "),
        report.pairs,
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
    match kind {
        Kind::Throughput => thousands(x),
        Kind::Latency => milliseconds(x),
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
                "{}: spans of {}, {} per request, uncompressed protobuf, {} connections",
                lane.name,
                lane.span_size.replace('k', " KiB"),
                lane.spans_per_request,
                lane.concurrency
            ),
        ),
        (
            "Throughput".to_owned(),
            format!(
                "{} requests/s offered ({} spans/s) for {} s; acknowledged spans/s",
                thousands(lane.throughput_requests_per_s as f64),
                spans(lane.throughput_requests_per_s),
                lane.throughput_s
            ),
        ),
        (
            "Latency".to_owned(),
            format!(
                "{} requests/s ({} spans/s) for {} s; `latency_us.p99`",
                thousands(lane.latency_requests_per_s as f64),
                spans(lane.latency_requests_per_s),
                lane.latency_s
            ),
        ),
        (
            "Pairs".to_owned(),
            format!(
                "{} in the order {}; every arm on a fresh rig after {} s of warm-up",
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
        let (name, rule) = match c.metric {
            Kind::Throughput => (
                "Throughput, acknowledged spans/s",
                "it drops more than 5% with p < 0.05",
            ),
            Kind::Latency => (
                "Durable-ack p99, ms",
                "it rises more than 10% with p < 0.05",
            ),
        };
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

    let _ = writeln!(
        out,
        "\n## Pairs\n\n| Pair | Order | Throughput A | Throughput B | Difference | p99 A, ms | p99 B, ms | \
         Difference |\n|---|---|---|---|---|---|---|---|"
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
        let [ta, tb, td] = row(Kind::Throughput, pair);
        let [la, lb, ld] = row(Kind::Latency, pair);
        let _ = writeln!(
            out,
            "| {pair} | {order} | {ta} | {tb} | {td} | {la} | {lb} | {ld} |"
        );
    }
    let _ = writeln!(
        out,
        "\n## Runs in order\n\nBusy cores are averaged over each measurement. Other jobs' containers have no cpuset, \
         so they can also run on the rig's CPUs.\n\n| Run | Pair | Measurement | Arm | Value | Rig CPUs busy | \
         Other CPUs busy | Load before, after | Other containers |\n|---|---|---|---|---|---|---|---|---|"
    );
    for (index, run) in (1..).zip(&report.runs) {
        let (kind, value) = match run.kind {
            Kind::Throughput => ("throughput", thousands(run.value())),
            Kind::Latency => ("p99, ms", milliseconds(run.value())),
        };
        let context = run.context.as_ref().map_or_else(
            || "n/a | n/a | n/a | n/a".to_owned(),
            |c| {
                format!(
                    "{:.1} | {:.1} | {:.1}, {:.1} | {}",
                    c.rig_cpus_busy,
                    c.other_cpus_busy,
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
                span_size: "1k",
                spans_per_request: 100,
                concurrency: 128,
                warmup_s: 3,
                throughput_requests_per_s: 20_000,
                throughput_s: 10,
                latency_requests_per_s: 1_000,
                latency_s: 30,
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
        assert_eq!(judge(Kind::Throughput, &busy, offered, &[]), None);
        let idle = loadgen(899_999.0, &[("200", 1000)], 0);
        assert_eq!(judge(Kind::Throughput, &idle, offered, &[]), None);
        let kept_up = loadgen(900_000.0, &[("200", 1000)], 0);
        assert!(
            judge(Kind::Throughput, &kept_up, offered, &[])
                .unwrap()
                .starts_with("not saturated")
        );
        // Latency: every request must succeed with 2xx.
        let clean = loadgen(10.0, &[("200", 1000)], 0);
        assert_eq!(judge(Kind::Latency, &clean, offered, &[]), None);
        assert_eq!((busy.errors(), busy.non_2xx()), (3, 100));
        assert!(judge(Kind::Latency, &busy, offered, &[]).is_some());
        let refused = loadgen(10.0, &[("200", 999), ("503", 1)], 0);
        assert!(judge(Kind::Latency, &refused, offered, &[]).is_some());
        // A rig container that stopped invalidates any arm.
        let stopped = ["crucible (OOM killed)".to_owned()];
        assert_eq!(
            judge(Kind::Latency, &clean, offered, &stopped).unwrap(),
            "stopped during the run: crucible (OOM killed)"
        );
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
            "description=regression: throughput -10.0% p=0.031, p99 +1.2% p=0.031 not gated; 5 pairs, 1kb"
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
