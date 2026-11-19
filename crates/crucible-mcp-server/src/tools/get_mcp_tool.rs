//! `get_mcp_tool`: one tool of one side over time (contract 9.9 `tools/{name}`): calls, errors and latency per
//! bucket, error classes, top clients and the deploys of its servers.

use crucible_api_types::Saturation;
use crucible_plan::Step;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::mcp::{Common, Latency, ServerSource, Stats, sources};
use crate::shape::{Meta, fit, meta};
use crate::tools::{Call, Check, Tool};

pub(crate) struct GetMcpTool;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The tool's name, as list_mcp_tools gives it.
    name: String,
    /// `client` or `server`: the side to read. The two sides are never summed.
    side: String,
    /// The start of the window, RFC 3339 (inclusive). Required.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// Only this server, when several servers have a tool of this name.
    #[serde(default)]
    server: Option<String>,
    /// `auto` (default), `sdk`, `proxy` or `otlp`: which capture source each server is read from.
    #[serde(default)]
    source: Option<String>,
    /// Count calls the caller caused (invalid requests) in the measures; false by default.
    #[serde(default)]
    include_caller_faults: Option<bool>,
}

/// One bucket with calls.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct Point {
    /// The bucket's first instant, RFC 3339.
    bucket: String,
    /// Calls.
    calls: u64,
    /// Failed calls.
    errors: u64,
    /// Latency.
    latency: Latency,
}

/// Calls of one error class.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct ClassCount {
    /// The error class.
    error_class: &'static str,
    /// The calls.
    calls: u64,
}

/// One client, by self-reported name.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct ClientRow {
    /// The client's name as it reported it.
    client_name: String,
    /// Its calls.
    stats: Stats,
}

/// One deploy of a server of the tool.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct DeployRow {
    /// The server.
    server_name: String,
    /// The deploy id; empty when unknown.
    deploy_id: String,
    /// The server version.
    server_version: String,
    /// First seen, RFC 3339.
    first_seen: String,
}

/// The tool over the window.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The tool.
    tool_name: String,
    /// Its calls over the window.
    stats: Stats,
    /// The bucket width: minute, five_minutes, fifteen_minutes, hour, day or week.
    granularity: &'static str,
    /// The buckets with calls, oldest first.
    series: Vec<Point>,
    /// Calls per error class, most first.
    error_classes: Vec<ClassCount>,
    /// The clients with the most calls.
    clients: Vec<ClientRow>,
    /// The deploys of the tool's servers seen in the window, oldest first.
    deploys: Vec<DeployRow>,
    /// The capture source each server was read from.
    capture_sources: Vec<ServerSource>,
    /// True when buckets were left out to keep the result under 64 KiB; narrow the window.
    truncated: bool,
    /// How complete and how fresh the answer is.
    meta: Meta,
}

const fn granularity(step: Step) -> &'static str {
    match step {
        Step::Minute => "minute",
        Step::FiveMinutes => "five_minutes",
        Step::FifteenMinutes => "fifteen_minutes",
        Step::Hour => "hour",
        Step::Day => "day",
        Step::Week => "week",
    }
}

impl Tool for GetMcpTool {
    const NAME: &'static str = "get_mcp_tool";
    const DESCRIPTION: &'static str = "Read one MCP tool (`name`) on one side (`side`: client or server) over the \
        window [from, to): its calls, errors, error rate and latency p50, p90 and p99 in microseconds over the \
        window and per time bucket (the finest width under 500 buckets; empty buckets are left out), calls per \
        error class, the clients with the most calls, and the deploys of its servers, which mark where its \
        latency may change. `server` picks the server when several have a tool of that name; `source` and \
        `include_caller_faults` work as in list_mcp_servers. At most 64 KiB per call; a cut series says \
        `truncated`.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        if args.name.is_empty() {
            check.error("name", "required", "give the tool's name, as list_mcp_tools lists it");
        }
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
        let found = call.service.mcp_tool(&call.ctx, &query, &args.name).await;
        let found = found.map_err(|e| ToolError::query(&e, call.request_id))?;
        let mut s = Saturation::default();
        let points: Vec<Point> = found
            .series
            .iter()
            .filter(|p| p.totals.calls > 0 || p.totals.caller_faults > 0)
            .map(|p| Point {
                bucket: p.bucket.to_string(),
                calls: s.u64(p.totals.calls).get(),
                errors: s.u64(p.totals.errors).get(),
                latency: Latency::of(&p.totals.latency, &mut s),
            })
            .collect();
        let stats = Stats::of(&found.totals, &mut s);
        let error_classes: Vec<ClassCount> = found
            .error_classes
            .iter()
            .map(|(class, calls)| ClassCount {
                error_class: class.as_str(),
                calls: s.u64(*calls).get(),
            })
            .collect();
        let clients: Vec<ClientRow> = found
            .clients
            .iter()
            .map(|c| ClientRow {
                client_name: c.client_name.to_string(),
                stats: Stats::of(&c.totals, &mut s),
            })
            .collect();
        let deploys: Vec<DeployRow> = found
            .deploys
            .iter()
            .map(|d| DeployRow {
                server_name: d.server_name.to_string(),
                deploy_id: d.deploy_id.to_string(),
                server_version: d.server_version.to_string(),
                first_seen: d.first_seen.to_string(),
            })
            .collect();
        Ok(fit(
            points,
            |_| {},
            |prefix, cut| Output {
                tool_name: found.tool_name.to_string(),
                stats: stats.clone(),
                granularity: granularity(found.step),
                series: prefix.to_vec(),
                error_classes: error_classes.clone(),
                clients: clients.clone(),
                deploys: deploys.clone(),
                capture_sources: sources(&found.capture_sources),
                truncated: cut,
                meta: meta(&found.meta),
            },
        ))
    }
}
