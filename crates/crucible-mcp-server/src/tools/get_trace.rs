//! `get_trace`: one trace as a span tree with previews (contract 8.7 and 8.8), up to 200 spans per call.

use crucible_api_types::Saturation;
use crucible_records::{SpanId, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{Meta, SpanRow, fit, meta};
use crate::tools::{Call, Check, Tool};

/// The most spans one call returns.
const MAX_SPANS: usize = 200;

pub(crate) struct GetTrace;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The trace id, 32 hex characters.
    trace_id: String,
    /// The `next_cursor` of the previous call for the same trace.
    #[serde(default)]
    cursor: Option<String>,
}

/// A trace's spans in start order, from the first or from the cursor.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The trace id.
    trace_id: String,
    /// Spans in the whole trace.
    span_count: u64,
    /// False: the trace's spans are all listed across the calls. True (above 5,000 spans): only the root spans
    /// and their direct children are listed, each with `child_count`; get_span_children reads further down.
    paged: bool,
    /// The spans, in start order; `parent_span_id` links them into a tree.
    spans: Vec<SpanRow>,
    /// Pass it as `cursor` with the same `trace_id` for the spans after these; null when none are left.
    next_cursor: Option<String>,
    /// True when spans are left after these, because of the cap of 200 spans or of 64 KiB per call.
    truncated: bool,
    /// True when `span_count` was above 2^53 - 1 and is clamped there.
    saturated: bool,
    /// How complete and how fresh the answer is: an empty trace that is `partial` or `lagging` may exist.
    meta: Meta,
}

impl Tool for GetTrace {
    const NAME: &'static str = "get_trace";
    const DESCRIPTION: &'static str = "Read one trace: its spans in start order with parent links, names, types, \
        timing, status, model, tokens, cost, and the first 1,024 bytes of each span's input and output (never \
        whole payloads). At most 200 spans and 64 KiB per call; when `truncated` is true, pass `next_cursor` back \
        as `cursor` for the next spans. Traces above 5,000 spans come back `paged`: the root spans and their \
        direct children, each with a child count; read deeper levels with get_span_children.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        let trace_id = check.trace_id("trace_id", &args.trace_id);
        check.finish()?;
        let Some(trace_id) = trace_id else {
            return Err(ToolError::validation(Vec::new()));
        };
        let after = args
            .cursor
            .map(|text| call.cursors.open_trace(&text, trace_id))
            .transpose()
            .map_err(ToolError::cursor)?;
        let detail = call.service.trace_by_id(&call.ctx, trace_id).await;
        let detail = detail.map_err(|e| ToolError::query(&e, call.request_id))?;
        let rest = detail.spans.iter().filter(|span| {
            let key = (span.summary.start_time, span.summary.span_id);
            after.is_none_or(|after| key > after)
        });
        let mut rows: Vec<((Timestamp, SpanId), SpanRow)> = rest
            .map(|span| {
                let key = (span.summary.start_time, span.summary.span_id);
                (key, SpanRow::new(span))
            })
            .take(MAX_SPANS + 1)
            .collect();
        let over_cap = rows.len() > MAX_SPANS;
        rows.truncate(MAX_SPANS);
        let mut s = Saturation::default();
        let span_count = s.u64(detail.span_count).get();
        Ok(fit(
            rows,
            |(_, row)| row.lighten(),
            |prefix, cut| {
                let truncated = cut || over_cap;
                Output {
                    trace_id: trace_id.to_string(),
                    span_count,
                    paged: detail.paged,
                    spans: prefix.iter().map(|(_, row)| row.clone()).collect(),
                    next_cursor: prefix
                        .last()
                        .filter(|_| truncated)
                        .map(|(last, _)| call.cursors.seal_trace(trace_id, *last)),
                    truncated,
                    saturated: s.saturated(),
                    meta: meta(&detail.meta),
                }
            },
        ))
    }
}
