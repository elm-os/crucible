//! Rollup rows (contract 3.10 and 6.4): partials written by the shipper from the rows of one batch, never by
//! ClickHouse. Each row merges with any row of the same key; `merge` applies the column functions of contract
//! 6.4 (sums saturate). The marker columns of aggregating tables are added by the shipper when it encodes a block.

use crate::caps;
use crate::cost::NanoUsd;
use crate::enums::{CaptureSource, Era, ErrorClass, Level, McpSide, ObservationType};
use crate::histogram::{Histogram, SemconvHistogram};
use crate::id::ProjectId;
use crate::merge::merge_distinct;
use crate::time::{CivilDate, Timestamp};

/// Per-session daily partial (`session_rollup`). Key: `(project_id, day, session_id)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionRollupRow {
    /// Key.
    pub project_id: ProjectId,
    /// Key: UTC day.
    pub day: CivilDate,
    /// Key.
    pub session_id: Box<str>,
    /// `min`.
    pub first_seen: Timestamp,
    /// `max`.
    pub last_seen: Timestamp,
    /// `sum`; incremented on root spans only.
    pub trace_count: u64,
    /// `sum`.
    pub span_count: u64,
    /// `sum`.
    pub error_count: u64,
    /// `sum`.
    pub input_tokens: u64,
    /// `sum`.
    pub output_tokens: u64,
    /// `sum`.
    pub total_tokens: u64,
    /// `sum`.
    pub total_cost_nusd: NanoUsd,
    /// Distinct user ids, at most [`caps::SESSION_USER_IDS_MAX_ITEMS`] (`groupUniqArrayArray(8)`).
    pub user_ids: Vec<Box<str>>,
}

impl SessionRollupRow {
    /// Merges the measures of a row with the same key. `user_ids` keeps the smallest distinct values in byte
    /// order; ClickHouse may keep a different subset once more than the cap are seen.
    pub fn merge(&mut self, other: &Self) {
        self.first_seen = self.first_seen.min(other.first_seen);
        self.last_seen = self.last_seen.max(other.last_seen);
        self.trace_count = self.trace_count.saturating_add(other.trace_count);
        self.span_count = self.span_count.saturating_add(other.span_count);
        self.error_count = self.error_count.saturating_add(other.error_count);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.total_cost_nusd = self.total_cost_nusd.saturating_add(other.total_cost_nusd);
        merge_distinct(
            &mut self.user_ids,
            &other.user_ids,
            caps::SESSION_USER_IDS_MAX_ITEMS,
        );
    }
}

/// Per-user daily partial (`user_rollup`). Key: `(project_id, day, user_id)`. A user's distinct session count is
/// computed at read time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserRollupRow {
    /// Key.
    pub project_id: ProjectId,
    /// Key: UTC day.
    pub day: CivilDate,
    /// Key.
    pub user_id: Box<str>,
    /// `min`.
    pub first_seen: Timestamp,
    /// `max`.
    pub last_seen: Timestamp,
    /// `sum`; incremented on root spans only.
    pub trace_count: u64,
    /// `sum`.
    pub span_count: u64,
    /// `sum`.
    pub error_count: u64,
    /// `sum`.
    pub input_tokens: u64,
    /// `sum`.
    pub output_tokens: u64,
    /// `sum`.
    pub total_tokens: u64,
    /// `sum`.
    pub total_cost_nusd: NanoUsd,
}

impl UserRollupRow {
    /// Merges the measures of a row with the same key.
    pub fn merge(&mut self, other: &Self) {
        self.first_seen = self.first_seen.min(other.first_seen);
        self.last_seen = self.last_seen.max(other.last_seen);
        self.trace_count = self.trace_count.saturating_add(other.trace_count);
        self.span_count = self.span_count.saturating_add(other.span_count);
        self.error_count = self.error_count.saturating_add(other.error_count);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.total_cost_nusd = self.total_cost_nusd.saturating_add(other.total_cost_nusd);
    }
}

/// Per-minute span metrics (`span_metrics`). Key: `(project_id, minute, environment, observation_type,
/// model_response, level, release)`. There is no user dimension.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanMetricsRow {
    /// Key.
    pub project_id: ProjectId,
    /// Key: the start of the minute ([`Timestamp::start_minute`] gives the column value).
    pub minute: Timestamp,
    /// Key.
    pub environment: Box<str>,
    /// Key.
    pub observation_type: ObservationType,
    /// Key.
    pub model_response: Box<str>,
    /// Key.
    pub level: Level,
    /// Key.
    pub release: Box<str>,
    /// `sum`.
    pub count: u64,
    /// `sum`.
    pub error_count: u64,
    /// `sum`.
    pub input_tokens: u64,
    /// `sum`.
    pub output_tokens: u64,
    /// `sum`.
    pub total_tokens: u64,
    /// `sum`.
    pub total_cost_nusd: NanoUsd,
    /// `sumMap`: span durations.
    pub dur_hist: Histogram,
    /// `sumMap`: `completion_start_time - start_time`, generations only.
    pub ttft_hist: Histogram,
}

impl SpanMetricsRow {
    /// Merges the measures of a row with the same key.
    pub fn merge(&mut self, other: &Self) {
        self.count = self.count.saturating_add(other.count);
        self.error_count = self.error_count.saturating_add(other.error_count);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.total_cost_nusd = self.total_cost_nusd.saturating_add(other.total_cost_nusd);
        self.dur_hist.merge(&other.dur_hist);
        self.ttft_hist.merge(&other.ttft_hist);
    }
}

/// Per-minute MCP tool rollup (`mcp_tool_rollup`). Key: `(project_id, side, minute, server_name, deploy_id,
/// capture_source, era, method, target_name, client_name, error_class, caller_fault)`. Rows of different sides or
/// capture sources are never summed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpToolRollupRow {
    /// Key.
    pub project_id: ProjectId,
    /// Key.
    pub side: McpSide,
    /// Key: the start of the minute ([`Timestamp::start_minute`] gives the column value).
    pub minute: Timestamp,
    /// Key.
    pub server_name: Box<str>,
    /// Key.
    pub deploy_id: Box<str>,
    /// Key.
    pub capture_source: CaptureSource,
    /// Key.
    pub era: Era,
    /// Key.
    pub method: Box<str>,
    /// Key.
    pub target_name: Box<str>,
    /// Key.
    pub client_name: Box<str>,
    /// Key.
    pub error_class: ErrorClass,
    /// Key.
    pub caller_fault: bool,
    /// `sum`.
    pub count: u64,
    /// `sum`.
    pub request_bytes: u64,
    /// `sum`.
    pub response_bytes: u64,
    /// `sumMap`: operation durations.
    pub dur_hist: Histogram,
    /// `sumMap`: exact counts per bucket of [`crate::MCP_SEMCONV_DURATION_BOUNDS`].
    pub semconv_hist: SemconvHistogram,
    /// `max`.
    pub last_seen: Timestamp,
}

impl McpToolRollupRow {
    /// Merges the measures of a row with the same key.
    pub fn merge(&mut self, other: &Self) {
        self.count = self.count.saturating_add(other.count);
        self.request_bytes = self.request_bytes.saturating_add(other.request_bytes);
        self.response_bytes = self.response_bytes.saturating_add(other.response_bytes);
        self.dur_hist.merge(&other.dur_hist);
        self.semconv_hist.merge(&other.semconv_hist);
        self.last_seen = self.last_seen.max(other.last_seen);
    }
}

/// Tool inventory partial (`mcp_tool_inventory`), folded from [`crate::McpToolInventoryRecord`]s. Key:
/// `(project_id, server_name, deploy_id, tool_name, schema_hash)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpToolInventoryRow {
    /// Key.
    pub project_id: ProjectId,
    /// Key.
    pub server_name: Box<str>,
    /// Key.
    pub deploy_id: Box<str>,
    /// Key.
    pub tool_name: Box<str>,
    /// Key.
    pub schema_hash: u64,
    /// `min`.
    pub first_seen: Timestamp,
    /// `max`.
    pub last_seen: Timestamp,
    /// Distinct capture sources in value order, at most [`caps::INVENTORY_CAPTURE_SOURCES_MAX_ITEMS`]
    /// (`groupUniqArrayArray(4)`; there are only four).
    pub capture_sources: Vec<CaptureSource>,
}

impl McpToolInventoryRow {
    /// Merges the measures of a row with the same key.
    pub fn merge(&mut self, other: &Self) {
        self.first_seen = self.first_seen.min(other.first_seen);
        self.last_seen = self.last_seen.max(other.last_seen);
        merge_distinct(
            &mut self.capture_sources,
            &other.capture_sources,
            caps::INVENTORY_CAPTURE_SOURCES_MAX_ITEMS,
        );
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn metrics(count: u64, durations: &[u64]) -> SpanMetricsRow {
        let mut dur_hist = Histogram::new();
        durations.iter().for_each(|&d| dur_hist.record(d));
        SpanMetricsRow {
            project_id: ProjectId::from_bytes([1; 16]),
            minute: Timestamp::from_micros(1_794_907_800_000_000),
            environment: "default".into(),
            observation_type: ObservationType::Generation,
            model_response: "model-a".into(),
            level: Level::Default,
            release: "".into(),
            count,
            error_count: count * 2,
            input_tokens: count * 3,
            output_tokens: count * 5,
            total_tokens: count * 8,
            total_cost_nusd: NanoUsd::from_nanos(i64::try_from(count).unwrap_or(0) * 7),
            dur_hist,
            ttft_hist: Histogram::new(),
        }
    }

    fn tool(count: u64, last_seen: i64, durations: &[u64]) -> McpToolRollupRow {
        let mut dur_hist = Histogram::new();
        let mut semconv_hist = SemconvHistogram::new();
        for &d in durations {
            dur_hist.record(d);
            semconv_hist.record(d);
        }
        McpToolRollupRow {
            project_id: ProjectId::from_bytes([1; 16]),
            side: McpSide::Server,
            minute: Timestamp::from_micros(1_794_907_800_000_000),
            server_name: "files".into(),
            deploy_id: "d1".into(),
            capture_source: CaptureSource::Sdk,
            era: Era::Modern,
            method: "tools/call".into(),
            target_name: "read_file".into(),
            client_name: "".into(),
            error_class: ErrorClass::None,
            caller_fault: false,
            count,
            request_bytes: count * 100,
            response_bytes: count * 200,
            dur_hist,
            semconv_hist,
            last_seen: Timestamp::from_micros(last_seen),
        }
    }

    proptest! {
        #[test]
        fn metric_rows_merge_like_their_union(
            a in proptest::collection::vec(0u64..10_000_000, 0..20),
            b in proptest::collection::vec(0u64..10_000_000, 0..20),
        ) {
            let (ra, rb) = (metrics(a.len() as u64, &a), metrics(b.len() as u64, &b));
            let all: Vec<u64> = a.iter().chain(&b).copied().collect();
            let mut ab = ra.clone();
            ab.merge(&rb);
            let mut ba = rb.clone();
            ba.merge(&ra);
            prop_assert_eq!(&ab, &ba);
            prop_assert_eq!(&ab, &metrics(all.len() as u64, &all));

            let (ta, tb) = (tool(a.len() as u64, 5, &a), tool(b.len() as u64, 9, &b));
            let mut tab = ta.clone();
            tab.merge(&tb);
            let expected = tool(all.len() as u64, 9, &all);
            prop_assert_eq!(tab, expected);
        }
    }

    #[test]
    fn session_user_ids_are_capped_and_sorted() {
        let row = |users: &[&str], first: i64, last: i64| SessionRollupRow {
            project_id: ProjectId::from_bytes([1; 16]),
            day: CivilDate::new(2026, 11, 17).unwrap(),
            session_id: "s1".into(),
            first_seen: Timestamp::from_micros(first),
            last_seen: Timestamp::from_micros(last),
            trace_count: 1,
            span_count: 2,
            error_count: 0,
            input_tokens: 1,
            output_tokens: 1,
            total_tokens: 2,
            total_cost_nusd: NanoUsd::from_nanos(3),
            user_ids: users.iter().map(|&u| u.into()).collect(),
        };
        let mut a = row(&["u5", "u1", "u3", "u7", "u9"], 50, 60);
        a.user_ids.sort_unstable();
        let b = row(&["u2", "u4", "u6", "u8", "u1"], 40, 55);
        a.merge(&b);
        let users: Vec<&str> = a.user_ids.iter().map(|u| &**u).collect();
        assert_eq!(users, ["u1", "u2", "u3", "u4", "u5", "u6", "u7", "u8"]);
        assert_eq!((a.first_seen.micros(), a.last_seen.micros()), (40, 60));
        assert_eq!(
            (a.trace_count, a.span_count, a.total_cost_nusd.nanos()),
            (2, 4, 6)
        );
    }

    #[test]
    fn user_and_inventory_rows_merge() {
        let user = |first: i64, tokens: u64| UserRollupRow {
            project_id: ProjectId::from_bytes([1; 16]),
            day: CivilDate::new(2026, 11, 17).unwrap(),
            user_id: "u1".into(),
            first_seen: Timestamp::from_micros(first),
            last_seen: Timestamp::from_micros(first + 10),
            trace_count: 1,
            span_count: 1,
            error_count: 1,
            input_tokens: tokens,
            output_tokens: tokens,
            total_tokens: 2 * tokens,
            total_cost_nusd: NanoUsd::from_nanos(1),
        };
        let mut u = user(5, 3);
        u.merge(&user(2, 4));
        assert_eq!(
            (u.first_seen.micros(), u.last_seen.micros(), u.total_tokens),
            (2, 15, 14)
        );

        let inventory = |sources: Vec<CaptureSource>, seen: i64| McpToolInventoryRow {
            project_id: ProjectId::from_bytes([1; 16]),
            server_name: "files".into(),
            deploy_id: "d1".into(),
            tool_name: "read_file".into(),
            schema_hash: 42,
            first_seen: Timestamp::from_micros(seen),
            last_seen: Timestamp::from_micros(seen),
            capture_sources: sources,
        };
        let mut i = inventory(vec![CaptureSource::Otlp], 7);
        i.merge(&inventory(vec![CaptureSource::Sdk, CaptureSource::Otlp], 3));
        assert_eq!(i.capture_sources, [CaptureSource::Sdk, CaptureSource::Otlp]);
        assert_eq!((i.first_seen.micros(), i.last_seen.micros()), (3, 7));
    }
}
