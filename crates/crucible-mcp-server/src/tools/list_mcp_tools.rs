//! `list_mcp_tools`: the tools of one side (contract 9.9 `tools`), with calls, errors, latency, the latest
//! schema hash and the listed tools that were never called.

use crucible_api_types::Saturation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::mcp::{Common, ServerSource, Stats, sources};
use crate::shape::{Meta, fit, meta};
use crate::tools::{Call, Check, Tool};

pub(crate) struct ListMcpTools;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// `client` or `server`: the side to read. The two sides are never summed.
    side: String,
    /// The start of the window, RFC 3339 (inclusive). Required.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// Only this server.
    #[serde(default)]
    server: Option<String>,
    /// `auto` (default), `sdk`, `proxy` or `otlp`: which capture source each server is read from.
    #[serde(default)]
    source: Option<String>,
    /// Count calls the caller caused (invalid requests) in the measures; false by default.
    #[serde(default)]
    include_caller_faults: Option<bool>,
}

/// One tool.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct Row {
    /// The server.
    server_name: String,
    /// The tool.
    tool_name: String,
    /// Its calls.
    stats: Stats,
    /// Request bytes of the calls counted.
    request_bytes: u64,
    /// Response bytes of the calls counted.
    response_bytes: u64,
    /// The xxh3-64 of its latest listed schema, 16 hex characters; null when no definition was listed.
    schema_hash: Option<String>,
    /// Listed by the server but not called in the window.
    unused: bool,
}

/// The tools, most calls first, then the unused ones.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The tools.
    data: Vec<Row>,
    /// The capture source each server was read from.
    capture_sources: Vec<ServerSource>,
    /// True when tools were left out to keep the result under 64 KiB; pass `server` to narrow.
    truncated: bool,
    /// How complete and how fresh the answer is.
    meta: Meta,
}

impl Tool for ListMcpTools {
    const NAME: &'static str = "list_mcp_tools";
    const DESCRIPTION: &'static str = "List the MCP tools called on one side (`side`: client or server) in the \
        window [from, to), most calls first, then the tools a server listed but nobody called (`unused`): per \
        tool its server, calls, errors, error rate, calls the caller caused, latency p50, p90 and p99 in \
        microseconds, request and response bytes and the hash of its latest schema. `server` narrows to one \
        server; `source` and `include_caller_faults` work as in list_mcp_servers. At most 64 KiB per call; \
        a cut list says `truncated`. get_mcp_tool reads one tool over time.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        let checked = Common {
            side: &args.side,
            from: &args.from,
            to: &args.to,
            source: args.source.as_deref(),
            include_caller_faults: args.include_caller_faults,
            server: args.server,
        }
        .check(&mut check);
        check.finish()?;
        let query = checked.query(call)?;
        let found = call.service.mcp_tools(&call.ctx, &query).await;
        let found = found.map_err(|e| ToolError::query(&e, call.request_id))?;
        let mut s = Saturation::default();
        let rows: Vec<Row> = found
            .tools
            .iter()
            .map(|tool| Row {
                server_name: tool.server_name.to_string(),
                tool_name: tool.tool_name.to_string(),
                stats: Stats::of(&tool.totals, &mut s),
                request_bytes: s.u64(tool.totals.request_bytes).get(),
                response_bytes: s.u64(tool.totals.response_bytes).get(),
                schema_hash: tool.schema_hash.map(|h| format!("{h:016x}")),
                unused: tool.unused,
            })
            .collect();
        Ok(fit(
            rows,
            |_| {},
            |prefix, cut| Output {
                data: prefix.to_vec(),
                capture_sources: sources(&found.capture_sources),
                truncated: cut,
                meta: meta(&found.meta),
            },
        ))
    }
}
