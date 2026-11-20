//! `list_mcp_servers`: the MCP servers of one side (contract 9.9 `servers`), with calls, errors, latency,
//! clients and the latest deploy's version.

use crucible_api_types::Saturation;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::mcp::{Common, ServerSource, Stats, sources};
use crate::shape::{Meta, fit, meta};
use crate::tools::{Call, Check, Tool};

pub(crate) struct ListMcpServers;

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
    /// `auto` (default), `sdk`, `proxy` or `otlp`: which capture source each server is read from.
    #[serde(default)]
    source: Option<String>,
    /// Count calls the caller caused (invalid requests) in the measures; false by default.
    #[serde(default)]
    include_caller_faults: Option<bool>,
}

/// One server.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct Row {
    /// The server.
    server_name: String,
    /// Its calls.
    stats: Stats,
    /// Distinct self-reported client names that called it.
    clients: u64,
    /// The version of its latest deploy in the window; null when none was seen.
    version: Option<String>,
    /// Deploys seen in the window.
    deploys: u64,
}

/// The servers of the side, most calls first.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The servers.
    data: Vec<Row>,
    /// The capture source each server was read from.
    capture_sources: Vec<ServerSource>,
    /// True when servers were left out to keep the result under 64 KiB; narrow the window.
    truncated: bool,
    /// How complete and how fresh the answer is.
    meta: Meta,
}

impl Tool for ListMcpServers {
    const NAME: &'static str = "list_mcp_servers";
    const DESCRIPTION: &'static str = "List the MCP servers seen on one side (`side`: client or server) in the \
        window [from, to), most calls first: calls, errors, error rate, calls the caller caused, latency p50, \
        p90 and p99 in microseconds, distinct clients, last seen, and the version of the latest deploy. \
        `source` picks the capture source (auto, sdk, proxy or otlp; auto reads each server from its SDK rows, \
        else its proxy rows, else plain OTLP rows, and never adds them up); `capture_sources` says which was \
        read. Caller-fault calls are left out of the measures unless include_caller_faults is true. At most \
        64 KiB per call. Then list_mcp_tools, get_mcp_tool and list_mcp_errors drill in.";
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
            server: None,
        }
        .check(&mut check);
        check.finish()?;
        let query = checked.query(call)?;
        let found = call.service.mcp_servers(&call.ctx, &query).await;
        let found = found.map_err(|e| ToolError::query(&e, call.request_id))?;
        let mut s = Saturation::default();
        let rows: Vec<Row> = found
            .servers
            .iter()
            .map(|server| Row {
                server_name: server.server_name.to_string(),
                stats: Stats::of(&server.totals, &mut s),
                clients: s.u64(server.clients).get(),
                version: server.version.as_ref().map(ToString::to_string),
                deploys: s.u64(server.deploys).get(),
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
