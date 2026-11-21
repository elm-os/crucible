//! The payload viewer (contracts 8.7 and 12.6): a byte range of one payload field of one span, with the
//! field's total length.

use crucible_records::{ProjectScope, SpanId, TableId, Timestamp, TraceId};
use crucible_schema::{Column, ColumnRef, SpanPayloadCol};

use crate::build::{RetentionClamp, trace_bounds_window};
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{FieldRange, Node, PointLookup};
use crate::plan::QueryPlan;

/// A payload field the viewer can read: the string columns of `span_payload`. The `metadata` selector of the
/// route is an array pair and the MCP fields live in `mcp_content`; neither is a field here yet.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PayloadField {
    /// `input`.
    Input,
    /// `output`.
    Output,
    /// `system_instructions`.
    SystemInstructions,
    /// `attributes`.
    Attributes,
    /// `resource_attributes`.
    ResourceAttributes,
    /// `events`.
    Events,
    /// `links`.
    Links,
    /// `tool_definitions`.
    ToolDefinitions,
    /// `tool_calls`.
    ToolCalls,
}

impl PayloadField {
    /// Every field.
    pub const ALL: [Self; 9] = [
        Self::Input,
        Self::Output,
        Self::SystemInstructions,
        Self::Attributes,
        Self::ResourceAttributes,
        Self::Events,
        Self::Links,
        Self::ToolDefinitions,
        Self::ToolCalls,
    ];

    /// The column, whose name is the field's selector on the route.
    pub fn column(self) -> ColumnRef {
        match self {
            Self::Input => SpanPayloadCol::Input,
            Self::Output => SpanPayloadCol::Output,
            Self::SystemInstructions => SpanPayloadCol::SystemInstructions,
            Self::Attributes => SpanPayloadCol::Attributes,
            Self::ResourceAttributes => SpanPayloadCol::ResourceAttributes,
            Self::Events => SpanPayloadCol::Events,
            Self::Links => SpanPayloadCol::Links,
            Self::ToolDefinitions => SpanPayloadCol::ToolDefinitions,
            Self::ToolCalls => SpanPayloadCol::ToolCalls,
        }
        .column_ref()
    }

    /// The selector.
    pub fn as_str(self) -> &'static str {
        self.column().name()
    }

    /// The field with this selector.
    pub fn parse(selector: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == selector)
    }
}

/// A byte range of one payload field of one span.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PayloadRangeRequest {
    /// The span's trace.
    pub trace_id: TraceId,
    /// The span.
    pub span_id: SpanId,
    /// The span's `start_time`, which the caller has from the span's core row.
    pub start_time: Timestamp,
    /// The field.
    pub field: PayloadField,
    /// The first byte.
    pub offset: u64,
    /// The number of bytes, at least 1.
    pub len: u64,
}

/// Bytes `[offset, offset + len)` of one payload field of one span. The outputs are, in order: the bytes (a
/// `String` value, or `Bytes` when the range cuts a character) and the field's total length in bytes. No row
/// means the span has no payload row (it expired under the payload retention, or was never stored); a range
/// past the end returns empty bytes with the total.
pub fn payload_range(
    scope: ProjectScope,
    clamp: &RetentionClamp,
    caller: Caller,
    request: PayloadRangeRequest,
) -> Result<Option<QueryPlan>, PlanError> {
    let range = FieldRange::new(
        TableId::SpanPayload,
        [
            SpanPayloadCol::StartMinute,
            SpanPayloadCol::TraceHash,
            SpanPayloadCol::SpanId,
            SpanPayloadCol::TraceId,
        ],
        vec![
            request.start_time.floor_minute().into(),
            request.trace_id.trace_hash().into(),
            Literal::from(request.span_id),
            request.trace_id.into(),
        ],
        request.field.column(),
        request.offset,
        request.len,
    )?
    .with_total();
    clamp.plan(
        scope,
        trace_bounds_window(request.start_time, request.start_time)?,
        caller.class(QueryClass::Point),
        Node::FieldRange(range),
    )
}

/// The bodies of one span's payload that can reference media (contract 14): the outputs are, in order, `input`,
/// `output`, `metadata_keys`, `metadata_values` and `lsn`. A span sent twice has a row per copy, told apart by
/// `lsn`; a span whose payload expired under the payload retention, or was never stored, has none.
pub fn span_bodies(
    scope: ProjectScope,
    clamp: &RetentionClamp,
    caller: Caller,
    trace_id: TraceId,
    span_id: SpanId,
    start_time: Timestamp,
) -> Result<Option<QueryPlan>, PlanError> {
    let lookup = PointLookup::new(
        TableId::SpanPayload,
        [
            SpanPayloadCol::StartMinute,
            SpanPayloadCol::TraceHash,
            SpanPayloadCol::SpanId,
            SpanPayloadCol::TraceId,
        ],
        vec![vec![
            start_time.floor_minute().into(),
            trace_id.trace_hash().into(),
            Literal::from(span_id),
            trace_id.into(),
        ]],
        [
            SpanPayloadCol::Input,
            SpanPayloadCol::Output,
            SpanPayloadCol::MetadataKeys,
            SpanPayloadCol::MetadataValues,
            SpanPayloadCol::Lsn,
        ],
    )?;
    clamp.plan(
        scope,
        trace_bounds_window(start_time, start_time)?,
        caller.class(QueryClass::Point),
        Node::PointLookup(lookup),
    )
}
