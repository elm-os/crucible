//! Data bounds (contract 8.8): where a project's data begins and ends.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::meta::Meta;

/// The first and last minute with data; both null when there is none.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct TimeRange {
    /// The first minute with data.
    #[schema(required = true, format = DateTime)]
    pub first: Option<String>,
    /// The last minute with data.
    #[schema(required = true, format = DateTime)]
    pub last: Option<String>,
}

/// MCP operations by the side that captured them.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct McpBounds {
    /// Operations captured on the client side.
    pub client: TimeRange,
    /// Operations captured on the server side.
    pub server: TimeRange,
}

/// Where a project's data begins and ends, inside its retention, with minute precision.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct DataBounds {
    /// Spans.
    pub spans: TimeRange,
    /// MCP operations.
    pub mcp: McpBounds,
    /// The first instant that has not expired; null when the project keeps its data forever.
    #[schema(required = true, format = DateTime)]
    pub retention_horizon: Option<String>,
    /// The widest window a list or analytics request may ask for.
    pub max_window_days: u32,
    /// The server's clock when it answered.
    #[schema(format = DateTime)]
    pub server_time: String,
    /// How complete and how fresh the response is.
    pub meta: Meta,
}
