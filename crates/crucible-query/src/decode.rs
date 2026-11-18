//! Typed views of the rows the store returns for the plans of this crate. A row that does not follow its plan's
//! output is a defect of the store and maps to `internal_error`.

use std::sync::Arc;

use crucible_plan::{Row, Value};
use crucible_recent::SpanSummary;
use crucible_records::{
    HeadKind, Level, Lsn, NanoUsd, ObservationType, SpanId, StatusCode, Timestamp, TraceId,
};
use crucible_schema::{Column, SpanCoreCol, TraceHeadsCol};

use crate::context::QueryServiceError;

type Decoded<T> = Result<T, QueryServiceError>;

fn bad(what: &str) -> QueryServiceError {
    QueryServiceError::Internal(format!("the store returned a row without a valid {what}"))
}

/// The values of a row, read by position.
pub(crate) struct Fields<'a>(pub(crate) &'a [Value]);

impl Fields<'_> {
    fn get(&self, index: usize, what: &str) -> Decoded<&Value> {
        self.0.get(index).ok_or_else(|| bad(what))
    }

    pub(crate) fn time(&self, index: usize) -> Decoded<Timestamp> {
        match self.get(index, "time")? {
            Value::Time(t) => Ok(*t),
            _ => Err(bad("time")),
        }
    }

    pub(crate) fn uint(&self, index: usize) -> Decoded<u64> {
        match self.get(index, "unsigned integer")? {
            Value::UInt(v) => Ok(*v),
            _ => Err(bad("unsigned integer")),
        }
    }

    pub(crate) fn int(&self, index: usize) -> Decoded<i64> {
        match self.get(index, "integer")? {
            Value::Int(v) => Ok(*v),
            _ => Err(bad("integer")),
        }
    }

    pub(crate) fn enum8(&self, index: usize) -> Decoded<u8> {
        match self.get(index, "enum")? {
            Value::Enum(v) => Ok(*v),
            _ => Err(bad("enum")),
        }
    }

    /// The bytes of a `String` or `FixedString` value.
    pub(crate) fn bytes(&self, index: usize) -> Decoded<&[u8]> {
        match self.get(index, "string")? {
            Value::String(text) => Ok(text.as_bytes()),
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(bad("string")),
        }
    }

    /// A `String` value as text; bytes that are not UTF-8 are replaced.
    pub(crate) fn text(&self, index: usize) -> Decoded<Arc<str>> {
        Ok(Arc::from(String::from_utf8_lossy(self.bytes(index)?)))
    }

    pub(crate) fn trace_id(&self, index: usize) -> Decoded<TraceId> {
        let bytes = self.bytes(index)?.try_into().map_err(|_| bad("trace id"))?;
        Ok(TraceId::from_bytes(bytes))
    }

    pub(crate) fn span_id(&self, index: usize) -> Decoded<SpanId> {
        let bytes = self.bytes(index)?.try_into().map_err(|_| bad("span id"))?;
        Ok(SpanId::from_bytes(bytes))
    }

    pub(crate) fn texts(&self, index: usize) -> Decoded<Vec<Arc<str>>> {
        match self.get(index, "array")? {
            Value::Array(items) => (0..items.len()).map(|i| Fields(items).text(i)).collect(),
            _ => Err(bad("array")),
        }
    }
}

/// The position of a column in a row of a plan that selects `C::ALL`.
fn at<C: Column + PartialEq>(column: C) -> usize {
    C::ALL
        .iter()
        .position(|c| *c == column)
        .unwrap_or(usize::MAX)
}

fn level(value: u8) -> Decoded<Level> {
    Level::from_value(value).ok_or_else(|| bad("level"))
}

fn status(value: u8) -> Decoded<StatusCode> {
    StatusCode::from_value(value).ok_or_else(|| bad("status"))
}

/// A `span_core` row of a plan that selects every column.
pub(crate) fn span(row: &Row) -> Decoded<SpanSummary> {
    use SpanCoreCol as C;
    let f = Fields(row.values());
    Ok(SpanSummary {
        trace_id: f.trace_id(at(C::TraceId))?,
        span_id: f.span_id(at(C::SpanId))?,
        parent_span_id: f.span_id(at(C::ParentSpanId))?,
        is_root: f.uint(at(C::IsRoot))? != 0,
        start_time: f.time(at(C::StartTime))?,
        end_time: f.time(at(C::EndTime))?,
        observation_type: ObservationType::from_value(f.enum8(at(C::ObservationType))?)
            .ok_or_else(|| bad("observation type"))?,
        level: level(f.enum8(at(C::Level))?)?,
        status_code: status(f.enum8(at(C::StatusCode))?)?,
        name: f.text(at(C::Name))?,
        trace_name: f.text(at(C::TraceName))?,
        user_id: f.text(at(C::UserId))?,
        session_id: f.text(at(C::SessionId))?,
        model: f.text(at(C::ModelResponse))?,
        environment: f.text(at(C::Environment))?,
        total_cost_nusd: NanoUsd::from_nanos(f.int(at(C::TotalCostNusd))?),
        input_tokens: f.uint(at(C::InputTokens))?,
        output_tokens: f.uint(at(C::OutputTokens))?,
        total_tokens: f.uint(at(C::TotalTokens))?,
    })
}

/// One trace head: the row of `trace_heads` that drives the trace list, or the head stage B emitted for a span
/// that is still in the recent index. Every value is the head span's (contract 3.9).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceHeadView {
    /// The trace.
    pub trace_id: TraceId,
    /// The head span.
    pub span_id: SpanId,
    /// `root` or `provisional`.
    pub head_kind: HeadKind,
    /// The list sort key.
    pub start_time: Timestamp,
    /// End of the head span.
    pub end_time: Timestamp,
    /// The head span's name.
    pub name: Arc<str>,
    /// The head span's level.
    pub level: Level,
    /// The head span's status.
    pub status_code: StatusCode,
    /// The head span's user.
    pub user_id: Arc<str>,
    /// The head span's session.
    pub session_id: Arc<str>,
    /// The head span's trace tags. Empty for a head that is only in the recent index.
    pub tags: Vec<Arc<str>>,
    /// Empty for a head that is only in the recent index.
    pub environment: Arc<str>,
    /// Empty for a head that is only in the recent index.
    pub release: Arc<str>,
    /// Empty for a head that is only in the recent index.
    pub version: Arc<str>,
    /// Empty for a head that is only in the recent index.
    pub input_preview: Arc<str>,
    /// Empty for a head that is only in the recent index.
    pub output_preview: Arc<str>,
    /// The LSN of the frame that carried the head span.
    pub lsn: Lsn,
    /// When the WAL accepted that frame.
    pub received_at: Timestamp,
}

impl TraceHeadView {
    /// The sort tuple `(start_time, trace_hash, trace_id)`.
    pub fn key(&self) -> (Timestamp, u32, TraceId) {
        (self.start_time, self.trace_id.trace_hash(), self.trace_id)
    }
}

/// A `trace_heads` row of a plan that selects every column.
pub(crate) fn head(row: &Row) -> Decoded<TraceHeadView> {
    use TraceHeadsCol as C;
    let f = Fields(row.values());
    Ok(TraceHeadView {
        trace_id: f.trace_id(at(C::TraceId))?,
        span_id: f.span_id(at(C::SpanId))?,
        head_kind: HeadKind::from_value(f.enum8(at(C::HeadKind))?)
            .ok_or_else(|| bad("head kind"))?,
        start_time: f.time(at(C::StartTime))?,
        end_time: f.time(at(C::EndTime))?,
        name: f.text(at(C::Name))?,
        level: level(f.enum8(at(C::Level))?)?,
        status_code: status(f.enum8(at(C::StatusCode))?)?,
        user_id: f.text(at(C::UserId))?,
        session_id: f.text(at(C::SessionId))?,
        tags: f.texts(at(C::Tags))?,
        environment: f.text(at(C::Environment))?,
        release: f.text(at(C::Release))?,
        version: f.text(at(C::Version))?,
        input_preview: f.text(at(C::InputPreview))?,
        output_preview: f.text(at(C::OutputPreview))?,
        lsn: Lsn::new(f.uint(at(C::Lsn))?),
        received_at: f.time(at(C::ReceivedAt))?,
    })
}
