//! What the MCP analytics tools share (contract 9.9): the arguments every one takes and the rows they return,
//! with the field names of the REST DTOs.

use crucible_api_types::Saturation;
use crucible_plan::{CaptureClass, ServerSources, SourceChoice};
use crucible_query::{McpQuery, McpTotals};
use crucible_records::McpSide;
use schemars::JsonSchema;
use serde::Serialize;

use crate::error::ToolError;
use crate::tools::{Call, Check, check_window};

/// The arguments every MCP analytics tool takes, before they are checked.
pub(crate) struct Common<'a> {
    pub(crate) side: &'a str,
    pub(crate) from: &'a str,
    pub(crate) to: &'a str,
    pub(crate) source: Option<&'a str>,
    pub(crate) include_caller_faults: Option<bool>,
    pub(crate) server: Option<String>,
}

/// The checked arguments, before the window is checked against the project's maximum.
pub(crate) struct Checked {
    query: Option<McpQuery>,
}

impl Common<'_> {
    /// Checks the arguments; their problems join `check`.
    pub(crate) fn check(self, check: &mut Check) -> Checked {
        let window = check.window(self.from, self.to);
        let side = match self.side {
            "client" => Some(McpSide::Client),
            "server" => Some(McpSide::Server),
            _ => {
                check.error("side", "invalid", "expected client or server");
                None
            }
        };
        let source = match self.source {
            None => Some(SourceChoice::Auto),
            Some(text) => {
                let source = SourceChoice::parse(text);
                if source.is_none() {
                    check.error("source", "invalid", "expected auto, sdk, proxy or otlp");
                }
                source
            }
        };
        let query = match (window, side, source) {
            (Some(window), Some(side), Some(source)) => Some(McpQuery {
                window,
                side,
                source,
                include_caller_faults: self.include_caller_faults.unwrap_or(false),
                server: self.server,
            }),
            _ => None,
        };
        Checked { query }
    }
}

impl Checked {
    /// The query, once every argument of the call passed: its window is checked against the project's maximum.
    pub(crate) fn query(self, call: &Call<'_>) -> Result<McpQuery, ToolError> {
        let query = self
            .query
            .ok_or_else(|| ToolError::validation(Vec::new()))?;
        check_window(query.window, call.settings.max_window_days)?;
        Ok(query)
    }
}

/// Latency percentiles in microseconds; null without calls.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct Latency {
    /// The median.
    p50_us: Option<u64>,
    /// The 90th percentile.
    p90_us: Option<u64>,
    /// The 99th percentile.
    p99_us: Option<u64>,
}

impl Latency {
    pub(crate) fn of(latency: &crucible_query::Latency, s: &mut Saturation) -> Self {
        Self {
            p50_us: latency.p50.map(|v| s.u64(v).get()),
            p90_us: latency.p90.map(|v| s.u64(v).get()),
            p99_us: latency.p99.map(|v| s.u64(v).get()),
        }
    }
}

/// The measures of a set of calls.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct Stats {
    /// Calls, without the caller-fault calls unless `include_caller_faults` was true.
    calls: u64,
    /// Calls with an error class other than `none`, under the same rule.
    errors: u64,
    /// `errors / calls`; null without calls.
    error_rate: Option<f64>,
    /// Calls the caller caused (invalid requests, unknown methods), counted either way.
    caller_faults: u64,
    /// Latency of the calls counted.
    latency: Latency,
    /// When the last call was seen, RFC 3339.
    last_seen: Option<String>,
}

impl Stats {
    pub(crate) fn of(totals: &McpTotals, s: &mut Saturation) -> Self {
        Self {
            calls: s.u64(totals.calls).get(),
            errors: s.u64(totals.errors).get(),
            error_rate: (totals.calls > 0).then(|| totals.errors as f64 / totals.calls as f64),
            caller_faults: s.u64(totals.caller_faults).get(),
            latency: Latency::of(&totals.latency, s),
            last_seen: totals.last_seen.map(|t| t.to_string()),
        }
    }
}

/// The capture source one server was read from.
#[derive(Clone, PartialEq, Debug, Serialize, JsonSchema)]
pub(crate) struct ServerSource {
    /// The server.
    server_name: String,
    /// The classes it has rows of in the window, best first: `sdk`, `proxy`, `otlp`.
    available: Vec<&'static str>,
    /// The class read; null when `source` named a class the server has no rows of.
    chosen: Option<&'static str>,
}

pub(crate) fn sources(servers: &[ServerSources]) -> Vec<ServerSource> {
    servers
        .iter()
        .map(|s| ServerSource {
            server_name: s.server_name.clone(),
            available: s.available.iter().map(|c| c.as_str()).collect(),
            chosen: s.chosen.map(CaptureClass::as_str),
        })
        .collect()
}
