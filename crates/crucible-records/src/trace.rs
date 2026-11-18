//! Trace heads, trace stats and rank-prefixed picks (contract 3.3 and 3.9).

use crate::caps;
use crate::cost::NanoUsd;
use crate::enums::{HeadKind, Level, StatusCode};
use crate::id::{ProjectId, SpanId, TraceId};
use crate::ingestion::Ingestion;
use crate::merge::merge_distinct;
use crate::time::{CivilDate, Timestamp};

/// The `min_head_start` of a `trace_stats` row with no head: the largest `DateTime64(6, 'UTC')` value,
/// 2299-12-31T23:59:59.999999Z, so that it loses every `min`.
pub const NO_HEAD_START: Timestamp = Timestamp::from_micros(10_413_791_999_999_999);

/// A rank-prefixed pick (contract 3.3): 17 rank bytes (`is_root`, then `end_time` as 8 bytes big-endian, then the
/// 8 `span_id` bytes) followed by the value.
///
/// Byte-wise `max` over the picks of a trace, which is how `trace_stats` merges them, selects the root span's
/// value, else the value with the greatest `(end_time, span_id)`. `Ord` is that byte-wise order. An empty pick
/// means "no value" and loses every `max`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Pick(Box<[u8]>);

impl Pick {
    /// Length of the rank prefix.
    pub const RANK_BYTES: usize = 17;

    /// The pick of one span's value; empty when the value is empty. A negative `end_time` (outside the valid
    /// range) ranks as zero.
    pub fn new(is_root: bool, end_time: Timestamp, span_id: SpanId, value: &str) -> Self {
        if value.is_empty() {
            return Self::default();
        }
        let end = u64::try_from(end_time.micros()).unwrap_or(0);
        let mut bytes = Vec::with_capacity(Self::RANK_BYTES + value.len());
        bytes.push(u8::from(is_root));
        bytes.extend_from_slice(&end.to_be_bytes());
        bytes.extend_from_slice(span_id.as_bytes());
        bytes.extend_from_slice(value.as_bytes());
        Self(bytes.into_boxed_slice())
    }

    /// A pick read back from the store.
    pub fn from_stored(bytes: Box<[u8]>) -> Self {
        Self(bytes)
    }

    /// The stored form: rank bytes and value.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Whether the pick holds no value.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value: the stored form without its rank prefix (empty for an empty or malformed pick).
    pub fn value(&self) -> &[u8] {
        self.0.get(Self::RANK_BYTES..).unwrap_or_default()
    }

    /// Merges another pick into this one (`max`).
    pub fn merge(&mut self, other: &Self) {
        if *other > *self {
            self.clone_from(other);
        }
    }
}

/// A trace head (`trace_heads`): the root-anchored driver of the trace list. Every value is the head span's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceHead {
    /// From the credential only.
    pub project_id: ProjectId,
    /// The trace.
    pub trace_id: TraceId,
    /// `root` for every span with `is_root`; `provisional` the first time stage B sees a trace without a root.
    pub head_kind: HeadKind,
    /// The head span.
    pub span_id: SpanId,
    /// Immutable; the list sort key.
    pub start_time: Timestamp,
    /// End of the head span.
    pub end_time: Timestamp,
    /// The head span's name.
    pub name: Box<str>,
    /// The head span's level.
    pub level: Level,
    /// The head span's status.
    pub status_code: StatusCode,
    /// After the request-local forward-fill (contract 3.3).
    pub user_id: Box<str>,
    /// After the request-local forward-fill.
    pub session_id: Box<str>,
    /// The head span's `trace_tags` after the request-local forward-fill.
    pub tags: Vec<Box<str>>,
    /// After the request-local forward-fill.
    pub environment: Box<str>,
    /// After the request-local forward-fill.
    pub release: Box<str>,
    /// After the request-local forward-fill.
    pub version: Box<str>,
    /// The head span's input preview.
    pub input_preview: Box<str>,
    /// The head span's output preview.
    pub output_preview: Box<str>,
    /// Where and when the WAL accepted the head span.
    pub ingestion: Ingestion,
}

impl TraceHead {
    /// `trace_hash` (contract 6.1).
    pub fn trace_hash(&self) -> u32 {
        self.trace_id.trace_hash()
    }
}

/// A mergeable partial of one trace (`trace_stats`); any number exist per trace and readers aggregate them.
/// The key is `(project_id, trace_id, row_day)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceStatsRow {
    /// Key.
    pub project_id: ProjectId,
    /// Key.
    pub trace_id: TraceId,
    /// Key: the UTC day of the contributing spans' `start_time`, also the partition anchor.
    pub row_day: CivilDate,
    /// `min`: displayed trace start.
    pub min_start: Timestamp,
    /// `max`: displayed trace end.
    pub max_end: Timestamp,
    /// `max`: set once a root span is seen.
    pub has_root: bool,
    /// `min`: earliest head `start_time`; [`NO_HEAD_START`] on rows with no head.
    pub min_head_start: Timestamp,
    /// `max`.
    pub max_level: Level,
    /// `sum`.
    pub span_count: u64,
    /// `sum`.
    pub error_count: u64,
    /// `sum`.
    pub generation_count: u64,
    /// `sum`.
    pub tool_count: u64,
    /// `sum`.
    pub mcp_count: u64,
    /// `sum`.
    pub total_cost_nusd: NanoUsd,
    /// `sum`.
    pub input_tokens: u64,
    /// `sum`.
    pub output_tokens: u64,
    /// `sum`.
    pub total_tokens: u64,
    /// Distinct environments, at most [`caps::TRACE_ENVIRONMENTS_MAX_ITEMS`] (`groupUniqArrayArray(16)`).
    pub environments: Vec<Box<str>>,
    /// `max` of rank-prefixed picks.
    pub trace_name_pick: Pick,
    /// `max` of rank-prefixed picks.
    pub user_id_pick: Pick,
    /// `max` of rank-prefixed picks.
    pub session_id_pick: Pick,
}

impl TraceStatsRow {
    /// Merges the measures of a row with the same key. Sums saturate. `environments` keeps the smallest distinct
    /// values in byte order; ClickHouse may keep a different subset once more than the cap are seen.
    pub fn merge(&mut self, other: &Self) {
        self.min_start = self.min_start.min(other.min_start);
        self.max_end = self.max_end.max(other.max_end);
        self.has_root |= other.has_root;
        self.min_head_start = self.min_head_start.min(other.min_head_start);
        self.max_level = self.max_level.max(other.max_level);
        self.span_count = self.span_count.saturating_add(other.span_count);
        self.error_count = self.error_count.saturating_add(other.error_count);
        self.generation_count = self.generation_count.saturating_add(other.generation_count);
        self.tool_count = self.tool_count.saturating_add(other.tool_count);
        self.mcp_count = self.mcp_count.saturating_add(other.mcp_count);
        self.total_cost_nusd = self.total_cost_nusd.saturating_add(other.total_cost_nusd);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        merge_distinct(
            &mut self.environments,
            &other.environments,
            caps::TRACE_ENVIRONMENTS_MAX_ITEMS,
        );
        self.trace_name_pick.merge(&other.trace_name_pick);
        self.user_id_pick.merge(&other.user_id_pick);
        self.session_id_pick.merge(&other.session_id_pick);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn pick_layout() {
        let span = SpanId::from_bytes([9, 8, 7, 6, 5, 4, 3, 2]);
        let pick = Pick::new(true, Timestamp::from_micros(0x0102), span, "checkout");
        let bytes = pick.as_bytes();
        assert_eq!(bytes[0], 1);
        assert_eq!(&bytes[1..9], &[0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(&bytes[9..17], span.as_bytes());
        assert_eq!(pick.value(), b"checkout");
        assert_eq!(Pick::from_stored(bytes.into()), pick);
        assert!(Pick::new(true, Timestamp::EPOCH, span, "").is_empty());
        assert_eq!(Pick::from_stored(Box::new([1, 2])).value(), b"");
    }

    #[test]
    fn no_head_start_is_the_last_datetime64_instant() {
        let end = CivilDate::new(2300, 1, 1).unwrap().start();
        assert_eq!(NO_HEAD_START.micros(), end.micros() - 1);
        assert_eq!(NO_HEAD_START.to_string(), "2299-12-31T23:59:59.999999Z");
    }

    /// One span's contribution to a pick: root flag, end time, span id and value.
    fn arb_span() -> impl Strategy<Value = (bool, i64, [u8; 8], String)> {
        (
            any::<bool>(),
            0i64..Timestamp::MAX_EXCLUSIVE.micros(),
            any::<[u8; 8]>(),
            "[a-z]{0,3}",
        )
    }

    fn arb_stats() -> impl Strategy<Value = TraceStatsRow> {
        (
            (
                0i64..1_000,
                0i64..1_000,
                any::<bool>(),
                0i64..1_000,
                0usize..4,
            ),
            proptest::collection::vec(0u64..1_000, 9),
            proptest::collection::vec("[a-c]", 0..4),
            proptest::collection::vec(arb_span(), 3),
        )
            .prop_map(|((start, end, root, head, level), sums, envs, picks)| {
                let pick = |(r, e, s, v): &(bool, i64, [u8; 8], String)| {
                    Pick::new(*r, Timestamp::from_micros(*e), SpanId::from_bytes(*s), v)
                };
                let mut environments: Vec<Box<str>> = envs.into_iter().map(Into::into).collect();
                environments.sort_unstable();
                environments.dedup();
                TraceStatsRow {
                    project_id: ProjectId::from_bytes([1; 16]),
                    trace_id: TraceId::from_bytes([2; 16]),
                    row_day: CivilDate::new(2026, 11, 17).unwrap(),
                    min_start: Timestamp::from_micros(start),
                    max_end: Timestamp::from_micros(end),
                    has_root: root,
                    min_head_start: if head % 2 == 0 {
                        NO_HEAD_START
                    } else {
                        Timestamp::from_micros(head)
                    },
                    max_level: Level::ALL[level],
                    span_count: sums[0],
                    error_count: sums[1],
                    generation_count: sums[2],
                    tool_count: sums[3],
                    mcp_count: sums[4],
                    total_cost_nusd: NanoUsd::from_nanos(sums[5] as i64),
                    input_tokens: sums[6],
                    output_tokens: sums[7],
                    total_tokens: sums[8],
                    environments,
                    trace_name_pick: pick(&picks[0]),
                    user_id_pick: pick(&picks[1]),
                    session_id_pick: pick(&picks[2]),
                }
            })
    }

    fn merged(a: &TraceStatsRow, b: &TraceStatsRow) -> TraceStatsRow {
        let mut out = a.clone();
        out.merge(b);
        out
    }

    proptest! {
        /// Contract 3.3: the byte-wise max of the picks is the root span's value, else the value with the greatest
        /// `(end_time, span_id)`.
        #[test]
        fn max_pick_follows_the_selection_rule(spans in proptest::collection::vec(arb_span(), 0..8)) {
            let best = spans
                .iter()
                .fold(Pick::default(), |mut acc, (root, end, span, value)| {
                    acc.merge(&Pick::new(*root, Timestamp::from_micros(*end), SpanId::from_bytes(*span), value));
                    acc
                });
            let expected = spans
                .iter()
                .filter(|(_, _, _, value)| !value.is_empty())
                .max_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)))
                .map(|(_, _, _, value)| value.as_bytes());
            prop_assert_eq!(best.value(), expected.unwrap_or_default());
            prop_assert_eq!(best.is_empty(), expected.is_none());
        }

        #[test]
        fn stats_merge_is_commutative_and_associative(
            a in arb_stats(),
            b in arb_stats(),
            c in arb_stats(),
        ) {
            prop_assert_eq!(merged(&a, &b), merged(&b, &a));
            prop_assert_eq!(merged(&merged(&a, &b), &c), merged(&a, &merged(&b, &c)));
            let ab = merged(&a, &b);
            prop_assert_eq!(ab.span_count, a.span_count + b.span_count);
            prop_assert_eq!(ab.min_start, a.min_start.min(b.min_start));
            prop_assert_eq!(ab.has_root, a.has_root || b.has_root);
        }
    }
}
