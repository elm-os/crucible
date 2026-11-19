//! `list_mcp_errors`: the errors of one side (contract 9.9 `errors`): calls per error class and per tool from
//! the rollup, or with `breakdown` the failed calls grouped by message and JSON-RPC code.

use crucible_api_types::Saturation;
use crucible_plan::MCP_LIST_MAX_LIMIT;
use crucible_records::truncate_str;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::mcp::{Common, ServerSource, Stats, sources};
use crate::shape::{Meta, fit, meta};
use crate::tools::{Call, Check, Tool};

/// The most groups of the breakdown one call returns.
const MAX_GROUPS: u32 = 50;
/// The bytes of an error message kept when a group alone is too large for the result.
const LIGHT_MESSAGE: usize = 256;

pub(crate) struct ListMcpErrors;

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
    /// Group the failed calls by message and JSON-RPC code instead of by class; false by default.
    #[serde(default)]
    breakdown: Option<bool>,
    /// `auto` (default), `sdk`, `proxy` or `otlp`: which capture source each server is read from.
    #[serde(default)]
    source: Option<String>,
    /// Count calls the caller caused (invalid requests) in the measures; false by default.
    #[serde(default)]
    include_caller_faults: Option<bool>,
}

/// Calls of one error class.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct ClassCount {
    /// The error class: protocol, validation, not_found, tool_execution, auth, rate_limit, timeout, transport,
    /// cancelled or internal.
    error_class: &'static str,
    /// The calls.
    calls: u64,
}

/// Failed calls of one method and target in one class.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct TargetRow {
    /// The method.
    method: String,
    /// The tool or prompt name, or the resource template.
    target_name: String,
    /// The error class.
    error_class: &'static str,
    /// The calls.
    calls: u64,
}

/// Failed calls that share what they say.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct GroupRow {
    /// The server.
    server_name: String,
    /// The method.
    method: String,
    /// The tool or prompt name, or the resource template.
    target_name: String,
    /// The error class.
    error_class: &'static str,
    /// The subclass an SDK annotated: none, auth, rate_limit, upstream or business.
    error_subclass: &'static str,
    /// The JSON-RPC error code; null when there was none.
    jsonrpc_code: Option<i64>,
    /// The scrubbed error message.
    error_message: String,
    /// The calls.
    calls: u64,
    /// When the last of them started, RFC 3339.
    last_seen: String,
}

/// The errors of the side.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// Every call of the read; null with `breakdown`.
    stats: Option<Stats>,
    /// Calls per error class, most first; empty with `breakdown`.
    classes: Vec<ClassCount>,
    /// Failed calls per method, target and class, most first; empty with `breakdown`.
    targets: Vec<TargetRow>,
    /// With `breakdown`: the groups, most calls first.
    breakdown: Vec<GroupRow>,
    /// True when rows were left out, at the group cap or to keep the result under 64 KiB.
    truncated: bool,
    /// The capture source each server was read from.
    capture_sources: Vec<ServerSource>,
    /// How complete and how fresh the answer is.
    meta: Meta,
}

impl Tool for ListMcpErrors {
    const NAME: &'static str = "list_mcp_errors";
    const DESCRIPTION: &'static str = "List the errors of MCP calls on one side (`side`: client or server) in the \
        window [from, to): calls per error class and failed calls per method, tool and class; or, with \
        `breakdown: true`, the failed calls grouped by server, method, tool, class, subclass, JSON-RPC code and \
        message, most first, at most 50 groups. `server` narrows to one server; `source` and \
        `include_caller_faults` work as in list_mcp_servers. At most 64 KiB per call.";
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
        let mut s = Saturation::default();
        if args.breakdown.unwrap_or(false) {
            let limit = MAX_GROUPS.min(MCP_LIST_MAX_LIMIT);
            let found = call.service.mcp_error_breakdown(&call.ctx, &query, limit).await;
            let found = found.map_err(|e| ToolError::query(&e, call.request_id))?;
            let rows: Vec<GroupRow> = found
                .groups
                .iter()
                .map(|g| GroupRow {
                    server_name: g.server_name.to_string(),
                    method: g.method.to_string(),
                    target_name: g.target_name.to_string(),
                    error_class: g.error_class.as_str(),
                    error_subclass: g.error_subclass.as_str(),
                    jsonrpc_code: g.jsonrpc_code,
                    error_message: g.error_message.to_string(),
                    calls: s.u64(g.calls).get(),
                    last_seen: g.last_seen.to_string(),
                })
                .collect();
            return Ok(fit(
                rows,
                |row| row.error_message = truncate_str(&row.error_message, LIGHT_MESSAGE).to_owned(),
                |prefix, cut| Output {
                    stats: None,
                    classes: Vec::new(),
                    targets: Vec::new(),
                    breakdown: prefix.to_vec(),
                    truncated: cut || found.truncated,
                    capture_sources: sources(&found.capture_sources),
                    meta: meta(&found.meta),
                },
            ));
        }
        let found = call.service.mcp_error_classes(&call.ctx, &query).await;
        let found = found.map_err(|e| ToolError::query(&e, call.request_id))?;
        let stats = Stats::of(&found.totals, &mut s);
        let classes: Vec<ClassCount> = found
            .classes
            .iter()
            .map(|(class, calls)| ClassCount {
                error_class: class.as_str(),
                calls: s.u64(*calls).get(),
            })
            .collect();
        let rows: Vec<TargetRow> = found
            .targets
            .iter()
            .map(|t| TargetRow {
                method: t.method.to_string(),
                target_name: t.target_name.to_string(),
                error_class: t.error_class.as_str(),
                calls: s.u64(t.calls).get(),
            })
            .collect();
        Ok(fit(
            rows,
            |_| {},
            |prefix, cut| Output {
                stats: Some(stats.clone()),
                classes: classes.clone(),
                targets: prefix.to_vec(),
                breakdown: Vec::new(),
                truncated: cut,
                capture_sources: sources(&found.capture_sources),
                meta: meta(&found.meta),
            },
        ))
    }
}
