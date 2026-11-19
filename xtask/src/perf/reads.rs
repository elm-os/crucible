//! `cargo xtask perf reads`: the read-latency bench (`docs/testing/perf.md`).
//!
//! One rig (ClickHouse, Postgres and Crucible from one build): the ack-to-API probe on the empty store; then a
//! dataset loaded through ingest, its span times spread over the read window, in chunks that each drain before
//! the next; the probe again on the loaded store; then one lane per read class, each an open-loop schedule of
//! that class alone, with Crucible's per-request counters read before and after it so that every counter's
//! change belongs to that class. Writes `reads.json` (schema [`READS_SCHEMA`]) and `summary.md`.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::gate::{Build, LABEL, Power, thousands};
use super::rig::{self, ArmKey, Context, Session};
use super::{RigName, create_dir, now, repository_root, shown, write};

/// The `schema` value of `reads.json`.
pub const READS_SCHEMA: &str = "crucible-perf.reads.v1";

/// The arguments of `perf reads`.
#[derive(Debug, PartialEq)]
pub struct ReadsArgs {
    /// The build to measure.
    pub image: String,
    /// The rig.
    pub rig: RigName,
    /// Spans to load; `None` takes the rig's default.
    pub spans: Option<u64>,
    /// Days the loaded spans' start times spread over, ending at the load.
    pub spread_days: u64,
    /// Reads per second of each lane, on average.
    pub rate: f64,
    /// Seconds of each lane.
    pub seconds: u64,
    /// Traces of each ack-to-API probe.
    pub probes: u32,
    /// Only these lanes, by name; `None` runs every lane.
    pub lanes: Option<Vec<String>>,
}

/// A read lane: a class of the load generator's `read`, and the request class of Crucible's counters it hits.
struct Lane {
    name: &'static str,
    server_class: &'static str,
    class: &'static str,
}

/// The lanes: trace by id, the 30-day trace list, and the other list and detail endpoints. The search looks
/// for a word no span holds over the window; the payload reads the attributes of one span. The MCP tool
/// dashboard gets its lane here once its endpoints exist (`docs/testing/perf.md`).
const LANES: [Lane; 13] = [
    Lane {
        name: "trace_by_id",
        server_class: "trace_by_id",
        class: r#"{"name":"trace_by_id","method":"GET","path":"/api/v1/projects/{project}/traces/{trace_id}","nonempty":"/spans"}"#,
    },
    Lane {
        name: "trace_by_id_hinted",
        server_class: "trace_by_id",
        class: r#"{"name":"trace_by_id_hinted","method":"GET","path":"/api/v1/projects/{project}/traces/{trace_id}?hint_time={trace_start}","nonempty":"/spans"}"#,
    },
    Lane {
        name: "trace_list_30d",
        server_class: "trace_list",
        class: r#"{"name":"trace_list_30d","method":"GET","path":"/api/v1/projects/{project}/traces?from={from}&to={to}&limit=50","nonempty":"/data"}"#,
    },
    Lane {
        name: "span_list_30d",
        server_class: "span_list",
        class: r#"{"name":"span_list_30d","method":"GET","path":"/api/v1/projects/{project}/spans?from={from}&to={to}&limit=50","nonempty":"/data"}"#,
    },
    Lane {
        name: "session_list_30d",
        server_class: "session_list",
        class: r#"{"name":"session_list_30d","method":"GET","path":"/api/v1/projects/{project}/sessions?from={from}&to={to}&limit=50"}"#,
    },
    Lane {
        name: "user_list_30d",
        server_class: "user_list",
        class: r#"{"name":"user_list_30d","method":"GET","path":"/api/v1/projects/{project}/users?from={from}&to={to}&limit=50"}"#,
    },
    Lane {
        name: "span_children",
        server_class: "span_children",
        class: r#"{"name":"span_children","method":"GET","path":"/api/v1/projects/{project}/traces/{trace_id}/spans?parent_span_id={root_span_id}&limit=100","nonempty":"/data"}"#,
    },
    Lane {
        name: "payload",
        server_class: "payload",
        class: r#"{"name":"payload","method":"GET","path":"/api/v1/projects/{project}/spans/{trace_id}/{span_id}/payload?field=attributes&start_time={span_start}"}"#,
    },
    Lane {
        name: "facts",
        server_class: "facts",
        class: r#"{"name":"facts","method":"GET","path":"/api/v1/projects/{project}/facts?trace_id={trace_id}"}"#,
    },
    Lane {
        name: "filter_values_30d",
        server_class: "filter_values",
        class: r#"{"name":"filter_values_30d","method":"GET","path":"/api/v1/projects/{project}/filter-values?source=spans&field=observation_type&from={from}&to={to}","nonempty":"/data"}"#,
    },
    Lane {
        name: "data_bounds",
        server_class: "data_bounds",
        class: r#"{"name":"data_bounds","method":"GET","path":"/api/v1/projects/{project}/data-bounds","nonempty":"/spans/first"}"#,
    },
    Lane {
        name: "search_30d",
        server_class: "search",
        class: r#"{"name":"search_30d","method":"POST","path":"/api/v1/projects/{project}/search","body":"{\"query\":\"tokennotloaded\",\"from\":\"{from}\",\"to\":\"{to}\"}"}"#,
    },
    Lane {
        name: "metrics_30d",
        server_class: "metrics",
        class: r#"{"name":"metrics_30d","method":"POST","path":"/api/v1/projects/{project}/metrics/query","body":"{\"source\":\"spans\",\"measures\":[{\"measure\":\"count\",\"aggregation\":\"sum\"}],\"from\":\"{from}\",\"to\":\"{to}\"}","nonempty":"/series"}"#,
    },
];

/// The default spans to load, and the load's requests per second (100 spans each).
fn load_plan(rig: RigName) -> (u64, u64) {
    match rig {
        // Millions of spans, loaded below the small rig's end-to-end capacity (about 58,000 spans/s).
        RigName::Small => (2_000_000, 150),
        // The 100-million-span dataset runs in the exclusive window: `--spans 100000000`.
        RigName::Full => (10_000_000, 500),
    }
}

const SPANS_PER_REQUEST: u64 = 100;
const CONCURRENCY: u32 = 8;
/// Above this send lag p95, a lane's requests queued for a connection: the schedule outran the server.
const SATURATED_LAG_US: u64 = 25_000;
/// Attempts of a lane, at most. An attempt during which the host dropped its page cache runs again: its reads
/// after the drop went to the disk, while the other lanes' came from the cache.
const ATTEMPTS: u32 = 3;

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn number<T: std::str::FromStr>(text: &str, flag: &str) -> Result<T, String> {
    text.parse()
        .map_err(|_| format!("{flag}: `{text}` is not a number"))
}

pub fn parse_reads(args: impl IntoIterator<Item = String>) -> Result<ReadsArgs, String> {
    let mut parsed = ReadsArgs {
        image: "HEAD".to_owned(),
        rig: RigName::Small,
        spans: None,
        spread_days: 30,
        rate: 2.0,
        seconds: 60,
        probes: 100,
        lanes: None,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--image" => parsed.image = value(&mut args, &flag)?,
            "--rig" => parsed.rig = RigName::parse(&value(&mut args, &flag)?)?,
            "--spans" => parsed.spans = Some(number(&value(&mut args, &flag)?, &flag)?),
            "--spread-days" => parsed.spread_days = number(&value(&mut args, &flag)?, &flag)?,
            "--rate" => parsed.rate = number(&value(&mut args, &flag)?, &flag)?,
            "--seconds" => parsed.seconds = number(&value(&mut args, &flag)?, &flag)?,
            "--probes" => parsed.probes = number(&value(&mut args, &flag)?, &flag)?,
            "--lanes" => {
                let names: Vec<String> = value(&mut args, &flag)?
                    .split(',')
                    .map(str::to_owned)
                    .collect();
                if let Some(unknown) = names
                    .iter()
                    .find(|name| !LANES.iter().any(|lane| lane.name == *name))
                {
                    return Err(format!(
                        "--lanes: `{unknown}` is not a lane of the read bench"
                    ));
                }
                parsed.lanes = Some(names);
            }
            other => return Err(format!("perf reads: unexpected argument `{other}`")),
        }
    }
    if !(parsed.rate > 0.0 && parsed.seconds > 0 && parsed.probes > 0) {
        return Err("perf reads: --rate, --seconds and --probes must be above 0".to_owned());
    }
    if parsed.spans == Some(0) || parsed.spread_days == 0 || parsed.spread_days > 400 {
        return Err(
            "perf reads: --spans must be above 0 and --spread-days from 1 to 400 (ingest refuses spans older \
             than its maximum event age, 400 days by default)"
                .to_owned(),
        );
    }
    Ok(parsed)
}

/// Percentiles of a load generator summary, in microseconds.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Percentiles {
    /// Samples.
    pub count: u64,
    /// Median.
    pub p50: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile.
    pub p99: u64,
}

/// One ack-to-API probe.
#[derive(Clone, Debug, Serialize)]
pub struct Fresh {
    /// The probe's report, relative to the results directory.
    pub report: String,
    /// Probes sent and listed.
    pub probes: u64,
    /// See `probes`.
    pub listed: u64,
    /// From the acknowledgement to the first list that names the trace.
    pub ack_to_list_us: Percentiles,
    /// Lists per listed probe: the first list leaves at the acknowledgement.
    pub lists_per_probe: Percentiles,
    /// Each list's round trip, so that `ack_to_list_us` reads as lists times their cost.
    pub list_us: Percentiles,
    /// What else the Docker host did during the probe.
    pub context: Context,
}

/// The loaded dataset.
#[derive(Clone, Debug, Serialize)]
pub struct DatasetReport {
    /// Spans acknowledged and stored.
    pub spans: u64,
    /// The load's runs, each a report relative to the results directory.
    pub reports: Vec<String>,
    /// The days the spans' start times spread over.
    pub spread_days: u64,
    /// Spans per second the load offered.
    pub offered_spans_per_s: u64,
    /// Seconds from the load's start until the store held every span.
    pub load_s: f64,
    /// Bytes of the store's active parts of the telemetry tables, and per span.
    pub bytes_on_disk: u64,
    /// See `bytes_on_disk`.
    pub bytes_per_span: f64,
    /// Whether the store fits within half of ClickHouse's memory, so that each lane attempt starts by reading
    /// the store's files into the page cache.
    pub warmed: bool,
}

/// One read lane.
#[derive(Clone, Debug, Serialize)]
pub struct LaneReport {
    /// The lane.
    pub name: &'static str,
    /// The request class of Crucible's counters.
    pub server_class: &'static str,
    /// The load generator's report of the last attempt, relative to the results directory.
    pub report: String,
    /// Attempts of the lane; the numbers are the last attempt's.
    pub attempts: u32,
    /// Requests sent, and answered 2xx and non-empty.
    pub sent: u64,
    /// See `sent`.
    pub ok: u64,
    /// Intended send time to complete answer, over `ok` answers.
    pub latency_us: Percentiles,
    /// `latency_us` over the reads due in the lane's first half, and in its second.
    pub latency_halves_us: [Percentiles; 2],
    /// The class's requests that Crucible counted during the lane.
    pub counted: u64,
    /// Store reads per request, from `crucible_api_store_reads`.
    pub store_reads_per_request: Option<f64>,
    /// Mean wait for the read barrier per request that waited, from `crucible_api_barrier_wait_microseconds`.
    pub barrier_wait_mean_us: Option<f64>,
    /// Connections the store opened during the lane (`crucible_clickhouse_connections_opened_total`).
    pub connections_opened: u64,
    /// ClickHouse merges running at the lane's start.
    pub merges_at_start: u64,
    /// What else the Docker host did during the lane.
    pub context: Option<Context>,
    /// Why the lane's latency does not stand, or `None`.
    pub invalid: Option<String>,
}

/// The JSON report of a bench run (schema [`READS_SCHEMA`]).
#[derive(Clone, Debug, Serialize)]
pub struct ReadsReport {
    /// Always [`READS_SCHEMA`].
    pub schema: &'static str,
    /// Always the gate's label.
    pub label: &'static str,
    /// A small-rig run.
    pub smoke: bool,
    /// When the run started and finished, UTC.
    pub started_at: String,
    /// See `started_at`.
    pub finished_at: String,
    /// The AC-power state.
    pub ac_power: Power,
    /// The rig's name and the Docker host.
    pub rig: &'static str,
    /// See `rig`.
    pub docker: String,
    /// The build.
    pub build: Build,
    /// SHA-256 of the load generator.
    pub loadgen_sha256: String,
    /// Reads per second of each lane, its seconds, its connections and its read window in days.
    pub rate: f64,
    /// See `rate`.
    pub seconds: u64,
    /// See `rate`.
    pub concurrency: u32,
    /// See `rate`.
    pub window_days: u64,
    /// The ack-to-API probe on the empty store, and on the loaded one.
    pub fresh_empty: Fresh,
    /// See `fresh_empty`.
    pub fresh_loaded: Fresh,
    /// The dataset.
    pub dataset: DatasetReport,
    /// One entry per lane.
    pub lanes: Vec<LaneReport>,
}

/// The value of a sample in Prometheus text: `name{labels} value`, with `labels` matched exactly (`""` for none).
pub fn sample(text: &str, name: &str, labels: &str) -> Option<f64> {
    let prefix = if labels.is_empty() {
        format!("{name} ")
    } else {
        format!("{name}{{{labels}}} ")
    };
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .and_then(|value| value.trim().parse().ok())
}

fn percentiles(summary: &serde_json::Value) -> Percentiles {
    let field = |name: &str| summary[name].as_u64().unwrap_or(0);
    Percentiles {
        count: field("count"),
        p50: field("p50"),
        p95: field("p95"),
        p99: field("p99"),
    }
}

fn parse(json: &str, what: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(json).map_err(|e| format!("{what}: {e}"))
}

/// Runs `cargo xtask perf reads`.
pub fn run_reads(args: &ReadsArgs) -> Result<(), String> {
    let root = repository_root();
    let rig = args.rig.spec();
    let (started_at, stamp) = now();
    let ac_power = rig::ac_power();
    let (docker, cpus) = rig::docker_info()?;
    rig::check_cpus(rig, cpus)?;
    let sha = rig::resolve(&root, &args.image)?;
    let (default_spans, load_rps) = load_plan(args.rig);
    let spans = args.spans.unwrap_or(default_spans);
    println!(
        "perf reads: {} ({sha}), {} rig, {} spans over {} days, lanes of {} reads/s for {} s",
        args.image,
        rig.name,
        thousands(spans as f64),
        args.spread_days,
        args.rate,
        args.seconds
    );
    let dir = root.join("perf/results/local/reads").join(format!(
        "{stamp}-{}{}",
        &sha[..7],
        if rig.smoke { "-smoke" } else { "" }
    ));
    for sub in ["runs", "logs", "dataset", "metrics"] {
        create_dir(&dir.join(sub))?;
    }
    let image = rig::image(&root, &sha)?;
    let loadgen = rig::build_loadgen(&root)?;
    let mut session = Session::new(rig)?;
    let loadgen_sha256 = session.start(&loadgen)?;
    let result = bench(&session, args, &image.0, &dir, spans, load_rps);
    let _ = std::fs::write(
        dir.join("logs").join("crucible.log"),
        docker_logs(&session.crucible()),
    );
    session.down();
    drop(session);
    let (fresh_empty, fresh_loaded, dataset, lanes) = result?;
    let report = ReadsReport {
        schema: READS_SCHEMA,
        label: LABEL,
        smoke: rig.smoke,
        started_at,
        finished_at: now().0,
        ac_power,
        rig: rig.name,
        docker,
        build: Build {
            reference: args.image.clone(),
            sha,
            image: image.0,
            image_id: image.1,
        },
        loadgen_sha256,
        rate: args.rate,
        seconds: args.seconds,
        concurrency: CONCURRENCY,
        window_days: args.spread_days,
        fresh_empty,
        fresh_loaded,
        dataset,
        lanes,
    };
    let json =
        serde_json::to_string_pretty(&report).map_err(|e| format!("reads.json: {e}"))? + "\n";
    write(&dir.join("reads.json"), &json)?;
    let text = summary(&report);
    write(&dir.join("summary.md"), &text)?;
    println!(
        "\n{text}\nperf reads: report {}",
        shown(&root, &dir.join("reads.json"))
    );
    Ok(())
}

fn docker_logs(container: &str) -> String {
    std::process::Command::new("docker")
        .args(["logs", container])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout).into_owned()
                + &String::from_utf8_lossy(&out.stderr)
        })
        .unwrap_or_default()
}

type Bench = (Fresh, Fresh, DatasetReport, Vec<LaneReport>);

fn bench(
    session: &Session<'_>,
    args: &ReadsArgs,
    image: &str,
    dir: &Path,
    spans: u64,
    load_rps: u64,
) -> Result<Bench, String> {
    let key = session.up(image, dir, "reads")?;
    let crucible = session.crucible();
    let bearer = format!("Authorization: Bearer {}", key.secret);
    let fresh_empty = fresh(
        session,
        &key,
        &crucible,
        &bearer,
        args.probes,
        dir,
        "fresh-empty",
    )?;
    println!(
        "perf reads: ack to API on the empty store: p50 {} us, p95 {} us, p99 {} us ({} of {} listed); {} lists \
         per probe (p50), each {} us (p50)",
        thousands(fresh_empty.ack_to_list_us.p50 as f64),
        thousands(fresh_empty.ack_to_list_us.p95 as f64),
        thousands(fresh_empty.ack_to_list_us.p99 as f64),
        fresh_empty.listed,
        fresh_empty.probes,
        fresh_empty.lists_per_probe.p50,
        thousands(fresh_empty.list_us.p50 as f64)
    );

    // The load: one chunk per day of the window, oldest first, each chunk's spans spread over its day in time
    // order, as live traffic arrives, so that an insert touches one or two daily partitions. Each chunk drains
    // before the next, so the WAL's backlog stays bounded.
    let started = Instant::now();
    let days = args.spread_days;
    let per_day = spans.div_ceil(days).div_ceil(SPANS_PER_REQUEST);
    let secs = per_day.div_ceil(load_rps).max(1);
    let (mut stored, mut reports) = (0u64, Vec::new());
    for chunk in 1..=days {
        let backdate = ((days - chunk) * 86_400).to_string();
        let path = format!("/tmp/load-{chunk}.json");
        session.loadgen_exec(
            &[
                "run".to_owned(),
                "--target".to_owned(),
                format!("{crucible}:4318"),
                "--rate".to_owned(),
                load_rps.to_string(),
                "--duration-secs".to_owned(),
                secs.to_string(),
                "--span-size".to_owned(),
                "1k".to_owned(),
                "--spans-per-request".to_owned(),
                SPANS_PER_REQUEST.to_string(),
                "--concurrency".to_owned(),
                "64".to_owned(),
                "--spread-secs".to_owned(),
                "86400".to_owned(),
                "--backdate-secs".to_owned(),
                backdate,
                "--seed".to_owned(),
                chunk.to_string(),
                "--header".to_owned(),
                bearer.clone(),
                "--out".to_owned(),
                path.clone(),
            ],
            Duration::from_secs(120 + 20 * secs),
        )?;
        let text = session.client_exec(&["cat", &path])?;
        let acked = parse(&text, &path)?["outcome"]["acked_spans"]
            .as_u64()
            .unwrap_or(0);
        let name = format!("dataset/load-{chunk}.json");
        write(&dir.join(&name), &text)?;
        reports.push((name, path));
        stored += acked;
        // Only this chunk is behind. The store drains at least 2,000 spans/s on any rig the gate runs on; allow
        // five minutes beyond that.
        let limit = Duration::from_secs(300 + acked / 2_000);
        if !session.wait_for_store(&key, stored, limit)? {
            return Err(format!(
                "the store did not hold the {stored} acknowledged spans of the load within {} s",
                limit.as_secs()
            ));
        }
        println!(
            "perf reads: loaded {} of {} spans ({:.0} s)",
            thousands(stored as f64),
            thousands(spans as f64),
            started.elapsed().as_secs_f64()
        );
    }
    let load_s = started.elapsed().as_secs_f64();
    // Past the visibility grace (2 s on one node), the store, not the recent index, answers for the dataset.
    std::thread::sleep(Duration::from_secs(10));
    let disk = session.clickhouse_row(
        "SELECT toUInt64(sum(bytes_on_disk)) AS bytes FROM system.parts WHERE active AND database = 'crucible'",
    )?;
    let bytes_on_disk = disk["bytes"]
        .as_u64()
        .or_else(|| disk["bytes"].as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    // The lanes measure the store answering from the page cache when the cache can hold it: within half of
    // ClickHouse's memory, which its page cache counts against. A larger store is read from the disk, as it
    // would be.
    let warmed = bytes_on_disk.saturating_mul(2) <= args.rig.spec().clickhouse.memory_bytes();
    let dataset = DatasetReport {
        spans: stored,
        reports: reports.iter().map(|(name, _)| name.clone()).collect(),
        spread_days: args.spread_days,
        offered_spans_per_s: load_rps * SPANS_PER_REQUEST,
        load_s,
        bytes_on_disk,
        bytes_per_span: bytes_on_disk as f64 / stored.max(1) as f64,
        warmed,
    };
    let fresh_loaded = fresh(
        session,
        &key,
        &crucible,
        &bearer,
        args.probes,
        dir,
        "fresh-loaded",
    )?;

    let mut lanes = Vec::new();
    let chosen = |lane: &&Lane| {
        args.lanes
            .as_ref()
            .is_none_or(|names| names.iter().any(|name| name == lane.name))
    };
    // One attempt of a lane. When the store fits the cache, the attempt starts by reading the store's files once,
    // so that what an earlier lane or a page-cache drop left in the cache does not decide its latency.
    let attempt_lane = |index: usize, lane: &Lane, attempt: u32| -> Result<LaneReport, String> {
        let merges = session.clickhouse_row("SELECT count() AS n FROM system.merges")?;
        let merges_at_start = merges["n"]
            .as_u64()
            .or_else(|| merges["n"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        let before = session.metrics()?;
        write(
            &dir.join(format!("metrics/{}-{attempt}-before.txt", lane.name)),
            &before,
        )?;
        if warmed {
            session.warm_store()?;
        }
        let mut command = vec![
            "read".to_owned(),
            "--target".to_owned(),
            format!("{crucible}:8080"),
            "--project".to_owned(),
            key.project_id.clone(),
            "--class".to_owned(),
            lane.class.to_owned(),
            "--rate".to_owned(),
            args.rate.to_string(),
            "--duration-secs".to_owned(),
            args.seconds.to_string(),
            "--concurrency".to_owned(),
            CONCURRENCY.to_string(),
            "--window-secs".to_owned(),
            (args.spread_days * 86_400).to_string(),
            "--seed".to_owned(),
            (index + 1).to_string(),
            "--header".to_owned(),
            bearer.clone(),
        ];
        for (_, path) in &reports {
            command.extend(["--dataset".to_owned(), path.clone()]);
        }
        let (text, context) = session.observed(|| {
            session.loadgen_exec(&command, Duration::from_secs(120 + 3 * args.seconds))
        })?;
        let text = text?;
        let after = session.metrics()?;
        write(
            &dir.join(format!("metrics/{}-{attempt}-after.txt", lane.name)),
            &after,
        )?;
        let name = format!("runs/{}-{attempt}.json", lane.name);
        write(&dir.join(&name), &text)?;
        let lane_report = judge_lane(lane, &parse(&text, &name)?, &before, &after, name);
        Ok(LaneReport {
            attempts: attempt,
            merges_at_start,
            context: Some(context),
            ..lane_report
        })
    };
    for (index, lane) in LANES.iter().enumerate().filter(|(_, lane)| chosen(lane)) {
        let mut attempt = 1;
        let mut lane_report = loop {
            let report = attempt_lane(index, lane, attempt)?;
            let drops = report.context.as_ref().map_or(0, |c| c.page_cache_drops);
            if drops == 0 || attempt == ATTEMPTS {
                break report;
            }
            println!(
                "perf reads: {}: the host dropped its page cache during attempt {attempt}, so reads went to the \
                 disk; running it again",
                lane.name
            );
            attempt += 1;
        };
        let (other_cpus_busy, page_cache_drops) = lane_report
            .context
            .as_ref()
            .map_or((0.0, 0), |c| (c.other_cpus_busy, c.page_cache_drops));
        if page_cache_drops > 0 {
            let disturbed = format!(
                "the host dropped its page cache during each of its {ATTEMPTS} attempts, so some of its reads \
                 went to the disk"
            );
            lane_report.invalid = Some(match lane_report.invalid.take() {
                Some(reasons) => format!("{reasons}; {disturbed}"),
                None => disturbed,
            });
        }
        let [first, second] = lane_report.latency_halves_us;
        println!(
            "perf reads: {}: p50 {} us, p95 {} us, p99 {} us (p95 {} us in the first half, {} us in the second), \
             {} of {} ok, {} store reads per request, {:.1} other CPUs busy, {} page-cache drops, attempt {}{}",
            lane.name,
            thousands(lane_report.latency_us.p50 as f64),
            thousands(lane_report.latency_us.p95 as f64),
            thousands(lane_report.latency_us.p99 as f64),
            thousands(first.p95 as f64),
            thousands(second.p95 as f64),
            lane_report.ok,
            lane_report.sent,
            lane_report
                .store_reads_per_request
                .map_or_else(|| "n/a".to_owned(), |r| format!("{r:.2}")),
            other_cpus_busy,
            page_cache_drops,
            lane_report.attempts,
            lane_report
                .invalid
                .as_ref()
                .map_or_else(String::new, |reason| format!("; invalid: {reason}"))
        );
        lanes.push(lane_report);
    }
    Ok((fresh_empty, fresh_loaded, dataset, lanes))
}

/// The lane's numbers from the load generator's report and the counters before and after it.
fn judge_lane(
    lane: &Lane,
    report: &serde_json::Value,
    before: &str,
    after: &str,
    path: String,
) -> LaneReport {
    let class = &report["classes"][0];
    let count = |key: &str| class[key].as_u64().unwrap_or(0);
    let (sent, ok) = (count("sent"), count("ok"));
    let labels = format!("class=\"{}\"", lane.server_class);
    let delta = |name: &str, labels: &str| {
        sample(after, name, labels).unwrap_or(0.0) - sample(before, name, labels).unwrap_or(0.0)
    };
    let counted = delta("crucible_api_store_reads_count", &labels).max(0.0) as u64;
    let reads = delta("crucible_api_store_reads_sum", &labels);
    let waits = delta("crucible_api_barrier_wait_microseconds_count", &labels);
    let waited = delta("crucible_api_barrier_wait_microseconds_sum", &labels);
    let connections = delta("crucible_clickhouse_connections_opened_total", "").max(0.0) as u64;
    let mut reasons = Vec::new();
    if ok < sent || sent == 0 {
        reasons.push(format!(
            "{} of {sent} requests were not answered 2xx with a non-empty answer, so the latency does not cover \
             them",
            sent - ok.min(sent)
        ));
    }
    if counted < ok {
        reasons.push(format!(
            "Crucible counted {counted} {} requests for {ok} answers",
            lane.server_class
        ));
    }
    // A request waits for a free connection only when the schedule outruns the server: its latency then grows
    // with the queue, not with the read.
    let lag_p95 = report["send_lag_us"]["p95"].as_u64().unwrap_or(0);
    if lag_p95 > SATURATED_LAG_US {
        reasons.push(format!(
            "saturated: requests waited {} ms (send lag p95) for a free connection, so the schedule outran the \
             server; lower --rate",
            lag_p95 / 1000
        ));
    }
    LaneReport {
        name: lane.name,
        server_class: lane.server_class,
        report: path,
        attempts: 1,
        sent,
        ok,
        latency_us: percentiles(&class["latency_us"]),
        latency_halves_us: [
            percentiles(&class["latency_halves_us"][0]),
            percentiles(&class["latency_halves_us"][1]),
        ],
        counted,
        store_reads_per_request: (counted > 0).then(|| reads / counted as f64),
        barrier_wait_mean_us: (waits > 0.0).then(|| waited / waits),
        connections_opened: connections,
        merges_at_start: 0,
        context: None,
        invalid: (!reasons.is_empty()).then(|| reasons.join("; ")),
    }
}

/// One ack-to-API probe of `probes` traces.
fn fresh(
    session: &Session<'_>,
    key: &ArmKey,
    crucible: &str,
    bearer: &str,
    probes: u32,
    dir: &Path,
    name: &str,
) -> Result<Fresh, String> {
    let (text, context) = session.observed(|| {
        session.loadgen_exec(
            &[
                "fresh".to_owned(),
                "--ingest".to_owned(),
                format!("{crucible}:4318"),
                "--api".to_owned(),
                format!("{crucible}:8080"),
                "--project".to_owned(),
                key.project_id.clone(),
                "--probes".to_owned(),
                probes.to_string(),
                "--header".to_owned(),
                bearer.to_owned(),
            ],
            Duration::from_secs(120 + u64::from(probes)),
        )
    })?;
    let text = text?;
    let path = format!("runs/{name}.json");
    write(&dir.join(&path), &text)?;
    let report = parse(&text, &path)?;
    Ok(Fresh {
        report: path,
        probes: report["probes"].as_u64().unwrap_or(0),
        listed: report["listed"].as_u64().unwrap_or(0),
        ack_to_list_us: percentiles(&report["ack_to_list_us"]),
        lists_per_probe: percentiles(&report["lists_per_probe"]),
        list_us: percentiles(&report["list_us"]),
        context,
    })
}

fn ms(us: u64) -> String {
    format!("{:.1}", us as f64 / 1000.0)
}

/// The markdown summary of `report`.
pub fn summary(report: &ReadsReport) -> String {
    let mut out = String::new();
    let smoke = if report.smoke { " (smoke)" } else { "" };
    let _ = writeln!(
        out,
        "# Read bench{smoke}\n\nMeasured on a laptop, not reference hardware.\n"
    );
    let dataset = &report.dataset;
    let _ = writeln!(out, "| Setting | Value |\n|---|---|");
    for (name, value) in [
        (
            "Build",
            format!("`{}` at `{}`", report.build.reference, report.build.sha),
        ),
        ("Rig", format!("{} ({})", report.rig, report.docker)),
        (
            "AC power",
            format!("{} ({})", report.ac_power.state, report.ac_power.source),
        ),
        (
            "Dataset",
            format!(
                "{} spans of 1 KiB, 100 per trace, start times spread over {} days, loaded through ingest at {} \
                 spans/s in {:.0} s; {} bytes on disk ({:.0} per span), {}",
                thousands(dataset.spans as f64),
                dataset.spread_days,
                thousands(dataset.offered_spans_per_s as f64),
                dataset.load_s,
                thousands(dataset.bytes_on_disk as f64),
                dataset.bytes_per_span,
                if dataset.warmed {
                    "read into the page cache before each lane attempt"
                } else {
                    "more than half of ClickHouse's memory, so the lanes read it from the disk"
                }
            ),
        ),
        (
            "Lanes",
            format!(
                "one class each, {} reads/s on average (exponential think times) for {} s, {} connections, \
                 windows of {} days",
                report.rate, report.seconds, report.concurrency, report.window_days
            ),
        ),
        (
            "Run",
            format!("{} to {}", report.started_at, report.finished_at),
        ),
    ] {
        let _ = writeln!(out, "| {name} | {value} |");
    }
    let _ = writeln!(
        out,
        "\n## Ack to API\n\nFrom the acknowledgement of one small trace to the first list of traces that names it, \
         polled every 2 ms; the first list leaves at the acknowledgement.\n\n| Store | Listed | p50 ms | p95 ms | \
         p99 ms | Lists per probe p50, p99 | List round trip p50, p95 ms | Other CPUs busy | Page-cache drops \
         |\n|---|---|---|---|---|---|---|---|---|"
    );
    for (store, fresh) in [
        ("empty", &report.fresh_empty),
        ("loaded", &report.fresh_loaded),
    ] {
        let p = fresh.ack_to_list_us;
        let _ = writeln!(
            out,
            "| {store} | {} of {} | {} | {} | {} | {}, {} | {}, {} | {:.1} | {} |",
            fresh.listed,
            fresh.probes,
            ms(p.p50),
            ms(p.p95),
            ms(p.p99),
            fresh.lists_per_probe.p50,
            fresh.lists_per_probe.p99,
            ms(fresh.list_us.p50),
            ms(fresh.list_us.p95),
            fresh.context.other_cpus_busy,
            fresh.context.page_cache_drops
        );
    }
    let _ = writeln!(
        out,
        "\n## Reads\n\nOther jobs' containers have no cpuset, so they can also run on the rig's CPUs. After a page-cache \
         drop on the host, the store's reads go to the disk, so an attempt during which the host dropped its page \
         cache runs again, up to {ATTEMPTS} attempts; the numbers are the last attempt's.\n\n| Lane | Attempts | OK \
         of sent | p50 ms | p95 ms | p99 ms | p95 ms by half | Store reads per request | Barrier wait mean ms | \
         Connections opened | Merges at start | Other CPUs busy | Page-cache drops | Valid \
         |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for lane in &report.lanes {
        let p = lane.latency_us;
        let [first, second] = lane.latency_halves_us;
        let _ = writeln!(
            out,
            "| {} | {} | {} of {} | {} | {} | {} | {}, {} | {} | {} | {} | {} | {} | {} | {} |",
            lane.name,
            lane.attempts,
            lane.ok,
            lane.sent,
            ms(p.p50),
            ms(p.p95),
            ms(p.p99),
            ms(first.p95),
            ms(second.p95),
            lane.store_reads_per_request
                .map_or_else(|| "n/a".to_owned(), |r| format!("{r:.2}")),
            lane.barrier_wait_mean_us
                .map_or_else(|| "n/a".to_owned(), |us| format!("{:.2}", us / 1000.0)),
            lane.connections_opened,
            lane.merges_at_start,
            lane.context
                .as_ref()
                .map_or_else(|| "n/a".to_owned(), |c| format!("{:.1}", c.other_cpus_busy)),
            lane.context
                .as_ref()
                .map_or_else(|| "n/a".to_owned(), |c| c.page_cache_drops.to_string()),
            lane.invalid.as_deref().unwrap_or("yes")
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<ReadsArgs, String> {
        parse_reads(line.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn arguments_default_to_the_small_rig() {
        let parsed = args("").unwrap();
        assert_eq!(
            (
                parsed.image.as_str(),
                parsed.rig,
                parsed.spans,
                parsed.spread_days
            ),
            ("HEAD", RigName::Small, None, 30)
        );
        let full =
            args("--rig full --spans 100000000 --rate 50 --seconds 60 --image main").unwrap();
        assert_eq!(
            (full.rig, full.spans, full.rate, full.seconds),
            (RigName::Full, Some(100_000_000), 50.0, 60)
        );
        assert!(args("--spans 0").is_err() && args("--spread-days 401").is_err());
        assert!(args("--rate -1").is_err() && args("--rate x").is_err());
        assert!(
            args("--lane x")
                .unwrap_err()
                .contains("unexpected argument")
        );
        let some = args("--lanes trace_by_id,trace_list_30d").unwrap();
        assert_eq!(
            some.lanes,
            Some(vec!["trace_by_id".to_owned(), "trace_list_30d".to_owned()])
        );
        assert!(
            args("--lanes nope")
                .unwrap_err()
                .contains("`nope` is not a lane")
        );
    }

    #[test]
    fn every_lane_is_a_valid_read_class() {
        for lane in &LANES {
            let class: serde_json::Value = serde_json::from_str(lane.class).unwrap();
            assert_eq!(class["name"], lane.name);
            if let Some(body) = class["body"].as_str() {
                serde_json::from_str::<serde_json::Value>(body).unwrap();
            }
        }
    }

    #[test]
    fn counters_are_read_from_prometheus_text() {
        let before = "crucible_api_store_reads_count{class=\"trace_by_id\"} 3\n\
                      crucible_api_store_reads_sum{class=\"trace_by_id\"} 18\n\
                      crucible_clickhouse_connections_opened_total 2\n";
        let after = "crucible_api_store_reads_count{class=\"trace_by_id\"} 13\n\
                     crucible_api_store_reads_sum{class=\"trace_by_id\"} 78\n\
                     crucible_api_barrier_wait_microseconds_count{class=\"trace_by_id\"} 10\n\
                     crucible_api_barrier_wait_microseconds_sum{class=\"trace_by_id\"} 500\n\
                     crucible_clickhouse_connections_opened_total 5\n";
        assert_eq!(
            sample(after, "crucible_clickhouse_connections_opened_total", ""),
            Some(5.0)
        );
        let report = serde_json::json!({"classes": [{"sent": 10, "ok": 10,
            "latency_us": {"count": 10, "p50": 1000, "p90": 2000, "p95": 2500, "p99": 3000}}]});
        let lane = judge_lane(&LANES[0], &report, before, after, "runs/x.json".to_owned());
        assert_eq!((lane.counted, lane.connections_opened), (10, 3));
        assert_eq!(lane.store_reads_per_request, Some(6.0));
        assert_eq!((lane.latency_us.p95, lane.latency_us.p99), (2_500, 3_000));
        assert_eq!(lane.barrier_wait_mean_us, Some(50.0));
        assert_eq!(lane.invalid, None);
        // A failed or empty answer invalidates the lane's latency.
        let short = serde_json::json!({"classes": [{"sent": 10, "ok": 9, "latency_us": {}}]});
        let lane = judge_lane(&LANES[0], &short, before, after, String::new());
        assert!(lane.invalid.unwrap().starts_with("1 of 10 requests"));
    }
}
