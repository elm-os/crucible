//! A node in miniature: a WAL log, stage B feeding the recent index, a shipper landing frames in the fake store,
//! and the query service over both.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crucible_auth::test_support::scope;
use crucible_env::{Clock, Instant, StoreFlavour, Stores};
use crucible_plan::{Caller, Value};
use crucible_query::{QueryService, ReadContext};
use crucible_recent::test_support::{FakeWal, ManualClock};
use crucible_recent::{
    ENTRY_BYTES, Frame, RecentConfig, RecentIndex, RecentWriter, SpanEntry, SpanSummary, WalLocator,
};
use crucible_records::{
    HeadKind, Level, Lsn, NO_HEAD_START, NanoUsd, ObservationType, Pick, ProjectId, ProjectScope,
    SpanId, StatusCode, TableId, Timestamp, TraceId, WalEpoch,
};
use crucible_schema::RetentionSettings;

use crate::store::{FakeStore, stored_row};

/// 2026-11-17T09:30:00Z, the wall clock at the start of every test.
pub const NOW: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);
pub const DAY: i64 = 86_400_000_000;
pub const GRACE: Duration = Duration::from_secs(2);
/// The epoch of the node's WAL.
pub const EPOCH: WalEpoch = WalEpoch::from_bytes([0xe0; 16]);

pub fn project(n: u8) -> ProjectId {
    ProjectId::from_bytes([n; 16])
}

pub fn scoped(n: u8) -> ProjectScope {
    scope(project(n)).unwrap()
}

/// A session read of project `n` that keeps its data forever.
pub fn ctx(n: u8) -> ReadContext {
    ReadContext::new(scoped(n), RetentionSettings::default(), Caller::Session)
}

pub fn trace(n: u64) -> TraceId {
    let mut bytes = [0xa5; 16];
    bytes[8..].copy_from_slice(&n.to_be_bytes());
    TraceId::from_bytes(bytes)
}

pub fn span_id(n: u64) -> SpanId {
    SpanId::from_bytes(n.to_be_bytes())
}

/// A span of project 1: `id` in `trace`, starting `offset` microseconds before [`NOW`].
pub fn span(trace_n: u64, id: u64, parent: u64, start: Timestamp) -> SpanEntry {
    let is_root = parent == 0;
    SpanEntry {
        project_id: project(1),
        summary: SpanSummary {
            trace_id: trace(trace_n),
            span_id: span_id(id),
            parent_span_id: span_id(parent),
            is_root,
            start_time: start,
            end_time: start.saturating_add_micros(1_000),
            observation_type: ObservationType::Span,
            level: Level::Default,
            status_code: StatusCode::Ok,
            name: Arc::from(format!("span-{id}")),
            trace_name: Arc::from(""),
            user_id: Arc::from(""),
            session_id: Arc::from(""),
            model: Arc::from(""),
            total_cost_nusd: NanoUsd::from_nanos(7),
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
        },
        head: is_root.then_some(HeadKind::Root),
        locator: WalLocator {
            segment: 1,
            offset: id,
            ordinal: 0,
        },
    }
}

fn bytes(id: impl AsRef<[u8]>) -> Value {
    Value::Bytes(id.as_ref().to_vec())
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

/// The `span_core` row of a span.
pub fn span_core_row(entry: &SpanEntry, lsn: Lsn, received_at: Timestamp) -> Vec<Value> {
    let s = &entry.summary;
    stored_row(
        TableId::SpanCore,
        &[
            ("project_id", Value::Uuid(entry.project_id.uuid())),
            ("start_minute", Value::Time(s.start_time.floor_minute())),
            (
                "trace_hash",
                Value::UInt(u64::from(s.trace_id.trace_hash())),
            ),
            ("start_time", Value::Time(s.start_time)),
            ("end_time", Value::Time(s.end_time)),
            ("span_id", bytes(s.span_id.as_bytes())),
            ("parent_span_id", bytes(s.parent_span_id.as_bytes())),
            ("trace_id", bytes(s.trace_id.as_bytes())),
            ("is_root", Value::UInt(u64::from(s.is_root))),
            ("status_code", Value::Enum(s.status_code.value())),
            ("observation_type", Value::Enum(s.observation_type.value())),
            ("level", Value::Enum(s.level.value())),
            ("name", text(&s.name)),
            ("trace_name", text(&s.trace_name)),
            ("user_id", text(&s.user_id)),
            ("session_id", text(&s.session_id)),
            ("model_response", text(&s.model)),
            ("input_tokens", Value::UInt(s.input_tokens)),
            ("output_tokens", Value::UInt(s.output_tokens)),
            ("total_tokens", Value::UInt(s.total_tokens)),
            ("total_cost_nusd", Value::Int(s.total_cost_nusd.nanos())),
            // A column only the store has: the index keeps no preview.
            ("input_preview", text("preview")),
            ("lsn", Value::UInt(lsn.get())),
            ("received_at", Value::Time(received_at)),
        ],
    )
}

/// The `trace_heads` row of a head span.
pub fn trace_head_row(
    entry: &SpanEntry,
    kind: HeadKind,
    lsn: Lsn,
    received_at: Timestamp,
) -> Vec<Value> {
    let s = &entry.summary;
    stored_row(
        TableId::TraceHeads,
        &[
            ("project_id", Value::Uuid(entry.project_id.uuid())),
            ("start_time", Value::Time(s.start_time)),
            (
                "trace_hash",
                Value::UInt(u64::from(s.trace_id.trace_hash())),
            ),
            ("trace_id", bytes(s.trace_id.as_bytes())),
            ("span_id", bytes(s.span_id.as_bytes())),
            ("head_kind", Value::Enum(kind.value())),
            ("end_time", Value::Time(s.end_time)),
            ("name", text(&s.name)),
            ("level", Value::Enum(s.level.value())),
            ("status_code", Value::Enum(s.status_code.value())),
            ("user_id", text(&s.user_id)),
            ("session_id", text(&s.session_id)),
            ("environment", text("prod")),
            ("wal_epoch", Value::Uuid(EPOCH.uuid())),
            ("lsn", Value::UInt(lsn.get())),
            ("received_at", Value::Time(received_at)),
        ],
    )
}

/// The `trace_stats` partials of one frame: one row per (trace, day), as the shipper writes them.
pub fn trace_stats_rows(spans: &[SpanEntry], lsn: Lsn) -> Vec<Vec<Value>> {
    let mut groups: BTreeMap<(ProjectId, TraceId, i64), Vec<&SpanEntry>> = BTreeMap::new();
    for entry in spans {
        let s = &entry.summary;
        let key = (entry.project_id, s.trace_id, s.start_time.day_number());
        groups.entry(key).or_default().push(entry);
    }
    groups
        .into_iter()
        .map(|((project_id, trace_id, _), members)| {
            let summaries = || members.iter().map(|entry| &entry.summary);
            let pick = |value: fn(&SpanSummary) -> &str| {
                let merged = summaries()
                    .map(|s| Pick::new(s.is_root, s.end_time, s.span_id, value(s)))
                    .max()
                    .unwrap();
                Value::string(merged.as_bytes().to_vec())
            };
            let heads = members.iter().filter(|entry| entry.head.is_some());
            let first = summaries().map(|s| s.start_time).min().unwrap();
            stored_row(
                TableId::TraceStats,
                &[
                    ("project_id", Value::Uuid(project_id.uuid())),
                    ("trace_id", bytes(trace_id.as_bytes())),
                    ("row_day", Value::Date(first.date())),
                    ("marker_batch_seq", Value::UInt(lsn.get())),
                    ("min_start", Value::Time(first)),
                    (
                        "max_end",
                        Value::Time(summaries().map(|s| s.end_time).max().unwrap()),
                    ),
                    (
                        "has_root",
                        Value::UInt(u64::from(summaries().any(|s| s.is_root))),
                    ),
                    (
                        "min_head_start",
                        Value::Time(
                            heads
                                .map(|e| e.summary.start_time)
                                .min()
                                .unwrap_or(NO_HEAD_START),
                        ),
                    ),
                    ("span_count", Value::UInt(members.len() as u64)),
                    (
                        "error_count",
                        Value::UInt(summaries().filter(|s| s.level == Level::Error).count() as u64),
                    ),
                    (
                        "total_cost_nusd",
                        Value::Int(summaries().map(|s| s.total_cost_nusd.nanos()).sum()),
                    ),
                    (
                        "input_tokens",
                        Value::UInt(summaries().map(|s| s.input_tokens).sum()),
                    ),
                    (
                        "output_tokens",
                        Value::UInt(summaries().map(|s| s.output_tokens).sum()),
                    ),
                    (
                        "total_tokens",
                        Value::UInt(summaries().map(|s| s.total_tokens).sum()),
                    ),
                    ("trace_name_pick", pick(|s| &s.trace_name)),
                    ("user_id_pick", pick(|s| &s.user_id)),
                    ("session_id_pick", pick(|s| &s.session_id)),
                ],
            )
        })
        .collect()
}

/// One accepted frame of the WAL.
#[derive(Clone, Debug)]
struct Logged {
    frame: Frame,
    at: Instant,
}

pub struct World {
    pub clock: ManualClock,
    pub wal: FakeWal,
    pub store: Arc<FakeStore>,
    pub index: Arc<RecentIndex>,
    pub writer: RecentWriter,
    pub service: QueryService,
    config: RecentConfig,
    log: Vec<Logged>,
    watermark: Lsn,
    next: u64,
}

impl World {
    /// A node whose recent index holds about `entries` entries.
    pub fn new(entries: u64) -> Self {
        let clock = ManualClock::new(NOW);
        let wal = FakeWal::new();
        let store = Arc::new(FakeStore::default());
        // Room for the entries and their interned strings.
        let config = RecentConfig::new(entries * (ENTRY_BYTES + 256), StoreFlavour::SingleNode);
        let (index, writer) =
            RecentIndex::new(config, Arc::new(clock.clone()), Arc::new(wal.clone()));
        let service = QueryService::new(
            Stores::new(Arc::clone(&store) as _),
            Arc::new(clock.clone()),
            Arc::clone(&index),
            EPOCH,
        );
        Self {
            clock,
            wal,
            store,
            index,
            writer,
            service,
            config,
            log: Vec::new(),
            watermark: Lsn::ZERO,
            next: 1,
        }
    }

    fn append(&mut self, frame: impl FnOnce(Lsn) -> Frame) -> Lsn {
        let lsn = Lsn::new(self.next);
        self.next += 1;
        self.log.push(Logged {
            frame: frame(lsn),
            at: self.clock.now(),
        });
        self.wal.set_durable(lsn);
        lsn
    }

    /// Makes a frame of spans durable (the ack) without stage B applying it.
    pub fn ack_only(&mut self, spans: Vec<SpanEntry>) -> Lsn {
        let received_at = self.clock.wall_now();
        self.append(|lsn| Frame::Spans {
            lsn,
            received_at,
            spans,
        })
    }

    /// Stage B applies every frame it has not applied yet.
    pub fn apply_pending(&mut self) {
        let applied = self.index.applied();
        for logged in &self.log {
            let lsn = frame_lsn(&logged.frame);
            if lsn > applied {
                self.writer.apply(logged.frame.clone());
            }
        }
    }

    /// Acks a frame of spans and lets stage B apply it.
    pub fn ack(&mut self, spans: Vec<SpanEntry>) -> Lsn {
        let lsn = self.ack_only(spans);
        self.apply_pending();
        lsn
    }

    /// The shipper lands every span frame at or below `through` in the store. Landing twice changes nothing.
    pub fn ship(&mut self, through: Lsn) {
        for logged in &self.log {
            let Frame::Spans {
                lsn,
                received_at,
                spans,
            } = &logged.frame
            else {
                continue;
            };
            if *lsn > through {
                continue;
            }
            for entry in spans {
                self.store
                    .insert(TableId::SpanCore, span_core_row(entry, *lsn, *received_at));
                if let Some(kind) = entry.head {
                    let row = trace_head_row(entry, kind, *lsn, *received_at);
                    self.store.insert(TableId::TraceHeads, row);
                }
            }
            for row in trace_stats_rows(spans, *lsn) {
                self.store.insert(TableId::TraceStats, row);
            }
        }
    }

    /// Appends and applies the checkpoint of everything at or below `last_lsn`.
    pub fn checkpoint(&mut self, last_lsn: Lsn) {
        self.watermark = last_lsn;
        self.append(|lsn| Frame::Checkpoint { lsn, last_lsn });
        self.apply_pending();
    }

    /// Ships, checkpoints and lets the visibility grace pass, so the index lets go of what the store serves.
    pub fn settle(&mut self) {
        let through = Lsn::new(self.next - 1);
        self.ship(through);
        self.checkpoint(through);
        self.clock.advance(GRACE);
        self.writer.maintain();
    }

    /// A crash and a recovery: the index is rebuilt from the WAL (contract 4.6 step 7), with every frame above
    /// the watermark plus the frames at or below it that are younger than the visibility grace.
    pub fn restart(&mut self) {
        let now = self.clock.now();
        let watermark = self.watermark;
        let replay: Vec<Frame> = self
            .log
            .iter()
            .filter(|logged| {
                let young = now.saturating_duration_since(logged.at) < GRACE;
                frame_lsn(&logged.frame) > watermark || young
            })
            .map(|logged| logged.frame.clone())
            .collect();
        let (index, writer) = RecentIndex::rebuild(
            self.config,
            Arc::new(self.clock.clone()),
            Arc::new(self.wal.clone()),
            watermark,
            replay,
        );
        self.service = QueryService::new(
            Stores::new(Arc::clone(&self.store) as _),
            Arc::new(self.clock.clone()),
            Arc::clone(&index),
            EPOCH,
        );
        self.index = index;
        self.writer = writer;
    }
}

fn frame_lsn(frame: &Frame) -> Lsn {
    match frame {
        Frame::Spans { lsn, .. }
        | Frame::Checkpoint { lsn, .. }
        | Frame::Quarantine { lsn, .. }
        | Frame::Tombstone { lsn, .. }
        | Frame::Other { lsn } => *lsn,
    }
}

/// Runs a future that never waits on the clock.
pub fn run<T>(future: impl Future<Output = T>) -> T {
    futures::executor::block_on(future)
}
