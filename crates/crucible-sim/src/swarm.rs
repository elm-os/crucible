//! Swarm testing for the system simulation: the coin-flip dimensions, the configuration mask, the run records
//! of the regression corpus and the per-dimension report.
//!
//! A configuration is a bit mask with one bit per [`Dim`]. The mask is an input of a run, next to the seed: the
//! mask decides which workload features, faults and config flags exist in the run, and the seed decides
//! everything else. A run is replayed from the pair.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crucible_env::{ChaChaRng, Rng};

/// What a dimension switches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A feature of the client traffic.
    Workload,
    /// A fault or a crash point.
    Fault,
    /// A configuration flag of the node.
    Config,
}

impl Kind {
    /// The name used in the report.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Workload => "workload",
            Self::Fault => "fault",
            Self::Config => "config",
        }
    }
}

macro_rules! dimensions {
    ($($variant:ident, $name:literal, $kind:ident, $about:literal;)*) => {
        /// One coin of a configuration. The discriminant is the bit of the mask.
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub enum Dim {
            $(#[doc = $about] $variant,)*
        }

        impl Dim {
            /// Every dimension, in bit order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            /// The name used in records and reports.
            pub const fn name(self) -> &'static str {
                match self { $(Self::$variant => $name,)* }
            }

            /// What the dimension switches.
            pub const fn kind(self) -> Kind {
                match self { $(Self::$variant => Kind::$kind,)* }
            }

            /// What the run does when the coin is heads.
            pub const fn about(self) -> &'static str {
                match self { $(Self::$variant => $about,)* }
            }
        }
    };
}

dimensions! {
    Json, "json", Workload, "Clients send JSON bodies as well as protobuf.";
    LargePayloads, "large_payloads", Workload, "Span attributes of several size classes, up to a few KiB.";
    DeepNesting, "deep_nesting", Workload, "Attribute values nested up to 90 levels.";
    BadUnicode, "bad_unicode", Workload, "Names and attribute strings that are not valid UTF-8 (or lone surrogates in JSON).";
    Rootless, "rootless", Workload, "Some traces never get a root span.";
    ConcurrentReads, "concurrent_reads", Workload, "Reader tasks run trace-by-id and trace lists beside the writers.";
    ClientRetries, "client_retries", Workload, "Clients send earlier requests again.";
    MultiProject, "multi_project", Workload, "Three projects, which also share trace ids.";
    Bursts, "bursts", Workload, "A client keeps several requests in flight.";
    MultiDay, "multi_day", Workload, "Span start times spread over several days, so over several partitions.";
    DiskCrashPoints, "disk_crash_points", Fault, "Crashes at a seeded disk operation, which includes the WAL barriers.";
    BarrierCrashes, "barrier_crashes", Fault, "Crashes at a crash barrier of the shipper.";
    CrashLoops, "crash_loops", Fault, "Many short incarnations that crash within milliseconds of their recovery.";
    CrashInRecovery, "crash_in_recovery", Fault, "Crashes while the WAL recovers.";
    CleanStops, "clean_stops", Fault, "Some incarnations end with a process stop instead of a crash.";
    DiskWriteErrors, "disk_write_errors", Fault, "Failed and short disk writes.";
    DiskReadFaults, "disk_read_faults", Fault, "Failed disk reads and transient bit flips.";
    DiskSyncErrors, "disk_sync_errors", Fault, "Failed syncs.";
    DiskFull, "disk_full", Fault, "A disk with little capacity.";
    SlowSync, "slow_sync", Fault, "Syncs that take milliseconds.";
    ChTimeouts, "ch_timeouts", Fault, "Insert timeouts: lost before apply, or applied late.";
    ChAckLost, "ch_ack_lost", Fault, "Inserts that apply while their ack is lost.";
    ChServerErrors, "ch_server_errors", Fault, "Server errors on inserts.";
    ChRejects, "ch_rejects", Fault, "Too-many-parts, memory and concurrency rejections.";
    ChHangOverload, "ch_hang_overload", Fault, "Inserts that hang, and overload that sheds inserts after a timeout.";
    ChPoison, "ch_poison", Fault, "Poison pairs, which end in quarantine.";
    ChProbeErrors, "ch_probe_errors", Fault, "Existence probes that fail.";
    ChReadErrors, "ch_read_errors", Fault, "Reads that the store does not answer.";
    WindowExpiry, "window_expiry", Fault, "Deduplication log entries expire early.";
    Outages, "outages", Fault, "Store outages of seconds.";
    LongOutages, "long_outages", Fault, "Store outages of minutes.";
    WallClockSteps, "wall_clock_steps", Fault, "The wall clock is skewed and steps between incarnations.";
    WalCompressionOff, "wal_compression_off", Config, "`wal.compression = none` instead of LZ4.";
    SmallBatches, "small_batches", Config, "Batches seal after a few rows or bytes.";
    WeekPartitions, "week_partitions", Config, "Weekly instead of daily partitions.";
    TinyRecent, "tiny_recent", Config, "A recent-index bound of a few KiB, so entries are dropped and gaps open.";
    SmallSegments, "small_segments", Config, "WAL segments of 8 or 16 KiB.";
    Clustered, "clustered", Config, "The clustered store flavour: an age window on the deduplication log and a 10 s grace.";
    OneConnection, "one_connection", Config, "One insert connection and one batch in flight.";
    ReplayRetention, "replay_retention", Config, "The WAL keeps checkpointed segments for 15 minutes.";
    FsyncLie, "fsync_lie", Fault, "Stress tier only: the disk reports syncs that did not happen.";
}

/// A configuration: one bit per [`Dim`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ConfigMask(pub u64);

impl ConfigMask {
    /// The mask with every dimension on.
    pub const FULL: Self = Self((1 << Dim::ALL.len()) - 1);

    /// One independent fair coin per dimension, from the stream of `config_seed`.
    pub fn draw(config_seed: u64) -> Self {
        let rng = ChaChaRng::from_u64(config_seed).fork("swarm/config");
        Self(rng.next_u64() & Self::FULL.0)
    }

    /// Whether the coin of `dim` is heads.
    pub const fn has(self, dim: Dim) -> bool {
        self.0 & (1 << dim as u64) != 0
    }

    /// The mask with `dim` on.
    #[must_use]
    pub const fn with(self, dim: Dim) -> Self {
        Self(self.0 | (1 << dim as u64))
    }

    /// The mask with `dim` off.
    #[must_use]
    pub const fn without(self, dim: Dim) -> Self {
        Self(self.0 & !(1 << dim as u64))
    }

    /// The names of the dimensions that are on.
    pub fn names(self) -> Vec<&'static str> {
        Dim::ALL
            .iter()
            .filter(|&&dim| self.has(dim))
            .map(|dim| dim.name())
            .collect()
    }
}

/// The runs of a budget: `configs` configurations starting at configuration seed `base`, each with `seeds` run
/// seeds. Configuration seed `c` gives the mask [`ConfigMask::draw`]`(c)` and the run seeds `c * 1000 + s`, so
/// [`config_of_seed`] finds the mask of a run seed again.
pub fn budget(base: u64, configs: u64, seeds: u64) -> impl Iterator<Item = (ConfigMask, u64)> {
    (base..base.saturating_add(configs)).flat_map(move |config| {
        (0..seeds.min(1_000)).map(move |s| {
            (
                ConfigMask::draw(config),
                config.wrapping_mul(1_000).wrapping_add(s),
            )
        })
    })
}

/// The mask that [`budget`] pairs with a run seed.
pub fn config_of_seed(seed: u64) -> ConfigMask {
    ConfigMask::draw(seed / 1_000)
}

/// One line of a run log or of the regression corpus `sim/corpus/seeds.jsonl`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunRecord {
    /// The configuration mask.
    pub config: u64,
    /// The run seed.
    pub seed: u64,
    /// `realistic` or `stress`.
    pub tier: String,
    /// `pass`, `fail`, or `loss-expected` for a stress run under the fsync lie whose violations follow from it.
    pub outcome: String,
    /// The violated invariant (`I1`, `I2`, ...), when the run failed.
    pub invariant: Option<String>,
    /// The hash of the run's event trace.
    pub trace_hash: String,
    /// The coverage targets the run reached.
    pub targets: Vec<String>,
    /// Why the line is in the corpus: `failed` (with the defect it found) or `sample`.
    pub note: String,
}

fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl RunRecord {
    /// The record as one line of JSON.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "{{\"config\":\"{:#x}\",\"seed\":{},\"tier\":",
            self.config, self.seed
        );
        json_string(&mut out, &self.tier);
        out.push_str(",\"outcome\":");
        json_string(&mut out, &self.outcome);
        out.push_str(",\"invariant\":");
        match &self.invariant {
            Some(invariant) => json_string(&mut out, invariant),
            None => out.push_str("null"),
        }
        out.push_str(",\"trace_hash\":");
        json_string(&mut out, &self.trace_hash);
        out.push_str(",\"targets\":[");
        for (i, target) in self.targets.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_string(&mut out, target);
        }
        out.push_str("],\"note\":");
        json_string(&mut out, &self.note);
        out.push('}');
        out
    }

    /// Parses a line that [`RunRecord::to_json`] wrote.
    pub fn parse(line: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let text = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("no `{name}`"))
        };
        let config = text("config")?;
        let config = u64::from_str_radix(config.trim_start_matches("0x"), 16)
            .map_err(|e| format!("config: {e}"))?;
        Ok(Self {
            config,
            seed: value
                .get("seed")
                .and_then(serde_json::Value::as_u64)
                .ok_or("no `seed`")?,
            tier: text("tier")?,
            outcome: text("outcome")?,
            invariant: text("invariant").ok(),
            trace_hash: text("trace_hash")?,
            targets: value
                .get("targets")
                .and_then(serde_json::Value::as_array)
                .map(|targets| {
                    targets
                        .iter()
                        .filter_map(|t| t.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            note: text("note").unwrap_or_default(),
        })
    }
}

/// How often a target was reached with a dimension on and off.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Split {
    on_hits: u64,
    on_runs: u64,
    off_hits: u64,
    off_runs: u64,
}

/// The Wilson score interval of `hits` out of `runs` at 95%.
fn wilson(hits: u64, runs: u64) -> (f64, f64) {
    if runs == 0 {
        return (0.0, 1.0);
    }
    let (n, p, z) = (runs as f64, hits as f64 / runs as f64, 1.96_f64);
    let centre = p + z * z / (2.0 * n);
    let spread = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt();
    let denominator = 1.0 + z * z / n;
    (
        ((centre - spread) / denominator).max(0.0),
        ((centre + spread) / denominator).min(1.0),
    )
}

/// The per-dimension trigger and suppressor summary of a set of runs, as Markdown.
///
/// A target is a coverage event of a run (a crash at a given barrier, a quarantine, an index gap, ...) or the
/// violation of an invariant. For each target, a dimension is a **trigger** when the target is reached more
/// often with the dimension on, and a **suppressor** when it is reached more often with it off; "more often"
/// means that the 95% Wilson intervals of the two rates do not overlap.
pub fn report(records: &[RunRecord]) -> String {
    let mut targets: BTreeMap<String, BTreeMap<Dim, Split>> = BTreeMap::new();
    let mut hits: BTreeMap<String, u64> = BTreeMap::new();
    let mut names: Vec<String> = Vec::new();
    for record in records {
        for target in &record.targets {
            if !names.contains(target) {
                names.push(target.clone());
            }
        }
        if let Some(invariant) = &record.invariant {
            let name = format!("violation of {invariant}");
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names.sort();
    for name in &names {
        let per_dim = targets.entry(name.clone()).or_default();
        for record in records {
            let hit = record.targets.contains(name)
                || record
                    .invariant
                    .as_ref()
                    .is_some_and(|i| *name == format!("violation of {i}"));
            *hits.entry(name.clone()).or_default() += u64::from(hit);
            for &dim in Dim::ALL {
                let split = per_dim.entry(dim).or_default();
                if ConfigMask(record.config).has(dim) {
                    split.on_runs += 1;
                    split.on_hits += u64::from(hit);
                } else {
                    split.off_runs += 1;
                    split.off_hits += u64::from(hit);
                }
            }
        }
    }

    let total = records.len() as u64;
    let count = |what: &str| records.iter().filter(|r| r.outcome == what).count();
    let mut out = String::new();
    out.push_str("# Swarm report\n\n");
    out.push_str(
        "Written by `cargo xtask sim report` from the run log of `cargo xtask sim run`. The method is in\n\
         [`system-simulation.md`](system-simulation.md).\n\n",
    );
    let _ = writeln!(
        out,
        "Runs: {total} ({} passed, {} failed, {} with losses expected under the fsync lie).\n",
        count("pass"),
        count("fail"),
        count("loss-expected")
    );
    out.push_str("## Dimensions\n\n| Dimension | Kind | Runs with it on |\n|---|---|---|\n");
    for &dim in Dim::ALL {
        let on = records
            .iter()
            .filter(|r| ConfigMask(r.config).has(dim))
            .count();
        let _ = writeln!(out, "| `{}` | {} | {on} |", dim.name(), dim.kind().name());
    }
    out.push_str(
        "\n## Targets\n\nA trigger raises the rate of a target and a suppressor lowers it; a dimension is listed \
         when the 95% Wilson\nintervals of the two rates do not overlap. The rates are `on` and `off` in percent.\n\n\
         | Target | Runs | Triggers | Suppressors |\n|---|---|---|---|\n",
    );
    for name in &names {
        let mut triggers = Vec::new();
        let mut suppressors = Vec::new();
        if let Some(per_dim) = targets.get(name) {
            for (dim, split) in per_dim {
                let on = wilson(split.on_hits, split.on_runs);
                let off = wilson(split.off_hits, split.off_runs);
                let rate = |hits: u64, runs: u64| 100.0 * hits as f64 / runs.max(1) as f64;
                let cell = format!(
                    "`{}` ({:.0} / {:.0})",
                    dim.name(),
                    rate(split.on_hits, split.on_runs),
                    rate(split.off_hits, split.off_runs)
                );
                if on.0 > off.1 {
                    triggers.push(cell);
                } else if on.1 < off.0 {
                    suppressors.push(cell);
                }
            }
        }
        let list = |cells: Vec<String>| {
            if cells.is_empty() {
                "none".to_owned()
            } else {
                cells.join(", ")
            }
        };
        let _ = writeln!(
            out,
            "| {name} | {} | {} | {} |",
            hits.get(name).copied().unwrap_or(0),
            list(triggers),
            list(suppressors)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_through_its_json_line() {
        let record = RunRecord {
            config: 0x1_0000_0001,
            seed: 7,
            tier: "realistic".to_owned(),
            outcome: "fail".to_owned(),
            invariant: Some("I3".to_owned()),
            trace_hash: "ab\"c".to_owned(),
            targets: vec!["quarantine".to_owned(), "index gap".to_owned()],
            note: "sample".to_owned(),
        };
        assert_eq!(RunRecord::parse(&record.to_json()), Ok(record));
    }

    #[test]
    fn masks_are_a_function_of_their_seed_and_cover_every_dimension() {
        assert_eq!(ConfigMask::draw(3), ConfigMask::draw(3));
        let mut seen = 0;
        let mut unseen = ConfigMask::FULL.0;
        for seed in 0..64 {
            let mask = ConfigMask::draw(seed);
            assert_eq!(mask.0 & !ConfigMask::FULL.0, 0);
            seen |= mask.0;
            unseen &= mask.0;
        }
        assert_eq!((seen, unseen), (ConfigMask::FULL.0, 0));
        assert_eq!(Dim::ALL.len(), 41);
    }

    #[test]
    fn the_report_names_a_trigger_and_a_suppressor() {
        let run = |config: u64, hit: bool| RunRecord {
            config,
            seed: 0,
            tier: "realistic".to_owned(),
            outcome: "pass".to_owned(),
            invariant: None,
            trace_hash: String::new(),
            targets: if hit {
                vec!["quarantine".to_owned()]
            } else {
                Vec::new()
            },
            note: String::new(),
        };
        let poison = ConfigMask::default().with(Dim::ChPoison);
        let loops = ConfigMask::default().with(Dim::CrashLoops);
        let mut records = Vec::new();
        for _ in 0..40 {
            records.push(run(poison.0, true));
            records.push(run(loops.0, false));
        }
        let text = report(&records);
        let line = text
            .lines()
            .find(|line| line.starts_with("| quarantine"))
            .unwrap_or_default();
        assert!(line.contains("`ch_poison` (100 / 0)"), "{line}");
        assert!(line.contains("`crash_loops` (0 / 100)"), "{line}");
    }
}
