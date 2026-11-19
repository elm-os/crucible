//! Metrics (contract 2.12).
//!
//! Metric names and label keys are `&'static str` values that exist only in [`registry`]: the definition
//! types cannot be built outside this crate. [`MemoryRecorder`] is the one recorder; it never reads a clock,
//! so simulation invariants can read exact values from it, and production renders it as Prometheus text.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// A counter's definition.
#[derive(Debug)]
pub struct CounterDef {
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
}

/// A gauge's definition.
#[derive(Debug)]
pub struct GaugeDef {
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
}

/// A histogram's definition. `buckets` are inclusive upper bounds in increasing order; values above the last
/// bound land in the implicit overflow bucket.
#[derive(Debug)]
pub struct HistogramDef {
    name: &'static str,
    help: &'static str,
    labels: &'static [&'static str],
    buckets: &'static [u64],
}

macro_rules! def_accessors {
    ($ty:ty) => {
        impl $ty {
            /// The metric's name.
            pub const fn name(&self) -> &'static str {
                self.name
            }

            /// The one-line description rendered as `# HELP`.
            pub const fn help(&self) -> &'static str {
                self.help
            }

            /// The label keys, in the order label values are passed.
            pub const fn labels(&self) -> &'static [&'static str] {
                self.labels
            }
        }
    };
}
def_accessors!(CounterDef);
def_accessors!(GaugeDef);
def_accessors!(HistogramDef);

impl HistogramDef {
    /// The inclusive upper bounds of the buckets.
    pub const fn buckets(&self) -> &'static [u64] {
        self.buckets
    }
}

/// The single registry of metric names and label keys.
pub mod registry {
    use super::{CounterDef, GaugeDef, HistogramDef};

    /// Always 1; the label carries the binary's version.
    pub static BUILD_INFO: GaugeDef = GaugeDef {
        name: "crucible_build_info",
        help: "Build information; the value is always 1.",
        labels: &["version"],
    };

    /// Always 1; the labels say which `Disk` backend the WAL root uses and why `auto` fell back.
    pub static WAL_BACKEND_INFO: GaugeDef = GaugeDef {
        name: "crucible_wal_backend_info",
        help: "Selected WAL disk backend and, for a fallback, its reason; the value is always 1.",
        labels: &["requested", "backend", "fallback_reason"],
    };

    /// HTTP requests answered, by listener and status code.
    pub static HTTP_REQUESTS: CounterDef = CounterDef {
        name: "crucible_http_requests_total",
        help: "HTTP requests answered.",
        labels: &["listener", "status"],
    };

    /// Time from request receipt to response head, by listener.
    pub static HTTP_REQUEST_DURATION: HistogramDef = HistogramDef {
        name: "crucible_http_request_duration_microseconds",
        help: "Time from request receipt to response head, in microseconds.",
        labels: &["listener"],
        buckets: &[
            100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
            1_000_000, 5_000_000,
        ],
    };

    /// Failed `accept` calls, by listener.
    pub static NET_ACCEPT_ERRORS: CounterDef = CounterDef {
        name: "crucible_net_accept_errors_total",
        help: "Failed accept calls on a listener.",
        labels: &["listener"],
    };

    /// Frames the WAL committer appended.
    pub static WAL_APPENDED_FRAMES: CounterDef = CounterDef {
        name: "crucible_wal_appended_frames_total",
        help: "Frames appended to the WAL.",
        labels: &[],
    };

    /// Frame bytes the WAL committer appended, headers included.
    pub static WAL_APPENDED_BYTES: CounterDef = CounterDef {
        name: "crucible_wal_appended_bytes_total",
        help: "Frame bytes appended to the WAL.",
        labels: &[],
    };

    /// Commit groups, or timed syncs, whose append or sync failed.
    pub static WAL_GROUP_FAILURES: CounterDef = CounterDef {
        name: "crucible_wal_group_failures_total",
        help: "WAL commit groups whose append or sync failed.",
        labels: &[],
    };

    /// Duration of each WAL `sync_data`.
    pub static WAL_SYNC_DURATION: HistogramDef = HistogramDef {
        name: "crucible_wal_sync_duration_microseconds",
        help: "Duration of each WAL sync, in microseconds.",
        labels: &[],
        buckets: &[
            50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000,
            1_000_000, 5_000_000,
        ],
    };

    /// The last durable LSN.
    pub static WAL_DURABLE_LSN: GaugeDef = GaugeDef {
        name: "crucible_wal_durable_lsn",
        help: "Last durable WAL LSN.",
        labels: &[],
    };

    /// The checkpoint watermark.
    pub static WAL_CHECKPOINT_LSN: GaugeDef = GaugeDef {
        name: "crucible_wal_checkpoint_lsn",
        help: "WAL checkpoint watermark: every frame at or below it is resolved.",
        labels: &[],
    };

    /// Total bytes of the retained WAL segments.
    pub static WAL_SEGMENT_BYTES: GaugeDef = GaugeDef {
        name: "crucible_wal_segment_bytes",
        help: "Total bytes of the retained WAL segments.",
        labels: &[],
    };

    /// Bytes of the WAL segments that hold a frame above the checkpoint watermark.
    pub static WAL_UNSHIPPED_BYTES: GaugeDef = GaugeDef {
        name: "crucible_wal_unshipped_bytes",
        help: "Bytes of the WAL segments that hold a frame above the checkpoint watermark.",
        labels: &[],
    };

    /// Free bytes on the WAL root at the last reading.
    pub static WAL_FREE_BYTES: GaugeDef = GaugeDef {
        name: "crucible_wal_free_bytes",
        help: "Free bytes on the WAL root at the last reading.",
        labels: &[],
    };

    /// 1 while the WAL is in the failed state, otherwise 0.
    pub static WAL_FAILED: GaugeDef = GaugeDef {
        name: "crucible_wal_failed",
        help: "1 while the WAL is in the failed state and ingest is refused, otherwise 0.",
        labels: &[],
    };

    /// WAL segments removed by garbage collection.
    pub static WAL_GC_REMOVED_SEGMENTS: CounterDef = CounterDef {
        name: "crucible_wal_gc_removed_segments_total",
        help: "WAL segments removed by garbage collection.",
        labels: &[],
    };

    /// OTLP export requests answered, by outcome (`accepted`, `partial`, or the reason for a refusal).
    pub static INGEST_REQUESTS: CounterDef = CounterDef {
        name: "crucible_ingest_requests_total",
        help: "OTLP export requests answered, by outcome.",
        labels: &["outcome"],
    };

    /// Spans of acknowledged OTLP export requests, by outcome (`accepted` or a partial-success reason).
    pub static INGEST_SPANS: CounterDef = CounterDef {
        name: "crucible_ingest_spans_total",
        help: "Spans of acknowledged OTLP export requests, by outcome.",
        labels: &["outcome"],
    };

    /// Request bytes held by ingest handlers, tracked against the `ingest.inflight_bytes` budget.
    pub static INGEST_INFLIGHT_BYTES: GaugeDef = GaugeDef {
        name: "crucible_ingest_inflight_bytes",
        help: "Request bytes held by ingest handlers, tracked against the ingest.inflight_bytes budget.",
        labels: &[],
    };

    /// Fact writes answered, by outcome (`accepted`, or the reason for a refusal).
    pub static INGEST_FACT_WRITES: CounterDef = CounterDef {
        name: "crucible_ingest_fact_writes_total",
        help: "Fact writes answered, by outcome.",
        labels: &["outcome"],
    };

    /// Facts made durable in the WAL.
    pub static INGEST_FACTS: CounterDef = CounterDef {
        name: "crucible_ingest_facts_total",
        help: "Facts made durable in the WAL.",
        labels: &[],
    };

    /// Batches the shipper sealed or shipped again, by kind (`sealed`: fresh boundaries; `reshipped`: a sealed
    /// batch found by recovery).
    pub static SHIPPER_BATCHES: CounterDef = CounterDef {
        name: "crucible_shipper_batches_total",
        help: "Batches the shipper sealed, or shipped again after a restart, by kind.",
        labels: &["kind"],
    };

    /// Checkpoints the shipper appended.
    pub static SHIPPER_CHECKPOINTS: CounterDef = CounterDef {
        name: "crucible_shipper_checkpoints_total",
        help: "Checkpoints the shipper appended.",
        labels: &[],
    };

    /// Inserts the shipper sent, and of those the ones whose outcome was unknown (`outcome`: `sent`,
    /// `unknown`).
    pub static SHIPPER_INSERTS: CounterDef = CounterDef {
        name: "crucible_shipper_inserts_total",
        help: "Inserts the shipper sent, and those whose outcome was unknown.",
        labels: &["outcome"],
    };

    /// Existence probes, by result (`sent`, `present`, `expired`: an absent answer that outlived its validity).
    pub static SHIPPER_PROBES: CounterDef = CounterDef {
        name: "crucible_shipper_probes_total",
        help: "Existence probes the shipper sent, and their notable results.",
        labels: &["result"],
    };

    /// (Batch, table, partition) pairs quarantined after repeated data errors.
    pub static SHIPPER_QUARANTINED_PAIRS: CounterDef = CounterDef {
        name: "crucible_shipper_quarantined_pairs_total",
        help: "Pairs quarantined after repeated data errors.",
        labels: &[],
    };

    /// Durable frames this binary could not decode or normalize, copied to the quarantine root.
    pub static SHIPPER_QUARANTINED_FRAMES: CounterDef = CounterDef {
        name: "crucible_shipper_quarantined_frames_total",
        help: "Durable frames this binary could not decode or normalize, copied to the quarantine root.",
        labels: &[],
    };

    /// Batches whose pairs were built across a restart with another normalizer or price table version than
    /// their seal records (invariant I7's stated exception).
    pub static SHIPPER_MIXED_VERSION_BATCHES: CounterDef = CounterDef {
        name: "crucible_shipper_mixed_version_batches_total",
        help: "Batches shipped again with another normalizer or price table version than their seal records.",
        labels: &[],
    };

    /// Estimated bytes of the rows the shipper holds, tracked against the `shipper.encoded_bytes` budget.
    pub static SHIPPER_ENCODED_BYTES: GaugeDef = GaugeDef {
        name: "crucible_shipper_encoded_bytes",
        help: "Estimated bytes of the rows the shipper holds, tracked against the shipper.encoded_bytes budget.",
        labels: &[],
    };

    /// Raw WAL bytes per second that checkpoints resolved over the last ten seconds.
    pub static SHIPPER_DRAIN_BYTES_PER_SECOND: GaugeDef = GaugeDef {
        name: "crucible_shipper_drain_bytes_per_second",
        help: "Raw WAL bytes per second that checkpoints resolved over the last ten seconds.",
        labels: &[],
    };

    /// Seconds since the API key snapshot last loaded or polled successfully (contract 11.8); -1 before the
    /// first load.
    pub static AUTH_KEY_SNAPSHOT_AGE: GaugeDef = GaugeDef {
        name: "crucible_auth_key_snapshot_age_seconds",
        help: "Seconds since the API key snapshot last loaded or polled successfully; -1 before the first load.",
        labels: &[],
    };

    /// Connections the telemetry store opened to ClickHouse, counted at `Net::connect`.
    pub static CLICKHOUSE_CONNECTIONS_OPENED: CounterDef = CounterDef {
        name: "crucible_clickhouse_connections_opened_total",
        help: "Connections the telemetry store opened to ClickHouse.",
        labels: &[],
    };

    /// Store reads each request of the read services issued, by request class (`crucible-query`'s
    /// `READ_CLASSES`): the count is the requests, the sum the reads.
    pub static API_STORE_READS: HistogramDef = HistogramDef {
        name: "crucible_api_store_reads",
        help: "Store reads each request of the read services issued, by request class.",
        labels: &["class"],
        buckets: &[0, 1, 2, 3, 4, 5, 6, 8, 10, 15, 20, 50, 100],
    };

    /// How long each request of the read services that waits for the recent index's read barrier waited, by
    /// request class, timed by the caller with the `Env` clock.
    pub static API_BARRIER_WAIT: HistogramDef = HistogramDef {
        name: "crucible_api_barrier_wait_microseconds",
        help: "How long each read that waits for the recent index's read barrier waited, in microseconds.",
        labels: &["class"],
        buckets: &[
            0, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000,
        ],
    };

    /// Every definition's name, label keys and bucket bounds, for the registry's consistency test.
    pub fn all() -> Vec<(&'static str, &'static [&'static str], &'static [u64])> {
        vec![
            (BUILD_INFO.name, BUILD_INFO.labels, &[]),
            (
                AUTH_KEY_SNAPSHOT_AGE.name,
                AUTH_KEY_SNAPSHOT_AGE.labels,
                &[],
            ),
            (WAL_BACKEND_INFO.name, WAL_BACKEND_INFO.labels, &[]),
            (HTTP_REQUESTS.name, HTTP_REQUESTS.labels, &[]),
            (
                HTTP_REQUEST_DURATION.name,
                HTTP_REQUEST_DURATION.labels,
                HTTP_REQUEST_DURATION.buckets,
            ),
            (NET_ACCEPT_ERRORS.name, NET_ACCEPT_ERRORS.labels, &[]),
            (WAL_APPENDED_FRAMES.name, WAL_APPENDED_FRAMES.labels, &[]),
            (WAL_APPENDED_BYTES.name, WAL_APPENDED_BYTES.labels, &[]),
            (WAL_GROUP_FAILURES.name, WAL_GROUP_FAILURES.labels, &[]),
            (
                WAL_SYNC_DURATION.name,
                WAL_SYNC_DURATION.labels,
                WAL_SYNC_DURATION.buckets,
            ),
            (WAL_DURABLE_LSN.name, WAL_DURABLE_LSN.labels, &[]),
            (WAL_CHECKPOINT_LSN.name, WAL_CHECKPOINT_LSN.labels, &[]),
            (WAL_SEGMENT_BYTES.name, WAL_SEGMENT_BYTES.labels, &[]),
            (WAL_UNSHIPPED_BYTES.name, WAL_UNSHIPPED_BYTES.labels, &[]),
            (WAL_FREE_BYTES.name, WAL_FREE_BYTES.labels, &[]),
            (WAL_FAILED.name, WAL_FAILED.labels, &[]),
            (
                WAL_GC_REMOVED_SEGMENTS.name,
                WAL_GC_REMOVED_SEGMENTS.labels,
                &[],
            ),
            (INGEST_REQUESTS.name, INGEST_REQUESTS.labels, &[]),
            (INGEST_SPANS.name, INGEST_SPANS.labels, &[]),
            (
                INGEST_INFLIGHT_BYTES.name,
                INGEST_INFLIGHT_BYTES.labels,
                &[],
            ),
            (INGEST_FACT_WRITES.name, INGEST_FACT_WRITES.labels, &[]),
            (INGEST_FACTS.name, INGEST_FACTS.labels, &[]),
            (SHIPPER_BATCHES.name, SHIPPER_BATCHES.labels, &[]),
            (SHIPPER_CHECKPOINTS.name, SHIPPER_CHECKPOINTS.labels, &[]),
            (SHIPPER_INSERTS.name, SHIPPER_INSERTS.labels, &[]),
            (SHIPPER_PROBES.name, SHIPPER_PROBES.labels, &[]),
            (
                SHIPPER_QUARANTINED_PAIRS.name,
                SHIPPER_QUARANTINED_PAIRS.labels,
                &[],
            ),
            (
                SHIPPER_QUARANTINED_FRAMES.name,
                SHIPPER_QUARANTINED_FRAMES.labels,
                &[],
            ),
            (
                SHIPPER_MIXED_VERSION_BATCHES.name,
                SHIPPER_MIXED_VERSION_BATCHES.labels,
                &[],
            ),
            (
                SHIPPER_ENCODED_BYTES.name,
                SHIPPER_ENCODED_BYTES.labels,
                &[],
            ),
            (
                SHIPPER_DRAIN_BYTES_PER_SECOND.name,
                SHIPPER_DRAIN_BYTES_PER_SECOND.labels,
                &[],
            ),
            (
                CLICKHOUSE_CONNECTIONS_OPENED.name,
                CLICKHOUSE_CONNECTIONS_OPENED.labels,
                &[],
            ),
            (
                API_STORE_READS.name,
                API_STORE_READS.labels,
                API_STORE_READS.buckets,
            ),
            (
                API_BARRIER_WAIT.name,
                API_BARRIER_WAIT.labels,
                API_BARRIER_WAIT.buckets,
            ),
        ]
    }
}

/// A monotonically increasing count.
#[derive(Clone, Debug)]
pub struct Counter(Arc<AtomicU64>);

impl Counter {
    /// Adds `n`.
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }

    /// The current value.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// A value that can go up and down.
#[derive(Clone, Debug)]
pub struct Gauge(Arc<AtomicI64>);

impl Gauge {
    /// Sets the value.
    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }

    /// The current value.
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct HistogramCells {
    bounds: &'static [u64],
    /// One cell per bound plus the overflow cell.
    buckets: Vec<AtomicU64>,
    sum: AtomicU64,
}

/// A distribution over fixed integer buckets.
#[derive(Clone, Debug)]
pub struct Histogram(Arc<HistogramCells>);

/// A histogram's contents at one moment.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HistogramSnapshot {
    /// Observations per bucket, not cumulative; the last entry is the overflow bucket.
    pub buckets: Vec<u64>,
    /// The sum of all observed values (wrapping).
    pub sum: u64,
    /// The number of observations.
    pub count: u64,
}

impl Histogram {
    fn new(def: &'static HistogramDef) -> Self {
        let buckets = (0..=def.buckets.len()).map(|_| AtomicU64::new(0)).collect();
        Self(Arc::new(HistogramCells {
            bounds: def.buckets,
            buckets,
            sum: AtomicU64::new(0),
        }))
    }

    /// Records one observation.
    pub fn record(&self, v: u64) {
        let index = self.0.bounds.partition_point(|bound| *bound < v);
        if let Some(cell) = self.0.buckets.get(index) {
            cell.fetch_add(1, Ordering::Relaxed);
        }
        self.0.sum.fetch_add(v, Ordering::Relaxed);
    }

    /// The current contents.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let buckets: Vec<u64> = self
            .0
            .buckets
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let count = buckets.iter().sum();
        HistogramSnapshot {
            buckets,
            sum: self.0.sum.load(Ordering::Relaxed),
            count,
        }
    }
}

/// Hands out metric handles. `labels` are the values for the definition's label keys, in order; a missing
/// value is recorded as the empty string and surplus values are ignored. Handles are cheap to clone and
/// should be fetched once and kept, not looked up per event.
pub trait Metrics: fmt::Debug + Send + Sync {
    /// The counter for this label set.
    fn counter(&self, def: &'static CounterDef, labels: &[&str]) -> Counter;
    /// The gauge for this label set.
    fn gauge(&self, def: &'static GaugeDef, labels: &[&str]) -> Gauge;
    /// The histogram for this label set.
    fn histogram(&self, def: &'static HistogramDef, labels: &[&str]) -> Histogram;
}

#[derive(Debug)]
enum Series {
    Counter(&'static CounterDef, Counter),
    Gauge(&'static GaugeDef, Gauge),
    Histogram(&'static HistogramDef, Histogram),
}

type SeriesKey = (&'static str, Vec<String>);

/// The in-memory recorder used by production and simulation. Series are kept sorted by name and label
/// values, so reads and renders are deterministic.
#[derive(Debug, Default)]
pub struct MemoryRecorder {
    series: Mutex<BTreeMap<SeriesKey, Series>>,
}

fn key(name: &'static str, keys: &[&str], values: &[&str]) -> SeriesKey {
    debug_assert_eq!(keys.len(), values.len(), "label values for {name}");
    let values = (0..keys.len())
        .map(|i| values.get(i).copied().unwrap_or("").to_owned())
        .collect();
    (name, values)
}

impl MemoryRecorder {
    /// Creates an empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SeriesKey, Series>> {
        self.series.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The counter's value, or 0 when the series was never touched.
    pub fn counter_value(&self, def: &'static CounterDef, labels: &[&str]) -> u64 {
        match self.lock().get(&key(def.name, def.labels, labels)) {
            Some(Series::Counter(_, c)) => c.get(),
            _ => 0,
        }
    }

    /// The gauge's value, or `None` when the series was never touched.
    pub fn gauge_value(&self, def: &'static GaugeDef, labels: &[&str]) -> Option<i64> {
        match self.lock().get(&key(def.name, def.labels, labels)) {
            Some(Series::Gauge(_, g)) => Some(g.get()),
            _ => None,
        }
    }

    /// The histogram's contents, or `None` when the series was never touched.
    pub fn histogram_value(
        &self,
        def: &'static HistogramDef,
        labels: &[&str],
    ) -> Option<HistogramSnapshot> {
        match self.lock().get(&key(def.name, def.labels, labels)) {
            Some(Series::Histogram(_, h)) => Some(h.snapshot()),
            _ => None,
        }
    }

    /// Renders every series in the Prometheus text exposition format (version 0.0.4), sorted by metric name
    /// and label values.
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        let mut previous = "";
        for ((name, values), series) in self.lock().iter() {
            let (help, kind, keys) = match series {
                Series::Counter(def, _) => (def.help, "counter", def.labels),
                Series::Gauge(def, _) => (def.help, "gauge", def.labels),
                Series::Histogram(def, _) => (def.help, "histogram", def.labels),
            };
            if *name != previous {
                // Writing to a String cannot fail.
                let _ = writeln!(
                    out,
                    "# HELP {name} {}\n# TYPE {name} {kind}",
                    escape(help, false)
                );
                previous = name;
            }
            match series {
                Series::Counter(_, c) => sample(&mut out, name, "", keys, values, None, &c.get()),
                Series::Gauge(_, g) => sample(&mut out, name, "", keys, values, None, &g.get()),
                Series::Histogram(def, h) => {
                    let snapshot = h.snapshot();
                    let mut cumulative = 0;
                    for (i, n) in snapshot.buckets.iter().enumerate() {
                        cumulative += n;
                        let le = def
                            .buckets
                            .get(i)
                            .map_or_else(|| "+Inf".to_owned(), u64::to_string);
                        sample(
                            &mut out,
                            name,
                            "_bucket",
                            keys,
                            values,
                            Some(&le),
                            &cumulative,
                        );
                    }
                    sample(&mut out, name, "_sum", keys, values, None, &snapshot.sum);
                    sample(
                        &mut out,
                        name,
                        "_count",
                        keys,
                        values,
                        None,
                        &snapshot.count,
                    );
                }
            }
        }
        out
    }
}

fn sample(
    out: &mut String,
    name: &str,
    suffix: &str,
    keys: &[&str],
    values: &[String],
    le: Option<&str>,
    value: &dyn fmt::Display,
) {
    out.push_str(name);
    out.push_str(suffix);
    let pairs = keys
        .iter()
        .copied()
        .zip(values.iter().map(String::as_str))
        .chain(le.map(|le| ("le", le)));
    let mut any = false;
    for (k, v) in pairs {
        let _ = write!(
            out,
            "{}{k}=\"{}\"",
            if any { ',' } else { '{' },
            escape(v, true)
        );
        any = true;
    }
    if any {
        out.push('}');
    }
    let _ = writeln!(out, " {value}");
}

fn escape(text: &str, quotes: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '"' if quotes => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out
}

impl Metrics for MemoryRecorder {
    fn counter(&self, def: &'static CounterDef, labels: &[&str]) -> Counter {
        let mut series = self.lock();
        let entry = series
            .entry(key(def.name, def.labels, labels))
            .or_insert_with(|| Series::Counter(def, Counter(Arc::default())));
        match entry {
            Series::Counter(_, c) => c.clone(),
            // Unreachable while registry names are unique; a detached handle keeps the call infallible.
            _ => Counter(Arc::default()),
        }
    }

    fn gauge(&self, def: &'static GaugeDef, labels: &[&str]) -> Gauge {
        let mut series = self.lock();
        let entry = series
            .entry(key(def.name, def.labels, labels))
            .or_insert_with(|| Series::Gauge(def, Gauge(Arc::default())));
        match entry {
            Series::Gauge(_, g) => g.clone(),
            _ => Gauge(Arc::default()),
        }
    }

    fn histogram(&self, def: &'static HistogramDef, labels: &[&str]) -> Histogram {
        let mut series = self.lock();
        let entry = series
            .entry(key(def.name, def.labels, labels))
            .or_insert_with(|| Series::Histogram(def, Histogram::new(def)));
        match entry {
            Series::Histogram(_, h) => h.clone(),
            _ => Histogram::new(def),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::registry::{BUILD_INFO, HTTP_REQUEST_DURATION, HTTP_REQUESTS, NET_ACCEPT_ERRORS};
    use super::*;

    #[test]
    fn registry_names_are_unique_and_well_formed() {
        let all = registry::all();
        let names: BTreeSet<_> = all.iter().map(|(name, ..)| *name).collect();
        assert_eq!(names.len(), all.len(), "duplicate metric name");
        let ident = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !s.starts_with(|c: char| c.is_ascii_digit())
        };
        for (name, labels, buckets) in all {
            assert!(ident(name) && name.starts_with("crucible_"), "{name}");
            for label in labels {
                assert!(ident(label) && *label != "le", "{name} label {label}");
            }
            assert!(
                buckets.windows(2).all(|w| w[0] < w[1]),
                "{name} buckets must increase"
            );
        }
    }

    #[test]
    fn handles_for_one_label_set_share_a_series() {
        let recorder = MemoryRecorder::new();
        let a = recorder.counter(&HTTP_REQUESTS, &["api", "200"]);
        let b = recorder.counter(&HTTP_REQUESTS, &["api", "200"]);
        let other = recorder.counter(&HTTP_REQUESTS, &["api", "503"]);
        a.add(2);
        b.add(3);
        other.add(1);
        assert_eq!(recorder.counter_value(&HTTP_REQUESTS, &["api", "200"]), 5);
        assert_eq!(recorder.counter_value(&HTTP_REQUESTS, &["api", "503"]), 1);
        assert_eq!(
            recorder.counter_value(&HTTP_REQUESTS, &["metrics", "200"]),
            0
        );
    }

    #[test]
    fn gauges_hold_the_last_value() {
        let recorder = MemoryRecorder::new();
        assert_eq!(recorder.gauge_value(&BUILD_INFO, &["1.2.3"]), None);
        let gauge = recorder.gauge(&BUILD_INFO, &["1.2.3"]);
        gauge.set(7);
        gauge.set(-1);
        assert_eq!(recorder.gauge_value(&BUILD_INFO, &["1.2.3"]), Some(-1));
    }

    #[test]
    fn histogram_bounds_are_inclusive_and_overflow_is_counted() {
        let recorder = MemoryRecorder::new();
        let h = recorder.histogram(&HTTP_REQUEST_DURATION, &["api"]);
        for v in [0, 100, 101, 250, 5_000_000, 5_000_001, u64::from(u32::MAX)] {
            h.record(v);
        }
        let snapshot = recorder
            .histogram_value(&HTTP_REQUEST_DURATION, &["api"])
            .unwrap();
        assert_eq!(snapshot.count, 7);
        assert_eq!(
            snapshot.sum,
            100 + 101 + 250 + 5_000_000 + 5_000_001 + u64::from(u32::MAX)
        );
        assert_eq!(snapshot.buckets[..2], [2, 2]);
        assert_eq!(snapshot.buckets[13..], [1, 2]);
        assert_eq!(
            snapshot.buckets.len(),
            HTTP_REQUEST_DURATION.buckets().len() + 1
        );
    }

    #[test]
    fn prometheus_text_is_sorted_escaped_and_cumulative() {
        let recorder = MemoryRecorder::new();
        recorder.counter(&NET_ACCEPT_ERRORS, &["api"]).add(1);
        recorder.counter(&HTTP_REQUESTS, &["metrics", "200"]).add(4);
        recorder.counter(&HTTP_REQUESTS, &["api", "200"]).add(2);
        recorder.gauge(&BUILD_INFO, &["0.1 \"q\"\\\n"]).set(1);
        let h = recorder.histogram(&HTTP_REQUEST_DURATION, &["api"]);
        h.record(90);
        h.record(400);
        h.record(9_000_000);

        let text = recorder.render_prometheus();
        let expected_head = "\
# HELP crucible_build_info Build information; the value is always 1.
# TYPE crucible_build_info gauge
crucible_build_info{version=\"0.1 \\\"q\\\"\\\\\\n\"} 1
# HELP crucible_http_request_duration_microseconds Time from request receipt to response head, in microseconds.
# TYPE crucible_http_request_duration_microseconds histogram
crucible_http_request_duration_microseconds_bucket{listener=\"api\",le=\"100\"} 1
crucible_http_request_duration_microseconds_bucket{listener=\"api\",le=\"250\"} 1
crucible_http_request_duration_microseconds_bucket{listener=\"api\",le=\"500\"} 2
";
        assert!(text.starts_with(expected_head), "{text}");
        let expected_tail = "\
crucible_http_request_duration_microseconds_bucket{listener=\"api\",le=\"5000000\"} 2
crucible_http_request_duration_microseconds_bucket{listener=\"api\",le=\"+Inf\"} 3
crucible_http_request_duration_microseconds_sum{listener=\"api\"} 9000490
crucible_http_request_duration_microseconds_count{listener=\"api\"} 3
# HELP crucible_http_requests_total HTTP requests answered.
# TYPE crucible_http_requests_total counter
crucible_http_requests_total{listener=\"api\",status=\"200\"} 2
crucible_http_requests_total{listener=\"metrics\",status=\"200\"} 4
# HELP crucible_net_accept_errors_total Failed accept calls on a listener.
# TYPE crucible_net_accept_errors_total counter
crucible_net_accept_errors_total{listener=\"api\"} 1
";
        assert!(text.ends_with(expected_tail), "{text}");
        assert_eq!(
            text,
            recorder.render_prometheus(),
            "rendering must be repeatable"
        );
    }
}
