//! `cargo xtask perf`: the A/B performance gate and the fsync-honesty probe of the rig
//! (`docs/testing/perf.md`).

mod gate;
mod rig;
mod stats;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use gate::{
    Build, Container, GateReport, Kind, LaneReport, LoadgenReport, Probe, RUN_SCHEMA, Rig, Run,
    Verdict, compare, judge, order, order_names, status_args, summary, verdict,
};
use rig::{Lane, RigSpec, Session};
use serde::Deserialize;

pub const USAGE: &str = "  perf gate --base <ref> [--candidate <ref>] [--lane 1kb|10kb] [--pairs N] [--rig full|small]
          [--post]
                       compare the candidate build (default HEAD) with the base build: N interleaved pairs
                       (default 5, at least 5 on the full rig) of a throughput and a latency measurement, each
                       arm on a fresh rig; writes gate.json and summary.md under perf/results/local/gate/;
                       --post sets the perf-gate commit status through gh (never for the small rig's smoke runs)
  perf fsync-probe [--rig full|small] [--out FILE]
                       classify the rig's WAL volume with the fsync-honesty probe and write its report
                       (default perf/results/local/fsync-probe/<time>.json)";

/// The fewest pairs of a gate run on the full rig: with fewer, p < 0.05 is out of reach.
pub const MIN_PAIRS: usize = 5;
/// The most pairs: the exact test counts in `u128`, which holds up to 127 differences.
pub const MAX_PAIRS: usize = 100;

/// Which rig.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RigName {
    /// The reference rig.
    Full,
    /// Reduced limits for smoke runs.
    Small,
}

impl RigName {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "full" => Ok(Self::Full),
            "small" => Ok(Self::Small),
            other => Err(format!("--rig: `{other}` is not full or small")),
        }
    }

    fn spec(self) -> &'static RigSpec {
        match self {
            Self::Full => &rig::FULL,
            Self::Small => &rig::SMALL,
        }
    }
}

/// The arguments of `perf gate`.
#[derive(Debug, PartialEq, Eq)]
pub struct GateArgs {
    pub base: String,
    pub candidate: String,
    pub lane: Lane,
    pub pairs: usize,
    pub rig: RigName,
    pub post: bool,
}

/// The arguments of `perf fsync-probe`.
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeArgs {
    pub rig: RigName,
    pub out: Option<PathBuf>,
}

/// The next argument, as the value of flag `flag`.
fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

pub fn parse_gate(args: impl IntoIterator<Item = String>) -> Result<GateArgs, String> {
    let mut parsed = GateArgs {
        base: String::new(),
        candidate: "HEAD".to_owned(),
        lane: Lane::Kib1,
        pairs: MIN_PAIRS,
        rig: RigName::Full,
        post: false,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--base" => parsed.base = value(&mut args, &flag)?,
            "--candidate" => parsed.candidate = value(&mut args, &flag)?,
            "--lane" => {
                let lane = value(&mut args, &flag)?;
                parsed.lane = Lane::parse(&lane)
                    .ok_or_else(|| format!("--lane: `{lane}` is not 1kb or 10kb"))?;
            }
            "--pairs" => {
                let pairs = value(&mut args, &flag)?;
                parsed.pairs = pairs
                    .parse()
                    .ok()
                    .filter(|n| (1..=MAX_PAIRS).contains(n))
                    .ok_or_else(|| {
                        format!("--pairs: `{pairs}` is not a number from 1 to {MAX_PAIRS}")
                    })?;
            }
            "--rig" => parsed.rig = RigName::parse(&value(&mut args, &flag)?)?,
            "--post" => parsed.post = true,
            other => return Err(format!("perf gate: unexpected argument `{other}`")),
        }
    }
    if parsed.base.is_empty() {
        return Err(
            "perf gate needs --base <ref>, the build to compare with (for example main)".to_owned(),
        );
    }
    if parsed.rig == RigName::Full && parsed.pairs < MIN_PAIRS {
        return Err(format!(
            "--pairs {}: the full rig needs at least {MIN_PAIRS} pairs, because with fewer the one-sided p-value \
             cannot fall below 0.05; a smoke run with fewer pairs takes --rig small",
            parsed.pairs
        ));
    }
    if parsed.post && parsed.rig == RigName::Small {
        return Err(
            "--post: small-rig results are smoke runs and are never posted; post a run on --rig full".to_owned(),
        );
    }
    Ok(parsed)
}

pub fn parse_probe(args: impl IntoIterator<Item = String>) -> Result<ProbeArgs, String> {
    let mut parsed = ProbeArgs {
        rig: RigName::Full,
        out: None,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--rig" => parsed.rig = RigName::parse(&value(&mut args, &flag)?)?,
            "--out" => parsed.out = Some(PathBuf::from(value(&mut args, &flag)?)),
            other => return Err(format!("perf fsync-probe: unexpected argument `{other}`")),
        }
    }
    Ok(parsed)
}

/// Runs `cargo xtask perf <args>`. `Ok(false)` when the gate found a regression or could not compare.
pub fn main(mut args: impl Iterator<Item = String>) -> Result<bool, String> {
    match args.next().as_deref() {
        Some("gate") => Ok(run_gate(&parse_gate(args)?)? == Verdict::Pass),
        Some("fsync-probe") => fsync_probe(&parse_probe(args)?).map(|()| true),
        _ => Err("usage:\n".to_owned() + USAGE),
    }
}

/// The repository root: the parent of this crate's directory.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// A UTC time as RFC 3339 (`2026-11-18T21:30:00Z`) and as a compact stamp for paths (`20261118T213000Z`).
fn utc(unix_seconds: u64) -> (String, String) {
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(0);
    let second = unix_seconds % 86_400;
    // Days to the civil date (proleptic Gregorian), counting from 0000-03-01 so leap days end a year.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (second / 3_600, second % 3_600 / 60, second % 60);
    (
        format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"),
        format!("{year:04}{month:02}{day:02}T{h:02}{m:02}{s:02}Z"),
    )
}

fn now() -> (String, String) {
    utc(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()))
}

fn create_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// `path` relative to the repository root when it is inside it, which reads the same on a host and in the
/// container of `tools/perf/xtask.sh`.
fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The fields of a `crucible-loadgen.fsync-probe.v1` report that the gate reads.
#[derive(Debug, Deserialize)]
struct ProbeReport {
    schema: String,
    verdict: String,
    checks: Vec<ProbeCheck>,
}

#[derive(Debug, Deserialize)]
struct ProbeCheck {
    name: String,
    value_ns: i64,
    floor_ns: u64,
    pass: bool,
}

const PROBE_SCHEMA: &str = "crucible-loadgen.fsync-probe.v1";

fn parse_probe_report(json: &str) -> Result<ProbeReport, String> {
    let report: ProbeReport =
        serde_json::from_str(json).map_err(|e| format!("the fsync probe's report: {e}"))?;
    if report.schema == PROBE_SCHEMA {
        Ok(report)
    } else {
        Err(format!(
            "the fsync probe's report has schema `{}`, not {PROBE_SCHEMA}",
            report.schema
        ))
    }
}

fn fsync_probe(args: &ProbeArgs) -> Result<(), String> {
    let root = repository_root();
    let rig = args.rig.spec();
    rig::check_cpus(rig, rig::docker_info()?.1)?;
    let loadgen = rig::build_loadgen(&root)?;
    let json = Session::new(rig)?.probe(&loadgen)?;
    let report = parse_probe_report(&json)?;
    let out = match &args.out {
        Some(out) => out.clone(),
        None => {
            let dir = root.join("perf/results/local/fsync-probe");
            create_dir(&dir)?;
            dir.join(format!("{}.json", now().1))
        }
    };
    write(&out, &json)?;
    println!(
        "fsync probe: the WAL volume of the {} rig is {}",
        rig.name, report.verdict
    );
    for check in &report.checks {
        println!(
            "  {}: {} ns net of its baseline, floor {} ns: {}",
            check.name,
            gate::thousands(check.value_ns as f64),
            gate::thousands(check.floor_ns as f64),
            if check.pass { "pass" } else { "fail" }
        );
    }
    println!("report: {}", shown(&root, &out));
    Ok(())
}

fn run_gate(args: &GateArgs) -> Result<Verdict, String> {
    let root = repository_root();
    let rig = args.rig.spec();
    let (started_at, stamp) = now();
    let ac_power = rig::ac_power();
    if ac_power.state != "online" {
        println!(
            "perf gate: AC power is {} ({}); the result will say so",
            ac_power.state, ac_power.source
        );
    }
    let (docker, cpus) = rig::docker_info()?;
    rig::check_cpus(rig, cpus)?;
    let base_sha = rig::resolve(&root, &args.base)?;
    let candidate_sha = rig::resolve(&root, &args.candidate)?;
    println!(
        "perf gate: {} ({base_sha}) against {} ({candidate_sha}), lane {}, {} pairs, {} rig",
        args.base,
        args.candidate,
        args.lane.name(),
        args.pairs,
        rig.name
    );
    let dir = root.join("perf/results/local/gate").join(format!(
        "{stamp}-{}-{}{}",
        &base_sha[..7],
        &candidate_sha[..7],
        if rig.smoke { "-smoke" } else { "" }
    ));
    create_dir(&dir.join("runs"))?;
    create_dir(&dir.join("logs"))?;

    let base_image = rig::image(&root, &base_sha)?;
    let candidate_image = if candidate_sha == base_sha {
        base_image.clone()
    } else {
        rig::image(&root, &candidate_sha)?
    };
    let loadgen = rig::build_loadgen(&root)?;
    let mut session = Session::new(rig)?;
    let loadgen_sha256 = session.start(&loadgen)?;
    let probe = match session.probe(&loadgen).and_then(|json| {
        let report = parse_probe_report(&json)?;
        write(&dir.join("probe.json"), &json)?;
        Ok(report)
    }) {
        Ok(report) => Probe {
            verdict: report.verdict,
            report: Some("probe.json".to_owned()),
            error: None,
        },
        Err(e) => Probe {
            verdict: "failed".to_owned(),
            report: None,
            error: Some(e),
        },
    };
    println!(
        "perf gate: fsync probe: the WAL volume is {}",
        probe.verdict
    );

    let mut runs = Vec::new();
    for pair in 0..args.pairs {
        for kind in [Kind::Throughput, Kind::Latency] {
            for arm in order(pair) {
                let image = match arm {
                    gate::Arm::A => &base_image.0,
                    gate::Arm::B => &candidate_image.0,
                };
                let name = match kind {
                    Kind::Throughput => "throughput",
                    Kind::Latency => "latency",
                };
                let tag = format!("pair-{}-{name}-{arm:?}", pair + 1);
                let out = session.arm(image, args.lane, kind, &dir, &tag)?;
                let report: LoadgenReport = serde_json::from_str(&out.report)
                    .map_err(|e| format!("runs/{tag}.json: {e}"))?;
                if report.schema != RUN_SCHEMA {
                    return Err(format!(
                        "runs/{tag}.json has schema `{}`, not {RUN_SCHEMA}",
                        report.schema
                    ));
                }
                let run = Run {
                    pair: pair + 1,
                    kind,
                    arm,
                    report: format!("runs/{tag}.json"),
                    log: format!("logs/{tag}.log"),
                    offered_spans_per_s: out.offered_spans_per_s,
                    acked_spans_per_s: report.outcome.acked_spans_per_s,
                    latency_p99_us: report.latency_us.p99,
                    errors: report.errors(),
                    non_2xx: report.non_2xx(),
                    invalid: judge(kind, &report, out.offered_spans_per_s, &out.stopped),
                    context: Some(out.context),
                };
                println!(
                    "perf gate: pair {}/{} {name} {arm:?}: {} spans/s acknowledged, p99 {} us, {} errors, {} \
                     non-2xx{}",
                    pair + 1,
                    args.pairs,
                    gate::thousands(run.acked_spans_per_s),
                    gate::thousands(run.latency_p99_us as f64),
                    run.errors,
                    run.non_2xx,
                    run.invalid
                        .as_ref()
                        .map_or_else(String::new, |reason| format!(
                            "; cannot be compared: {reason}"
                        ))
                );
                runs.push(run);
            }
        }
    }
    drop(session);

    let not_gated = match (probe.verdict.as_str(), &probe.error) {
        ("honest", _) => None,
        (_, Some(error)) => Some(format!(
            "durable-ack p99 is reported and not gated: the fsync probe failed ({error})"
        )),
        (verdict, None) => Some(format!(
            "durable-ack p99 is reported and not gated: the fsync probe classified the WAL volume {verdict}"
        )),
    };
    let comparisons = vec![
        compare(Kind::Throughput, &runs, None),
        compare(Kind::Latency, &runs, not_gated),
    ];
    let lane = args.lane;
    let containers = rig
        .containers()
        .into_iter()
        .map(|(role, limits)| Container {
            role,
            image: match role {
                "crucible" => format!("{} or {}", base_image.0, candidate_image.0),
                "clickhouse" => rig::CLICKHOUSE_IMAGE.to_owned(),
                "postgres" => rig::POSTGRES_IMAGE.to_owned(),
                _ => rig::CLIENT_IMAGE.to_owned(),
            },
            cpuset: limits.cpuset,
            memory: limits.memory,
        })
        .collect();
    let build = |reference: &str, sha: String, (image, image_id): (String, String)| Build {
        reference: reference.to_owned(),
        sha,
        image,
        image_id,
    };
    let report = GateReport {
        schema: gate::GATE_SCHEMA,
        label: gate::LABEL,
        smoke: rig.smoke,
        started_at,
        finished_at: now().0,
        ac_power,
        rig: Rig {
            name: rig.name,
            containers,
            docker,
        },
        lane: LaneReport {
            name: lane.name(),
            span_size: lane.span_size(),
            spans_per_request: lane.spans_per_request(),
            concurrency: rig::CONCURRENCY,
            warmup_s: rig.warmup_s,
            throughput_requests_per_s: rig.throughput_rps,
            throughput_s: rig.throughput_s,
            latency_requests_per_s: rig.latency_rps,
            latency_s: rig.latency_s,
        },
        base: build(&args.base, base_sha, base_image),
        candidate: build(&args.candidate, candidate_sha, candidate_image),
        loadgen_sha256,
        pairs: args.pairs,
        order: order_names(args.pairs),
        probe,
        verdict: verdict(&comparisons),
        comparisons,
        runs,
    };
    let json = serde_json::to_string_pretty(&report).map_err(|e| format!("gate.json: {e}"))? + "\n";
    write(&dir.join("gate.json"), &json)?;
    let text = summary(&report);
    write(&dir.join("summary.md"), &text)?;
    println!(
        "\n{text}\nperf gate: report {}",
        shown(&root, &dir.join("gate.json"))
    );
    if args.post {
        post(&root, &report)?;
    }
    Ok(report.verdict)
}

/// `arg` quoted for a POSIX shell.
fn shell_quote(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_=/.:{}".contains(c))
    {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Sets the perf-gate commit status of `report` on the candidate commit through `gh`.
fn post(root: &Path, report: &GateReport) -> Result<(), String> {
    let args = status_args(report);
    let retry = format!(
        "gh {}",
        args.iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    );
    match Command::new("gh").current_dir(root).args(&args).output() {
        Ok(out) if out.status.success() => {
            println!(
                "perf gate: posted the {} status on {}",
                gate::STATUS_CONTEXT,
                report.candidate.sha
            );
            Ok(())
        }
        Ok(out) => Err(format!(
            "gh could not post the status ({}): {}\nretry with: {retry}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!(
            "gh: {e}; install the GitHub CLI and run `gh auth login`, or post from a machine that has it:\n  \
             {retry}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_args(line: &str) -> Result<GateArgs, String> {
        parse_gate(line.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn gate_arguments_default_to_the_documented_values() {
        assert_eq!(
            gate_args("--base main").unwrap(),
            GateArgs {
                base: "main".to_owned(),
                candidate: "HEAD".to_owned(),
                lane: Lane::Kib1,
                pairs: 5,
                rig: RigName::Full,
                post: false,
            }
        );
        let all =
            gate_args("--base a --candidate b --lane 10kb --pairs 7 --rig full --post").unwrap();
        assert_eq!(
            (all.candidate.as_str(), all.lane, all.pairs, all.post),
            ("b", Lane::Kib10, 7, true)
        );
    }

    #[test]
    fn gate_arguments_are_validated() {
        let refuse = |line: &str, needle: &str| {
            let error = gate_args(line).unwrap_err();
            assert!(error.contains(needle), "`{line}`: {error}");
        };
        refuse("", "needs --base");
        refuse("--candidate HEAD", "needs --base");
        refuse("--base", "--base needs a value");
        refuse("--base main --lane 1k", "`1k` is not 1kb or 10kb");
        refuse("--base main --rig huge", "`huge` is not full or small");
        refuse("--base main --pairs 0", "from 1 to 100");
        refuse("--base main --pairs 101", "from 1 to 100");
        refuse("--base main --pairs five", "from 1 to 100");
        refuse("--base main --head x", "unexpected argument `--head`");
        // Fewer than five pairs only as a smoke run, and smoke runs are never posted.
        refuse("--base main --pairs 4", "at least 5 pairs");
        refuse("--base main --pairs 2 --rig full", "takes --rig small");
        refuse("--base main --rig small --post", "never posted");
        let smoke = gate_args("--base main --candidate main --rig small --pairs 2").unwrap();
        assert_eq!((smoke.rig, smoke.pairs), (RigName::Small, 2));
        assert!(gate_args("--base main --rig small --pairs 1").is_ok());
    }

    #[test]
    fn probe_arguments() {
        let args = |line: &str| parse_probe(line.split_whitespace().map(str::to_owned));
        assert_eq!(
            args("").unwrap(),
            ProbeArgs {
                rig: RigName::Full,
                out: None
            }
        );
        assert_eq!(
            args("--rig small --out p.json").unwrap(),
            ProbeArgs {
                rig: RigName::Small,
                out: Some(PathBuf::from("p.json"))
            }
        );
        assert!(
            args("--dir /x")
                .unwrap_err()
                .contains("unexpected argument")
        );
        assert!(args("--out").unwrap_err().contains("needs a value"));
    }

    #[test]
    fn subcommands() {
        let run = |line: &str| main(line.split_whitespace().map(str::to_owned));
        assert!(run("").unwrap_err().starts_with("usage:"));
        assert!(run("bench").unwrap_err().starts_with("usage:"));
        assert!(run("gate --pairs 3").unwrap_err().contains("needs --base"));
    }

    #[test]
    fn utc_times() {
        assert_eq!(
            utc(0),
            (
                "1970-01-01T00:00:00Z".to_owned(),
                "19700101T000000Z".to_owned()
            )
        );
        assert_eq!(utc(951_782_400).0, "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_700_000_000).0, "2023-11-14T22:13:20Z");
        assert_eq!(utc(4_102_444_799).0, "2099-12-31T23:59:59Z");
    }

    #[test]
    fn probe_reports_parse() {
        let json = r#"{"schema":"crucible-loadgen.fsync-probe.v1","dir":"/probe","verdict":"honest",
            "checks":[{"name":"small_write_flush","value_ns":1190707,"floor_ns":100000,"pass":true}]}"#;
        let report = parse_probe_report(json).unwrap();
        assert_eq!(
            (report.verdict.as_str(), report.checks.len()),
            ("honest", 1)
        );
        let check = &report.checks[0];
        assert_eq!(
            (
                check.name.as_str(),
                check.value_ns,
                check.floor_ns,
                check.pass
            ),
            ("small_write_flush", 1_190_707, 100_000, true)
        );
        assert!(parse_probe_report(&json.replace("probe.v1", "probe.v0")).is_err());
    }

    #[test]
    fn retry_commands_are_quoted_for_a_shell() {
        assert_eq!(shell_quote("state=success"), "state=success");
        assert_eq!(
            shell_quote("repos/{owner}/{repo}/statuses/abc"),
            "repos/{owner}/{repo}/statuses/abc"
        );
        assert_eq!(
            shell_quote("description=no regression: it's"),
            r"'description=no regression: it'\''s'"
        );
    }
}
