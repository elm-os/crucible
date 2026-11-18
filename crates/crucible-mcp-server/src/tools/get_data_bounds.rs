//! `get_data_bounds`: where the project's data begins and ends (contract 8.8), so that an agent picks a window
//! before it lists.

use crucible_query::MinuteRange;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{Meta, meta};
use crate::tools::{Call, Tool};

pub(crate) struct GetDataBounds;

/// No arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {}

/// The first and last minute with data; both null when there is none.
#[derive(Serialize, JsonSchema)]
pub(crate) struct TimeRange {
    /// The first minute with data, RFC 3339.
    first: Option<String>,
    /// The last minute with data, RFC 3339.
    last: Option<String>,
}

fn range(range: Option<MinuteRange>) -> TimeRange {
    TimeRange {
        first: range.map(|r| r.first.to_string()),
        last: range.map(|r| r.last.to_string()),
    }
}

/// MCP operations by the side that captured them.
#[derive(Serialize, JsonSchema)]
pub(crate) struct McpBounds {
    /// Operations captured on the client side.
    client: TimeRange,
    /// Operations captured on the server side.
    server: TimeRange,
}

/// Where the project's data begins and ends inside its retention, with minute precision.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// Spans.
    spans: TimeRange,
    /// MCP operations.
    mcp: McpBounds,
    /// The first instant that has not expired; null when the project keeps its data forever.
    retention_horizon: Option<String>,
    /// The widest window, in days, that `list_traces` accepts.
    max_window_days: u32,
    /// The server's clock when it answered, RFC 3339.
    server_time: String,
    /// How complete and how fresh the answer is.
    meta: Meta,
}

impl Tool for GetDataBounds {
    const NAME: &'static str = "get_data_bounds";
    const DESCRIPTION: &'static str = "Where the project's data begins and ends: the first and last minute \
        with spans and with MCP operations, the retention horizon, the widest window the list tools accept, and \
        the server's time. Call it first to choose `from` and `to` for list_traces.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, Args {}: Args) -> Result<Output, ToolError> {
        let bounds = call.service.data_bounds(&call.ctx).await;
        let bounds = bounds.map_err(|e| ToolError::query(&e, call.request_id))?;
        Ok(Output {
            spans: range(bounds.spans),
            mcp: McpBounds {
                client: range(bounds.mcp_client),
                server: range(bounds.mcp_server),
            },
            retention_horizon: bounds.retention_horizon.map(|t| t.to_string()),
            max_window_days: call.settings.max_window_days,
            server_time: bounds.meta.as_of.to_string(),
            meta: meta(&bounds.meta),
        })
    }
}
