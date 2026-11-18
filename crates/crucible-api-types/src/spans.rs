//! The children of a span: `GET projects/{project_id}/traces/{trace_id}/spans` (contract 8.8 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::meta::Meta;
use crate::trace_detail::Span;

/// The query of `GET projects/{project_id}/traces/{trace_id}/spans`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct SpanPageParams {
    /// The span whose direct children to list (16 hex characters).
    pub parent_span_id: String,
    /// The `next_cursor` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cursor: Option<String>,
    /// Page size: 1 to 2,000, default 500.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 2000)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// One page of the direct children of a span, in start order. Each carries `child_count`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SpanPage {
    /// The children.
    pub data: Vec<Span>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// How complete and how fresh the page is.
    pub meta: Meta,
}
