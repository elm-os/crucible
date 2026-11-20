//! `compare_mcp_deploys`: one server's tools in two deploys (contract 9.9 `compare`): calls, errors and latency
//! per tool on each side, and the tools added, removed or listed with a changed schema.

use crucible_api_types::Saturation;
use crucible_query::{CompareSide, McpCompareQuery};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::mcp::{Common, ServerSource, Stats, sources};
use crate::shape::{Meta, fit, meta};
use crate::tools::{Call, Check, Tool};

pub(crate) struct CompareMcpDeploys;

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The server whose deploys are compared.
    server: String,
    /// The deploy compared against, as the server's deploys list it.
    baseline_deploy: String,
    /// The deploy compared.
    candidate_deploy: String,
    /// `client` or `server`: the side to read. The two sides are never summed.
    side: String,
    /// The start of a window that holds both deploys, RFC 3339 (inclusive). Required.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// `auto` (default), `sdk`, `proxy` or `otlp`: which capture source the server is read from.
    #[serde(default)]
    source: Option<String>,
    /// Count calls the caller caused (invalid requests) in the measures; false by default.
    #[serde(default)]
    include_caller_faults: Option<bool>,
}

/// One tool on both sides.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct Row {
    /// The tool.
    tool_name: String,
    /// Its calls in the baseline deploy.
    baseline: Stats,
    /// Its calls in the candidate deploy.
    candidate: Stats,
}

/// The compare.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The server.
    server_name: String,
    /// Each tool called in either deploy, by name.
    tools: Vec<Row>,
    /// Tools the candidate deploy listed and the baseline did not.
    added: Vec<String>,
    /// Tools the baseline deploy listed and the candidate did not.
    removed: Vec<String>,
    /// Tools both listed with a different schema hash.
    changed: Vec<String>,
    /// The capture source the server was read from.
    capture_sources: Vec<ServerSource>,
    /// True when tools were left out to keep the result under 64 KiB.
    truncated: bool,
    /// How complete and how fresh the answer is.
    meta: Meta,
}

impl Tool for CompareMcpDeploys {
    const NAME: &'static str = "compare_mcp_deploys";
    const DESCRIPTION: &'static str = "Compare one MCP server's tools between two of its deploys \
        (`baseline_deploy`, `candidate_deploy`) seen in the window [from, to) on one side (`side`: client or \
        server): per tool the calls, errors, error rate and latency p50, p90 and p99 in microseconds of each \
        deploy, and the tools added, removed or listed with a changed schema. The deploys of a server are in \
        get_mcp_tool's `deploys` and the REST deploys list. `source` and `include_caller_faults` work as in \
        list_mcp_servers. At most 64 KiB per call.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        if args.server.is_empty() {
            check.error(
                "server",
                "required",
                "give the server whose deploys to compare",
            );
        }
        if args.baseline_deploy == args.candidate_deploy {
            let message =
                "give two different deploys; an empty id is the deploy of unknown version";
            check.error("candidate_deploy", "invalid", message);
        }
        let checked = Common {
            side: &args.side,
            from: &args.from,
            to: &args.to,
            source: args.source.as_deref(),
            include_caller_faults: args.include_caller_faults,
            server: Some(args.server.clone()),
        }
        .check(&mut check);
        check.finish()?;
        let compare = McpCompareQuery {
            query: checked.query(call)?,
            baseline: CompareSide::Deploy(args.baseline_deploy),
            candidate: CompareSide::Deploy(args.candidate_deploy),
        };
        let found = call.service.mcp_compare(&call.ctx, &compare).await;
        let found = found.map_err(|e| ToolError::query(&e, call.request_id))?;
        let mut s = Saturation::default();
        let rows: Vec<Row> = found
            .tools
            .iter()
            .map(|t| Row {
                tool_name: t.tool_name.to_string(),
                baseline: Stats::of(&t.baseline, &mut s),
                candidate: Stats::of(&t.candidate, &mut s),
            })
            .collect();
        let names = |list: &[std::sync::Arc<str>]| -> Vec<String> {
            list.iter().map(ToString::to_string).collect()
        };
        Ok(fit(
            rows,
            |_| {},
            |prefix, cut| Output {
                server_name: args.server.clone(),
                tools: prefix.to_vec(),
                added: names(&found.added),
                removed: names(&found.removed),
                changed: names(&found.changed),
                capture_sources: sources(&found.capture_sources),
                truncated: cut,
                meta: meta(&found.meta),
            },
        ))
    }
}
