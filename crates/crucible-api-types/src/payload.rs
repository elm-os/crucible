//! The payload viewer: `GET projects/{project_id}/spans/{trace_id}/{span_id}/payload` (contract 8.7 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

/// A payload field the viewer reads: the `field` selector.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum PayloadField {
    /// The input, verbatim (JSON or text).
    Input,
    /// The output, verbatim (JSON or text).
    Output,
    /// The system instructions (JSON).
    SystemInstructions,
    /// The span attributes no mapping consumed (a JSON object).
    Attributes,
    /// The resource attributes no mapping consumed (a JSON object).
    ResourceAttributes,
    /// The span events (a JSON list).
    Events,
    /// The span links (a JSON list).
    Links,
    /// The tool definitions (JSON).
    ToolDefinitions,
    /// The tool calls (a JSON list).
    ToolCalls,
}

/// The query of `GET projects/{project_id}/spans/{trace_id}/{span_id}/payload`. The body is the field's bytes
/// as stored; a `Range: bytes=<first>-[<last>]` header asks for part of it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct PayloadParams {
    /// The field to read.
    pub field: PayloadField,
    /// The span's start time as the API returned it (RFC 3339): it locates the span's payload.
    #[param(format = DateTime)]
    pub start_time: String,
    /// `1` sends the field as a download (`Content-Disposition: attachment`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 1)]
    #[ts(optional)]
    pub download: Option<u8>,
}
