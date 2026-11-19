//! The service, and what every read of it shares: the retention clamp, the store calls and the per-trace
//! aggregates.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crucible_env::metrics::registry;
use crucible_env::{Clock, Histogram, Metrics, QueryError, Stores};
use crucible_plan::{MAX_LIST, PlanError, QueryPlan, RetentionClamp, Row, trace_page_stats};
use crucible_recent::{Consistency, ReadState, RecentIndex, RecentSpan};
use crucible_records::{
    Level, NO_HEAD_START, NanoUsd, Pick, TableId, TimeWindow, Timestamp, TraceId, WalEpoch,
};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::decode::Fields;

/// Every instant the contract allows: the window of a lookup by trace id, which the retention clamp then cuts.
pub(crate) fn all_time() -> Result<TimeWindow, QueryServiceError> {
    TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE)
        .map_err(|_| QueryServiceError::Internal("the valid time range is empty".to_owned()))
}

/// The read services over the telemetry store and the recent-span index (contract 8.2, 8.7 and 8.8).
///
/// Every method takes a [`ReadContext`], and so a `ProjectScope` and the project's retention settings. The
/// post-operators run here, identically over ClickHouse and over the simulation's store: the merge with the
/// recent index, span de-duplication by `span_id`, pick resolution and provisional-head filtering.
#[derive(Clone, Debug)]
pub struct QueryService {
    pub(crate) stores: Stores,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) index: Arc<RecentIndex>,
    /// The epoch of the WAL that feeds `index`: the LSNs of this node's head cursors count its frames.
    pub(crate) wal_epoch: WalEpoch,
    /// The per-request counters, when the service records them.
    meters: Option<Arc<Meters>>,
}

/// The request classes of the read services, which label the per-request counters: one per method.
pub const READ_CLASSES: [&str; 11] = [
    "trace_by_id",
    "trace_list",
    "trace_list_since",
    "span_children",
    "payload",
    "facts",
    "fact_target_bounds",
    "filter_values",
    "data_bounds",
    "search",
    "metrics",
];

/// The handles of the per-request counters, fetched once per class.
#[derive(Debug)]
struct Meters {
    /// Per class: the store reads of one request, and its wait for the read barrier.
    classes: BTreeMap<&'static str, (Histogram, Histogram)>,
}

impl QueryService {
    /// A service over these stores and this node's recent index, which the WAL of epoch `wal_epoch` feeds.
    pub fn new(
        stores: Stores,
        clock: Arc<dyn Clock>,
        index: Arc<RecentIndex>,
        wal_epoch: WalEpoch,
    ) -> Self {
        Self {
            stores,
            clock,
            index,
            wal_epoch,
            meters: None,
        }
    }

    /// The service, recording for every request, by class ([`READ_CLASSES`]), the store reads it issued
    /// (`crucible_api_store_reads`) and, for the reads that wait for the recent index's read barrier, how long
    /// they waited (`crucible_api_barrier_wait_microseconds`), timed with the service's clock.
    #[must_use]
    pub fn with_metrics(mut self, metrics: &dyn Metrics) -> Self {
        let classes = READ_CLASSES
            .iter()
            .map(|class| {
                (
                    *class,
                    (
                        metrics.histogram(&registry::API_STORE_READS, &[class]),
                        metrics.histogram(&registry::API_BARRIER_WAIT, &[class]),
                    ),
                )
            })
            .collect();
        self.meters = Some(Arc::new(Meters { classes }));
        self
    }

    /// A read of request class `class`, one of [`READ_CLASSES`].
    pub(crate) fn read(&self, ctx: &ReadContext, class: &'static str) -> Read<'_> {
        let now = self.clock.wall_now();
        Read {
            service: self,
            ctx: *ctx,
            clamp: RetentionClamp::new(ctx.retention, now),
            now,
            store_down: None,
            class,
            store_reads: 0,
            barrier_wait: None,
        }
    }

    /// `Strong` when stage B has applied every durable frame. Taken before the index is read, so that a strong
    /// answer covers every span acknowledged before the request.
    pub(crate) fn consistency(&self) -> Consistency {
        if self.index.caught_up() {
            Consistency::Strong
        } else {
            Consistency::Lagging
        }
    }
}

/// One request: the context, the clamp at the request's instant, and whether the store failed to answer.
pub(crate) struct Read<'a> {
    pub(crate) service: &'a QueryService,
    pub(crate) ctx: ReadContext,
    pub(crate) clamp: RetentionClamp,
    pub(crate) now: Timestamp,
    /// Set when a store read failed as unavailable; the read goes on with what the recent index holds.
    store_down: Option<String>,
    /// The request's class, which labels its counters.
    class: &'static str,
    /// Store reads issued so far.
    store_reads: u64,
    /// How long the request waited for the read barrier, when it waited.
    barrier_wait: Option<Duration>,
}

impl Drop for Read<'_> {
    /// Records the request's counters, however it ended.
    fn drop(&mut self) {
        let Some(meters) = &self.service.meters else {
            return;
        };
        if let Some((reads, barrier)) = meters.classes.get(self.class) {
            reads.record(self.store_reads);
            if let Some(waited) = self.barrier_wait {
                barrier.record(u64::try_from(waited.as_micros()).unwrap_or(u64::MAX));
            }
        }
    }
}

impl Read<'_> {
    /// Notes how long the request waited for the read barrier.
    pub(crate) fn barrier_waited(&mut self, waited: Duration) {
        self.barrier_wait = Some(self.barrier_wait.unwrap_or_default() + waited);
    }
    /// The rows of a plan. No plan (the window has expired) is no rows. An unavailable store is no rows too,
    /// and is remembered: the response is then partial, or an error when the index has nothing to offer.
    pub(crate) async fn rows(
        &mut self,
        plan: Result<Option<QueryPlan>, PlanError>,
    ) -> Result<Vec<Row>, QueryServiceError> {
        let Some(plan) = plan? else {
            return Ok(Vec::new());
        };
        if self.store_down.is_some() {
            return Ok(Vec::new());
        }
        self.store_reads += 1;
        let result = match self.service.stores.telemetry.execute(plan).await {
            Ok(stream) => stream.rows().await,
            Err(error) => Err(error),
        };
        match result {
            Ok(rows) => Ok(rows),
            Err(QueryError::Unavailable(reason)) => {
                self.store_down = Some(reason);
                Ok(Vec::new())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The first instant of `table` that has not expired.
    pub(crate) fn cutoff(&self, table: TableId) -> Result<Option<Timestamp>, QueryServiceError> {
        Ok(self.clamp.cutoff(table)?)
    }

    /// Drops the recent entries that retention has expired: the index is filtered with the cut-off the plans
    /// are clamped to.
    pub(crate) fn retained(
        &self,
        mut spans: Vec<RecentSpan>,
    ) -> Result<Vec<RecentSpan>, QueryServiceError> {
        if let Some(cutoff) = self.cutoff(TableId::SpanCore)? {
            spans.retain(|span| span.summary.start_time >= cutoff);
        }
        Ok(spans)
    }

    /// Ends the read. `from_index` says whether the recent index contributed to the answer: with the store
    /// unavailable and nothing from the index, the read fails with `telemetry_store_unavailable`.
    pub(crate) fn finish(
        &self,
        as_of: Timestamp,
        consistency: Consistency,
        state: ReadState,
        tables: &[TableId],
        from_index: bool,
    ) -> Result<Meta, QueryServiceError> {
        match &self.store_down {
            Some(reason) if !from_index => Err(QueryError::Unavailable(reason.clone()).into()),
            down => Ok(Meta::new(as_of, consistency, state, tables, down.is_some())),
        }
    }

    /// The merged aggregates of several traces: the store's `trace_stats` plus the recent entries.
    pub(crate) async fn stats(
        &mut self,
        trace_ids: &[TraceId],
    ) -> Result<BTreeMap<TraceId, Stats>, QueryServiceError> {
        let (scope, caller) = (self.ctx.scope, self.ctx.caller);
        let (recent, state) = self.service.index.traces(scope, trace_ids);
        let mut stats: BTreeMap<TraceId, Stats> = BTreeMap::new();
        for span in self.retained(recent)? {
            stats
                .entry(span.summary.trace_id)
                .or_default()
                .add_recent(&span, state);
        }
        for ids in trace_ids.chunks(MAX_LIST) {
            let plan = trace_page_stats(scope, all_time()?, &self.clamp, caller, ids);
            for row in self.rows(plan).await? {
                let (trace_id, stored) = Stats::decode(&row)?;
                stats.entry(trace_id).or_default().merge(&stored);
            }
        }
        Ok(stats)
    }
}

/// The aggregates of one trace while they are merged: picks are still rank-prefixed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Stats {
    pub(crate) min_start: Timestamp,
    pub(crate) max_end: Timestamp,
    pub(crate) has_root: bool,
    pub(crate) min_head_start: Timestamp,
    pub(crate) span_count: u64,
    pub(crate) error_count: u64,
    pub(crate) total_cost_nusd: NanoUsd,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: u64,
    trace_name: Pick,
    user_id: Pick,
    session_id: Pick,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            min_start: NO_HEAD_START,
            max_end: Timestamp::EPOCH,
            has_root: false,
            min_head_start: NO_HEAD_START,
            span_count: 0,
            error_count: 0,
            total_cost_nusd: NanoUsd::ZERO,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            trace_name: Pick::default(),
            user_id: Pick::default(),
            session_id: Pick::default(),
        }
    }
}

impl Stats {
    /// The aggregates of a trace of which only a head is known.
    pub(crate) fn of_head(start_time: Timestamp, end_time: Timestamp) -> Self {
        Self {
            min_start: start_time,
            max_end: end_time,
            min_head_start: start_time,
            ..Self::default()
        }
    }

    /// A row of `trace_page_stats`.
    fn decode(row: &Row) -> Result<(TraceId, Self), QueryServiceError> {
        let f = Fields(row.values());
        let pick = |index| f.bytes(index).map(|bytes| Pick::from_stored(bytes.into()));
        let stats = Self {
            min_start: f.time(1)?,
            max_end: f.time(2)?,
            has_root: f.uint(3)? != 0,
            min_head_start: f.time(4)?,
            span_count: f.uint(5)?,
            error_count: f.uint(6)?,
            total_cost_nusd: NanoUsd::from_nanos(f.int(7)?),
            input_tokens: f.uint(8)?,
            output_tokens: f.uint(9)?,
            total_tokens: f.uint(10)?,
            trace_name: pick(11)?,
            user_id: pick(12)?,
            session_id: pick(13)?,
        };
        Ok((f.trace_id(0)?, stats))
    }

    /// Merges the store's partials (contract 3.9).
    fn merge(&mut self, other: &Self) {
        self.min_start = self.min_start.min(other.min_start);
        self.max_end = self.max_end.max(other.max_end);
        self.has_root |= other.has_root;
        self.min_head_start = self.min_head_start.min(other.min_head_start);
        self.span_count = self.span_count.saturating_add(other.span_count);
        self.error_count = self.error_count.saturating_add(other.error_count);
        self.total_cost_nusd = self.total_cost_nusd.saturating_add(other.total_cost_nusd);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.trace_name.merge(&other.trace_name);
        self.user_id.merge(&other.user_id);
        self.session_id.merge(&other.session_id);
    }

    /// Adds one recent entry. The bounds, the root flag and the picks merge idempotently, so every entry
    /// counts. The sums count only entries above the checkpoint watermark: an entry at or below it is confirmed
    /// in the store, whose sums already hold it. Token and cost sums leave out a usage aggregate, as the store's
    /// do (the sums rule of contract 3.1); the counts take every span.
    fn add_recent(&mut self, span: &RecentSpan, state: ReadState) {
        let s = &span.summary;
        self.min_start = self.min_start.min(s.start_time);
        self.max_end = self.max_end.max(s.end_time);
        self.has_root |= s.is_root;
        if span.head.is_some() {
            self.min_head_start = self.min_head_start.min(s.start_time);
        }
        let pick = |value: &str| Pick::new(s.is_root, s.end_time, s.span_id, value);
        self.trace_name.merge(&pick(&s.trace_name));
        self.user_id.merge(&pick(&s.user_id));
        self.session_id.merge(&pick(&s.session_id));
        if span.lsn > state.watermark {
            self.span_count = self.span_count.saturating_add(1);
            self.error_count = self
                .error_count
                .saturating_add(u64::from(s.level == Level::Error));
            if !s.usage_aggregate {
                self.total_cost_nusd = self.total_cost_nusd.saturating_add(s.total_cost_nusd);
                self.input_tokens = self.input_tokens.saturating_add(s.input_tokens);
                self.output_tokens = self.output_tokens.saturating_add(s.output_tokens);
                self.total_tokens = self.total_tokens.saturating_add(s.total_tokens);
            }
        }
    }

    /// Resolves the picks (contract 3.3).
    pub(crate) fn resolve(&self) -> TraceSummary {
        let text = |pick: &Pick| Arc::from(String::from_utf8_lossy(pick.value()));
        TraceSummary {
            min_start: self.min_start,
            max_end: self.max_end,
            has_root: self.has_root,
            span_count: self.span_count,
            error_count: self.error_count,
            total_cost_nusd: self.total_cost_nusd,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            trace_name: text(&self.trace_name),
            user_id: text(&self.user_id),
            session_id: text(&self.session_id),
        }
    }
}

/// The trace-level aggregates of one trace, merged from `trace_stats` and the recent index, with its picks
/// resolved.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceSummary {
    /// The displayed trace start.
    pub min_start: Timestamp,
    /// The displayed trace end.
    pub max_end: Timestamp,
    /// Whether a root span has been seen.
    pub has_root: bool,
    /// Spans. The store's sum plus the recent entries above the checkpoint watermark, so this and the other
    /// sums run ahead by the spans that are in the store while the watermark has not passed them yet (the
    /// watermark is a low watermark: one unresolved earlier batch holds it back).
    pub span_count: u64,
    /// Spans of level `error`.
    pub error_count: u64,
    /// The total cost.
    pub total_cost_nusd: NanoUsd,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
    /// The trace name: the root span's, else the latest non-empty one.
    pub trace_name: Arc<str>,
    /// The user, picked the same way.
    pub user_id: Arc<str>,
    /// The session, picked the same way.
    pub session_id: Arc<str>,
}
