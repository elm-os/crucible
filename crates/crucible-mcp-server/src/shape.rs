//! Result shaping: the wire forms the tools share (contract 1.3: times as RFC 3339 strings, ids as hex, costs
//! in USD, counts saturating at 2^53 - 1), previews, and the hard cap on the size of a tool result.
//!
//! The rows follow the field names of the REST DTOs and add previews. They derive their output schemas here,
//! so the DTO crate keeps its two derives.

use crucible_api_types::{Saturation, usd_from_nanos};
use crucible_plan::Value;
use crucible_query as query;
use crucible_query::{SpanSource, TraceListItem, TraceSpan};
use crucible_records::{SpanId, Timestamp};
use crucible_schema::{Column, SpanCoreCol};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::Serialize;

/// The most bytes a tool result may take: the serialized `CallToolResult`, whose JSON is carried twice, as
/// `structuredContent` and as its text.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;

/// The bytes of the tool result that carries `output`.
pub(crate) fn result_bytes(output: &impl Serialize) -> usize {
    serde_json::to_value(output)
        .ok()
        .and_then(|value| serde_json::to_vec(&CallToolResult::structured(value)).ok())
        .map_or(usize::MAX, |bytes| bytes.len())
}

/// The output of a list cut at row boundaries to stay under [`MAX_RESULT_BYTES`]: the output of every row
/// when it fits, else of the longest prefix that does. A prefix always holds one row when there is one; a row
/// too large on its own is `lighten`ed first. `render(rows, cut)` builds the output of a prefix; `cut` says
/// that rows were left out, so the output must say so and continue after the prefix's last row.
pub(crate) fn fit<T: Clone, O: Serialize>(
    mut rows: Vec<T>,
    lighten: impl Fn(&mut T),
    render: impl Fn(&[T], bool) -> O,
) -> O {
    let fits = |output: &O| result_bytes(output) <= MAX_RESULT_BYTES;
    let whole = render(&rows, false);
    if rows.len() <= 1 || fits(&whole) {
        if rows.len() == 1 && !fits(&whole) {
            lighten(&mut rows[0]);
            return render(&rows, false);
        }
        return whole;
    }
    // A cut prefix carries a cursor and the cut markers whatever its length, so its size grows with each row:
    // find the longest prefix that fits by bisection.
    let (mut fitting, mut too_long) = (0, rows.len());
    while too_long - fitting > 1 {
        let middle = fitting + (too_long - fitting) / 2;
        if fits(&render(&rows[..middle], true)) {
            fitting = middle;
        } else {
            too_long = middle;
        }
    }
    if fitting == 0 {
        lighten(&mut rows[0]);
        fitting = 1;
    }
    render(&rows[..fitting], true)
}

pub(crate) fn micros_between(start: Timestamp, end: Timestamp) -> u64 {
    u64::try_from(end.micros().saturating_sub(start.micros())).unwrap_or(0)
}

/// How complete and how fresh a response is (contract 8.6).
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct Meta {
    /// The instant the response describes, RFC 3339.
    as_of: String,
    /// Whether rows may be missing, for the reason in `partial_reason`.
    partial: bool,
    /// `store_unavailable`, `deadline` or `quarantined`, when `partial`.
    #[serde(skip_serializing_if = "Option::is_none")]
    partial_reason: Option<&'static str>,
    /// `strong` when the response reflects every span the server acknowledged before the call; `lagging` when
    /// it may trail.
    consistency: &'static str,
}

pub(crate) fn meta(meta: &query::Meta) -> Meta {
    Meta {
        as_of: meta.as_of.to_string(),
        partial: meta.partial,
        partial_reason: meta.partial_reason.map(|reason| reason.as_str()),
        consistency: match meta.consistency {
            query::Consistency::Strong => "strong",
            query::Consistency::Lagging => "lagging",
        },
    }
}

/// The value of a text column of a `span_core` row, by its position in `SpanCoreCol::ALL`.
fn stored_text(row: &[Value], column: SpanCoreCol) -> Option<String> {
    let at = SpanCoreCol::ALL.iter().position(|c| *c == column)?;
    match row.get(at)? {
        Value::String(text) => Some(text.clone()),
        Value::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// The input and output previews of a span: from its `span_core` row, or none while only the recent index
/// holds it.
fn span_previews(span: &TraceSpan) -> (Option<String>, Option<String>) {
    match &span.source {
        SpanSource::Store(row) => (
            stored_text(row.values(), SpanCoreCol::InputPreview),
            stored_text(row.values(), SpanCoreCol::OutputPreview),
        ),
        SpanSource::Recent(_) => (None, None),
    }
}

/// One span with its previews.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct SpanRow {
    /// The span id, 16 hex characters.
    span_id: String,
    /// The parent span id; null for a root span.
    parent_span_id: Option<String>,
    /// The span name.
    name: String,
    /// `span`, `generation`, `event`, `embedding`, `agent`, `tool`, `chain`, `retriever`, `evaluator` or
    /// `guardrail`.
    observation_type: &'static str,
    /// The start, RFC 3339.
    start_time: String,
    /// The end, RFC 3339.
    end_time: String,
    /// The duration in microseconds.
    duration_us: u64,
    /// `unset`, `ok` or `error`.
    status_code: &'static str,
    /// `debug`, `default`, `warning` or `error`.
    level: &'static str,
    /// The model that answered, for a generation.
    model: String,
    /// The cost in USD.
    total_cost_usd: f64,
    /// Input tokens.
    input_tokens: u64,
    /// Output tokens.
    output_tokens: u64,
    /// Total tokens.
    total_tokens: u64,
    /// The number of direct children, when the response pages the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    child_count: Option<u64>,
    /// The first 1,024 bytes of the input; null while the span is only in the recent index (the seconds after
    /// it arrived) or when it was left out to keep the result small.
    input_preview: Option<String>,
    /// The first 1,024 bytes of the output, null like `input_preview`.
    output_preview: Option<String>,
    /// True when the previews were left out because the span alone came near the result cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    previews_omitted: bool,
    /// True when a count above 2^53 - 1 was clamped there.
    saturated: bool,
}

impl SpanRow {
    pub(crate) fn new(span: &TraceSpan) -> Self {
        let summary = &span.summary;
        let mut s = Saturation::default();
        let (input_preview, output_preview) = span_previews(span);
        Self {
            span_id: summary.span_id.to_string(),
            parent_span_id: (summary.parent_span_id != SpanId::ZERO)
                .then(|| summary.parent_span_id.to_string()),
            name: summary.name.to_string(),
            observation_type: summary.observation_type.as_str(),
            start_time: summary.start_time.to_string(),
            end_time: summary.end_time.to_string(),
            duration_us: s
                .u64(micros_between(summary.start_time, summary.end_time))
                .get(),
            status_code: summary.status_code.as_str(),
            level: summary.level.as_str(),
            model: summary.model.to_string(),
            total_cost_usd: usd_from_nanos(summary.total_cost_nusd.nanos()),
            input_tokens: s.u64(summary.input_tokens).get(),
            output_tokens: s.u64(summary.output_tokens).get(),
            total_tokens: s.u64(summary.total_tokens).get(),
            child_count: span.child_count.map(|count| s.u64(count).get()),
            input_preview,
            output_preview,
            previews_omitted: false,
            saturated: s.saturated(),
        }
    }

    /// Leaves the previews out.
    pub(crate) fn lighten(&mut self) {
        self.input_preview = None;
        self.output_preview = None;
        self.previews_omitted = true;
    }

    /// The largest span row: its name, model and previews at the caps of contract 3.1 (1,024, 256 and 1,024
    /// bytes), all control characters, which JSON escapes most.
    #[cfg(test)]
    pub(crate) fn at_caps() -> Self {
        let control = |bytes: usize| "\u{1}".repeat(bytes);
        Self {
            span_id: "000000000000000a".to_owned(),
            parent_span_id: Some("0000000000000001".to_owned()),
            name: control(1_024),
            observation_type: "generation",
            start_time: "2026-11-17T09:29:00.000000Z".to_owned(),
            end_time: "2026-11-17T09:29:01.250000Z".to_owned(),
            duration_us: 1_250_000,
            status_code: "error",
            level: "error",
            model: control(256),
            total_cost_usd: 0.000_123_4,
            input_tokens: 1_234,
            output_tokens: 567,
            total_tokens: 1_801,
            child_count: Some(3),
            input_preview: Some(control(1_024)),
            output_preview: Some(control(1_024)),
            previews_omitted: false,
            saturated: false,
        }
    }
}

/// One trace of the trace list.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct TraceRow {
    /// The trace id, 32 hex characters.
    trace_id: String,
    /// The name of the head span: the root, or the earliest span while the root has not arrived.
    name: String,
    /// The trace's name, from its root span or its latest named span.
    trace_name: String,
    /// `root`, or `provisional` while the root has not arrived.
    head_kind: &'static str,
    /// The start of the head span, RFC 3339.
    start_time: String,
    /// From the earliest start to the latest end of the trace's spans, in microseconds.
    duration_us: u64,
    /// The head span's status: `unset`, `ok` or `error`.
    status_code: &'static str,
    /// The head span's level: `debug`, `default`, `warning` or `error`.
    level: &'static str,
    /// Spans in the trace.
    span_count: u64,
    /// Spans of level `error`.
    error_count: u64,
    /// The trace's cost in USD.
    total_cost_usd: f64,
    /// Input tokens.
    input_tokens: u64,
    /// Output tokens.
    output_tokens: u64,
    /// Total tokens.
    total_tokens: u64,
    /// The trace's user.
    user_id: String,
    /// The trace's session.
    session_id: String,
    /// The head span's environment.
    environment: String,
    /// The head span's release.
    release: String,
    /// The trace's tags.
    tags: Vec<String>,
    /// True when the tags were left out because the trace alone came near the result cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    trimmed: bool,
    /// True when a count above 2^53 - 1 was clamped there.
    saturated: bool,
}

impl TraceRow {
    pub(crate) fn new(item: &TraceListItem) -> Self {
        let (head, summary) = (&item.head, &item.summary);
        let mut s = Saturation::default();
        Self {
            trace_id: head.trace_id.to_string(),
            name: head.name.to_string(),
            trace_name: summary.trace_name.to_string(),
            head_kind: head.head_kind.as_str(),
            start_time: head.start_time.to_string(),
            duration_us: s
                .u64(micros_between(summary.min_start, summary.max_end))
                .get(),
            status_code: head.status_code.as_str(),
            level: head.level.as_str(),
            span_count: s.u64(summary.span_count).get(),
            error_count: s.u64(summary.error_count).get(),
            total_cost_usd: usd_from_nanos(summary.total_cost_nusd.nanos()),
            input_tokens: s.u64(summary.input_tokens).get(),
            output_tokens: s.u64(summary.output_tokens).get(),
            total_tokens: s.u64(summary.total_tokens).get(),
            user_id: summary.user_id.to_string(),
            session_id: summary.session_id.to_string(),
            environment: head.environment.to_string(),
            release: head.release.to_string(),
            tags: head.tags.iter().map(ToString::to_string).collect(),
            trimmed: false,
            saturated: s.saturated(),
        }
    }

    /// Leaves the tags out: 64 tags of 256 bytes are the one part of a trace that can outgrow a result.
    pub(crate) fn lighten(&mut self) {
        self.tags = Vec::new();
        self.trimmed = true;
    }
}

/// One span of the span list. A row at the field caps of contract 3.1 fits a result on its own.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct SpanListRow {
    /// The trace id, 32 hex characters.
    trace_id: String,
    /// The span id, 16 hex characters.
    span_id: String,
    /// The parent span id; null for a root span.
    parent_span_id: Option<String>,
    /// The span name.
    name: String,
    /// `span`, `generation`, `event`, `embedding`, `agent`, `tool`, `chain`, `retriever`, `evaluator` or
    /// `guardrail`.
    observation_type: &'static str,
    /// The start, RFC 3339: the list's sort key.
    start_time: String,
    /// The duration in microseconds.
    duration_us: u64,
    /// `unset`, `ok` or `error`.
    status_code: &'static str,
    /// `debug`, `default`, `warning` or `error`.
    level: &'static str,
    /// The model that answered, for a generation.
    model: String,
    /// The span's environment.
    environment: String,
    /// The user the span names; empty when none.
    user_id: String,
    /// The session the span names; empty when none.
    session_id: String,
    /// The cost in USD.
    total_cost_usd: f64,
    /// Total tokens.
    total_tokens: u64,
    /// True when a count above 2^53 - 1 was clamped there.
    saturated: bool,
}

impl SpanListRow {
    /// The rows of a page, each with the sort tuple that a cursor after it resumes from.
    pub(crate) fn rows(page: &query::SpanListPage) -> Vec<((Timestamp, SpanId), Self)> {
        page.data
            .iter()
            .map(|span| {
                let mut s = Saturation::default();
                let row = Self {
                    trace_id: span.trace_id.to_string(),
                    span_id: span.span_id.to_string(),
                    parent_span_id: (span.parent_span_id != SpanId::ZERO)
                        .then(|| span.parent_span_id.to_string()),
                    name: span.name.to_string(),
                    observation_type: span.observation_type.as_str(),
                    start_time: span.start_time.to_string(),
                    duration_us: s.u64(micros_between(span.start_time, span.end_time)).get(),
                    status_code: span.status_code.as_str(),
                    level: span.level.as_str(),
                    model: span.model.to_string(),
                    environment: span.environment.to_string(),
                    user_id: span.user_id.to_string(),
                    session_id: span.session_id.to_string(),
                    total_cost_usd: usd_from_nanos(span.total_cost_nusd.nanos()),
                    total_tokens: s.u64(span.total_tokens).get(),
                    saturated: s.saturated(),
                };
                ((span.start_time, span.span_id), row)
            })
            .collect()
    }
}

/// One session of the session list.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct SessionRow {
    /// The session id, as the spans name it.
    session_id: String,
    /// The earliest span start, RFC 3339.
    first_seen: String,
    /// The latest span end, RFC 3339: the list's sort key.
    last_seen: String,
    /// Traces: the session's root spans.
    trace_count: u64,
    /// Spans.
    span_count: u64,
    /// Spans with an error.
    error_count: u64,
    /// The cost in USD.
    total_cost_usd: f64,
    /// Input tokens.
    input_tokens: u64,
    /// Output tokens.
    output_tokens: u64,
    /// Total tokens.
    total_tokens: u64,
    /// The users the session's spans name, ascending; at most 8 are kept.
    user_ids: Vec<String>,
    /// True when the user ids were left out because the session alone came near the result cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    trimmed: bool,
    /// True when a count above 2^53 - 1 was clamped there.
    saturated: bool,
}

impl SessionRow {
    pub(crate) fn new(session: &query::SessionSummary) -> Self {
        let mut s = Saturation::default();
        Self {
            session_id: session.session_id.to_string(),
            first_seen: session.first_seen.to_string(),
            last_seen: session.last_seen.to_string(),
            trace_count: s.u64(session.trace_count).get(),
            span_count: s.u64(session.span_count).get(),
            error_count: s.u64(session.error_count).get(),
            total_cost_usd: usd_from_nanos(session.total_cost_nusd.nanos()),
            input_tokens: s.u64(session.input_tokens).get(),
            output_tokens: s.u64(session.output_tokens).get(),
            total_tokens: s.u64(session.total_tokens).get(),
            user_ids: session.user_ids.iter().map(ToString::to_string).collect(),
            trimmed: false,
            saturated: s.saturated(),
        }
    }

    /// Leaves the user ids out: 8 ids of 512 bytes are the one part of a session that can outgrow a result.
    pub(crate) fn lighten(&mut self) {
        self.user_ids = Vec::new();
        self.trimmed = true;
    }
}

/// One user of the user list.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct UserRow {
    /// The user id, as the spans name it.
    user_id: String,
    /// The earliest span start, RFC 3339.
    first_seen: String,
    /// The latest span end, RFC 3339: the list's sort key.
    last_seen: String,
    /// Traces: the user's root spans.
    trace_count: u64,
    /// The distinct sessions that the user's traces in the window name.
    session_count: u64,
    /// Spans.
    span_count: u64,
    /// Spans with an error.
    error_count: u64,
    /// The cost in USD.
    total_cost_usd: f64,
    /// Input tokens.
    input_tokens: u64,
    /// Output tokens.
    output_tokens: u64,
    /// Total tokens.
    total_tokens: u64,
    /// True when a count above 2^53 - 1 was clamped there.
    saturated: bool,
}

impl UserRow {
    pub(crate) fn new(user: &query::UserSummary) -> Self {
        let mut s = Saturation::default();
        Self {
            user_id: user.user_id.to_string(),
            first_seen: user.first_seen.to_string(),
            last_seen: user.last_seen.to_string(),
            trace_count: s.u64(user.trace_count).get(),
            session_count: s.u64(user.session_count).get(),
            span_count: s.u64(user.span_count).get(),
            error_count: s.u64(user.error_count).get(),
            total_cost_usd: usd_from_nanos(user.total_cost_nusd.nanos()),
            input_tokens: s.u64(user.input_tokens).get(),
            output_tokens: s.u64(user.output_tokens).get(),
            total_tokens: s.u64(user.total_tokens).get(),
            saturated: s.saturated(),
        }
    }
}

/// One session of a user.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct UserSessionRow {
    /// The session id.
    session_id: String,
    /// The start of the session's earliest trace of the user in the window, RFC 3339.
    first_trace_at: String,
    /// The start of the latest one, RFC 3339: the list's sort key.
    last_trace_at: String,
    /// The user's traces in the session.
    trace_count: u64,
    /// True when a count above 2^53 - 1 was clamped there.
    saturated: bool,
}

impl UserSessionRow {
    pub(crate) fn new(session: &query::UserSession) -> Self {
        let mut s = Saturation::default();
        Self {
            session_id: session.session_id.to_string(),
            first_trace_at: session.first_trace_at.to_string(),
            last_trace_at: session.last_trace_at.to_string(),
            trace_count: s.u64(session.trace_count).get(),
            saturated: s.saturated(),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde::Serialize;

    use super::*;

    #[derive(Clone, Debug, Serialize)]
    struct Item {
        text: String,
        light: bool,
    }

    #[derive(Serialize)]
    struct Page {
        data: Vec<Item>,
        cut: bool,
        next: Option<&'static str>,
    }

    fn page(rows: &[Item], cut: bool) -> Page {
        Page {
            data: rows.to_vec(),
            cut,
            next: cut.then_some("v1.k.cursor-of-the-last-row-of-the-prefix"),
        }
    }

    /// `count` copies of `row` as a cut page of a list, with a cursor of the longest kind.
    fn list<R: Serialize>(row: &R, count: usize) -> serde_json::Value {
        let row = serde_json::to_value(row).unwrap();
        serde_json::json!({
            "data": vec![row; count],
            "next_cursor": "v".repeat(200),
            "has_more": true,
            "truncated": true,
            "meta": { "as_of": "2026-11-17T09:30:00.000000Z", "partial": false, "consistency": "strong" },
        })
    }

    /// The most copies of `row` that a result holds under the cap.
    fn rows_per_result<R: Serialize>(row: &R) -> usize {
        (1..=1_000)
            .take_while(|&count| result_bytes(&list(row, count)) <= MAX_RESULT_BYTES)
            .last()
            .unwrap_or(0)
    }

    /// Text that JSON escapes most: a control character costs 13 bytes of a result, 6 in the JSON and 7 in its
    /// copy as an escaped string.
    fn control(bytes: usize) -> String {
        "\u{1}".repeat(bytes)
    }

    fn span_row(name: String, model: String, preview: Option<String>) -> SpanRow {
        SpanRow {
            span_id: "000000000000000a".to_owned(),
            parent_span_id: Some("0000000000000001".to_owned()),
            name,
            observation_type: "generation",
            start_time: "2026-11-17T09:29:00.000000Z".to_owned(),
            end_time: "2026-11-17T09:29:01.250000Z".to_owned(),
            duration_us: 1_250_000,
            status_code: "ok",
            level: "default",
            model,
            total_cost_usd: 0.000_123_4,
            input_tokens: 1_234,
            output_tokens: 567,
            total_tokens: 1_801,
            child_count: Some(3),
            input_preview: preview.clone(),
            output_preview: preview,
            previews_omitted: false,
            saturated: false,
        }
    }

    fn trace_row(text: impl Fn(usize) -> String, tags: Vec<String>) -> TraceRow {
        TraceRow {
            trace_id: "a5a5a5a5a5a5a5a50000000000000001".to_owned(),
            name: text(1_024),
            trace_name: text(512),
            head_kind: "root",
            start_time: "2026-11-17T09:29:00.000000Z".to_owned(),
            duration_us: 1_250_000,
            status_code: "error",
            level: "error",
            span_count: 40,
            error_count: 2,
            total_cost_usd: 0.012_5,
            input_tokens: 12_345,
            output_tokens: 6_789,
            total_tokens: 19_134,
            user_id: text(512),
            session_id: text(512),
            environment: text(128),
            release: text(256),
            tags,
            trimmed: false,
            saturated: false,
        }
    }

    #[test]
    fn a_row_at_the_field_caps_fits_a_result_on_its_own() {
        // The caps of contract 3.1: a name of 1,024 bytes, a model of 256 and previews of 1,024.
        let span = SpanRow::at_caps();
        assert!(rows_per_result(&span) >= 1);
        // 64 tags of 256 bytes are what can outgrow a result; a trace without them fits.
        let mut trace = trace_row(control, vec![control(256); 64]);
        assert_eq!(rows_per_result(&trace), 0);
        trace.lighten();
        assert!(rows_per_result(&trace) >= 1);
    }

    #[test]
    fn a_result_holds_a_dozen_spans_with_full_previews_and_a_whole_page_of_traces() {
        // The result carries its JSON twice, so a span whose input and output fill their previews takes about
        // 5 KB of the 64 KiB, and one without previews about 1 KB.
        let full = span_row(
            "llm call".to_owned(),
            "model-a".to_owned(),
            Some("a".repeat(1_024)),
        );
        assert!(
            (10..=14).contains(&rows_per_result(&full)),
            "{}",
            rows_per_result(&full)
        );
        let bare = span_row("tool call".to_owned(), String::new(), None);
        assert!(
            (55..=80).contains(&rows_per_result(&bare)),
            "{}",
            rows_per_result(&bare)
        );
        // A trace with everyday names holds the 50 traces of a page whole.
        let trace = trace_row(|bytes| "x".repeat(bytes.min(32)), vec!["prod".to_owned()]);
        assert!(rows_per_result(&trace) >= 50, "{}", rows_per_result(&trace));
    }

    /// Text that JSON escapes heavily: control characters, quotes and backslashes cost up to 13 bytes each in
    /// the result, which carries the JSON once and once more as an escaped string.
    fn hostile() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop_oneof![
                Just('\u{0}'),
                Just('"'),
                Just('\\'),
                Just('\n'),
                Just('é'),
                Just('a')
            ],
            0..3_000,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        // Each case serializes pages of up to 120 KB several times.
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Every page fits the cap and holds at least one row, and paging on from each cut reads every row once.
        #[test]
        fn pages_fit_the_cap_and_paging_reads_every_row_once(
            texts in prop::collection::vec(hostile(), 0..40),
        ) {
            let rows: Vec<Item> = texts.into_iter().map(|text| Item { text, light: false }).collect();
            let lighten = |item: &mut Item| {
                item.text.clear();
                item.light = true;
            };
            let mut rest = rows.clone();
            let mut read = Vec::new();
            loop {
                let output = fit(rest.clone(), lighten, page);
                prop_assert!(result_bytes(&output) <= MAX_RESULT_BYTES);
                prop_assert!(!output.data.is_empty() || rest.is_empty());
                prop_assert_eq!(output.cut, output.data.len() < rest.len());
                let taken = output.data.len();
                read.extend(output.data);
                rest.drain(..taken);
                if rest.is_empty() {
                    break;
                }
            }
            prop_assert_eq!(read.len(), rows.len());
            for (read, row) in read.iter().zip(&rows) {
                prop_assert!(read.light || read.text == row.text);
            }
        }
    }
}
