//! The MCP analytics under `projects/{project_id}/mcp/` (contract 9.9 and 12.6): servers, tools, one tool,
//! errors, clients, users, sessions, operations, one operation, deploys and compare.
//!
//! Every route takes `side` (`client` or `server`), `from` and `to`, and accepts `source` (`auto`, `sdk`,
//! `proxy` or `otlp`), `include_caller_faults` and `server`. Each checks every parameter before it asks for the
//! query service, and every refusal names its parameter (contract 12.3).

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{
    McpCompareParams, McpListParams, McpOperationsParams, McpParams, McpViewParams, Problem,
    Saturation,
};
use crucible_plan::{CaptureClass, MCP_LIST_MAX_LIMIT, McpOpsFilter, ServerSources, SourceChoice};
use crucible_query::{
    CompareSide, McpCompareQuery, McpOperationRow, McpOperationsQuery, McpQuery, McpTotals,
};
use crucible_records::{ErrorClass, McpSide, SpanId, TimeWindow};
use utoipa::OpenApi;

use crate::access::{ApiState, Project};
use crate::convert::meta;
use crate::params::{Params, check_window};
use crate::problem::ApiError;
use crate::routes::cursor_invalid;
use crate::routes::metrics::step_name;

/// The default number of rows of the MCP lists.
const DEFAULT_LIMIT: u32 = 50;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    let at = |rest: &str| format!("/projects/{{project_id}}/mcp/{rest}");
    Router::new()
        .route(&at("clients"), get(get_mcp_clients))
        .route(&at("compare"), get(get_mcp_compare))
        .route(&at("deploys"), get(list_mcp_deploys))
        .route(&at("errors"), get(get_mcp_errors))
        .route(&at("operations"), get(list_mcp_operations))
        .route(&at("operations/{trace_id}/{span_id}"), get(get_mcp_operation))
        .route(&at("servers"), get(list_mcp_servers))
        .route(&at("sessions"), get(list_mcp_sessions))
        .route(&at("tools"), get(list_mcp_tools))
        .route(&at("tools/{name}"), get(get_mcp_tool))
        .route(&at("users"), get(list_mcp_users))
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    get_mcp_clients,
    get_mcp_compare,
    get_mcp_errors,
    get_mcp_operation,
    get_mcp_tool,
    list_mcp_deploys,
    list_mcp_operations,
    list_mcp_servers,
    list_mcp_sessions,
    list_mcp_tools,
    list_mcp_users,
))]
pub(super) struct Paths;

/// The parameters every route takes, taken from the query string; their problems are recorded in `params`.
struct Common {
    window: Option<TimeWindow>,
    side: Option<McpSide>,
    source: Option<SourceChoice>,
    include_caller_faults: bool,
    server: Option<String>,
}

impl Common {
    fn take(params: &mut Params) -> Self {
        let window = params.window();
        let side = match params.required("side").as_deref() {
            Some("client") => Some(McpSide::Client),
            Some("server") => Some(McpSide::Server),
            Some(_) => {
                params.error("side", "invalid", "expected client or server");
                None
            }
            None => None,
        };
        let source = match params.optional("source") {
            None => Some(SourceChoice::Auto),
            Some(text) => {
                let source = SourceChoice::parse(&text);
                if source.is_none() {
                    params.error("source", "invalid", "expected auto, sdk, proxy or otlp");
                }
                source
            }
        };
        let include_caller_faults = boolean(params, "include_caller_faults");
        let server = params.optional("server");
        Self {
            window,
            side,
            source,
            include_caller_faults,
            server,
        }
    }

    /// The query, once `params` finished without a problem; the window is checked against the project's
    /// maximum.
    fn query(self, project: &Project) -> Result<McpQuery, ApiError> {
        let rid = project.request_id;
        let (Some(window), Some(side), Some(source)) = (self.window, self.side, self.source) else {
            return Err(ApiError::validation(rid, Vec::new()));
        };
        check_window(rid, window, project.settings.max_window_days)?;
        Ok(McpQuery {
            window,
            side,
            source,
            include_caller_faults: self.include_caller_faults,
            server: self.server,
        })
    }
}

/// An optional `true` or `false` parameter, false when absent.
fn boolean(params: &mut Params, name: &str) -> bool {
    match params.optional(name).as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => {
            params.error(name, "invalid", "expected true or false");
            false
        }
    }
}

const fn class(class: CaptureClass) -> wire::CaptureClass {
    match class {
        CaptureClass::Sdk => wire::CaptureClass::Sdk,
        CaptureClass::Proxy => wire::CaptureClass::Proxy,
        CaptureClass::Otlp => wire::CaptureClass::Otlp,
    }
}

fn sources(servers: &[ServerSources]) -> Vec<wire::McpServerSource> {
    servers
        .iter()
        .map(|s| wire::McpServerSource {
            server_name: s.server_name.clone(),
            available: s.available.iter().copied().map(class).collect(),
            chosen: s.chosen.map(class),
        })
        .collect()
}

fn latency(latency: &crucible_query::Latency, s: &mut Saturation) -> wire::McpLatency {
    wire::McpLatency {
        p50_us: latency.p50.map(|v| s.u64(v)),
        p90_us: latency.p90.map(|v| s.u64(v)),
        p99_us: latency.p99.map(|v| s.u64(v)),
    }
}

fn rate(errors: u64, calls: u64) -> Option<f64> {
    (calls > 0).then(|| errors as f64 / calls as f64)
}

fn stats(totals: &McpTotals, s: &mut Saturation) -> wire::McpStats {
    wire::McpStats {
        calls: s.u64(totals.calls),
        errors: s.u64(totals.errors),
        error_rate: rate(totals.errors, totals.calls),
        caller_faults: s.u64(totals.caller_faults),
        latency: latency(&totals.latency, s),
        last_seen: totals.last_seen.map(|t| t.to_string()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn deploy(d: &crucible_query::McpDeploy) -> wire::McpDeployRow {
    wire::McpDeployRow {
        server_name: d.server_name.to_string(),
        deploy_id: d.deploy_id.to_string(),
        server_version: d.server_version.to_string(),
        capture_source: d.capture_source.into(),
        first_seen: d.first_seen.to_string(),
        last_seen: d.last_seen.to_string(),
    }
}

fn client(c: &crucible_query::McpClient, s: &mut Saturation) -> wire::McpClientRow {
    wire::McpClientRow {
        client_name: c.client_name.to_string(),
        stats: stats(&c.totals, s),
    }
}

fn class_counts(classes: &[(ErrorClass, u64)], s: &mut Saturation) -> Vec<wire::McpClassCount> {
    classes
        .iter()
        .map(|(error_class, calls)| wire::McpClassCount {
            error_class: (*error_class).into(),
            calls: s.u64(*calls),
        })
        .collect()
}

fn code(code: Option<i64>) -> Option<i32> {
    code.and_then(|c| i32::try_from(c).ok())
}

fn operation(op: &McpOperationRow) -> wire::McpOperationRow {
    let mut s = Saturation::default();
    wire::McpOperationRow {
        trace_id: op.trace_id.to_string(),
        span_id: op.span_id.to_string(),
        parent_span_id: (op.parent_span_id != SpanId::ZERO).then(|| op.parent_span_id.to_string()),
        start_time: op.start_time.to_string(),
        duration_us: s.u64(op.duration_us),
        server_name: op.server_name.to_string(),
        server_version: op.server_version.to_string(),
        deploy_id: op.deploy_id.to_string(),
        capture_source: op.capture_source.into(),
        era: op.era.into(),
        transport: op.transport.into(),
        method: op.method.to_string(),
        target_kind: op.target_kind.into(),
        target_name: op.target_name.to_string(),
        status: op.status.into(),
        error_class: op.error_class.into(),
        error_subclass: op.error_subclass.into(),
        caller_fault: op.caller_fault,
        jsonrpc_code: code(op.jsonrpc_code),
        error_type: op.error_type.to_string(),
        error_message: op.error_message.to_string(),
        client_name: op.client_name.to_string(),
        client_version: op.client_version.to_string(),
        user_id: op.user_id.to_string(),
        user_subject_hash: op.user_subject_hash.map(|h| hex(&h)),
        identity_source: op.identity_source.into(),
        group_key: op.group_key.to_string(),
        group_key_kind: op.group_key_kind.into(),
        request_bytes: s.u64(op.request_bytes),
        response_bytes: s.u64(op.response_bytes),
        content_captured: op.content_captured,
        result_type: op.result_type.into(),
        saturated: s.saturated(),
    }
}

/// The page size of an MCP list, default [`DEFAULT_LIMIT`].
fn limit(params: &mut Params) -> u32 {
    params.limit(MCP_LIST_MAX_LIMIT).unwrap_or(DEFAULT_LIMIT)
}

/// Whether the request asks for the breakdown section.
fn breakdown(params: &mut Params) -> bool {
    match params.optional("view").as_deref() {
        None | Some("summary") => false,
        Some("breakdown") => true,
        Some(_) => {
            params.error("view", "invalid", "expected summary or breakdown");
            false
        }
    }
}

const fn view(breakdown: bool) -> wire::McpView {
    if breakdown {
        wire::McpView::Breakdown
    } else {
        wire::McpView::Summary
    }
}

/// List MCP servers
///
/// The servers of one side: calls, errors, caller-fault calls, latency percentiles, distinct clients and last
/// seen from the tool rollup, and the version of each server's latest deploy.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/servers",
    operation_id = "list_mcp_servers",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams),
    responses(
        (status = 200, description = "The servers.", body = wire::McpServerList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_mcp_servers(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpServerList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let found = service.mcp_servers(&project.ctx, &query).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    let data = found
        .servers
        .iter()
        .map(|server| wire::McpServerRow {
            server_name: server.server_name.to_string(),
            stats: stats(&server.totals, &mut s),
            clients: s.u64(server.clients),
            version: server.version.as_ref().map(ToString::to_string),
            deploys: s.u64(server.deploys),
        })
        .collect();
    Ok(Json(wire::McpServerList {
        data,
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}

/// List MCP tools
///
/// The tools of one side: the calls of `tools/call` per server and tool from the tool rollup, each tool's
/// latest schema hash, and the tools listed in the inventory without a call in the window (`unused`).
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/tools",
    operation_id = "list_mcp_tools",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams),
    responses(
        (status = 200, description = "The tools.", body = wire::McpToolList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_mcp_tools(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpToolList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let found = service.mcp_tools(&project.ctx, &query).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    let data = found
        .tools
        .iter()
        .map(|tool| wire::McpToolRow {
            server_name: tool.server_name.to_string(),
            tool_name: tool.tool_name.to_string(),
            stats: stats(&tool.totals, &mut s),
            request_bytes: s.u64(tool.totals.request_bytes),
            response_bytes: s.u64(tool.totals.response_bytes),
            schema_hash: tool.schema_hash.map(|h| format!("{h:016x}")),
            unused: tool.unused,
        })
        .collect();
    Ok(Json(wire::McpToolList {
        data,
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}

/// Get an MCP tool
///
/// One tool of one side: its calls, errors and latency percentiles over the window and per bucket, its error
/// classes and top clients, and the deploys of its servers as markers for the series.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/tools/{name}",
    operation_id = "get_mcp_tool",
    tag = "mcp",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("name" = String, Path, description = "The tool's name."),
        McpParams,
    ),
    responses(
        (status = 200, description = "The tool.", body = wire::McpToolDetail),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_mcp_tool(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpToolDetail>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    let name = project.path.get("name").cloned().unwrap_or_default();
    if name.is_empty() {
        params.error("name", "required", "the tool's name is required");
    }
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let found = service.mcp_tool(&project.ctx, &query, &name).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    let series = found
        .series
        .iter()
        .map(|point| wire::McpSeriesPoint {
            bucket: point.bucket.to_string(),
            calls: s.u64(point.totals.calls),
            errors: s.u64(point.totals.errors),
            latency: latency(&point.totals.latency, &mut s),
        })
        .collect();
    Ok(Json(wire::McpToolDetail {
        tool_name: found.tool_name.to_string(),
        stats: stats(&found.totals, &mut s),
        request_bytes: s.u64(found.totals.request_bytes),
        response_bytes: s.u64(found.totals.response_bytes),
        granularity: step_name(found.step),
        series,
        error_classes: class_counts(&found.error_classes, &mut s),
        clients: found.clients.iter().map(|c| client(c, &mut s)).collect(),
        deploys: found.deploys.iter().map(deploy).collect(),
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}

/// Get MCP errors
///
/// With `view=summary` (the default), the error classes of one side from the tool rollup: calls per class and
/// per method, target and class. With `view=breakdown`, failed calls grouped by server, method, target, class,
/// subclass, JSON-RPC code and message, from the operations.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/errors",
    operation_id = "get_mcp_errors",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams, McpViewParams),
    responses(
        (status = 200, description = "The errors.", body = wire::McpErrors),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_mcp_errors(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpErrors>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    let (breakdown, limit) = (breakdown(&mut params), limit(&mut params));
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let mut s = Saturation::default();
    if breakdown {
        let found = service.mcp_error_breakdown(&project.ctx, &query, limit).await;
        let found = found.map_err(|e| ApiError::query(rid, &e))?;
        let groups = found
            .groups
            .iter()
            .map(|g| wire::McpErrorGroupRow {
                server_name: g.server_name.to_string(),
                method: g.method.to_string(),
                target_name: g.target_name.to_string(),
                error_class: g.error_class.into(),
                error_subclass: g.error_subclass.into(),
                jsonrpc_code: code(g.jsonrpc_code),
                error_message: g.error_message.to_string(),
                calls: s.u64(g.calls),
                last_seen: g.last_seen.to_string(),
            })
            .collect();
        return Ok(Json(wire::McpErrors {
            view: view(true),
            stats: None,
            classes: Vec::new(),
            targets: Vec::new(),
            breakdown: groups,
            truncated: found.truncated,
            capture_sources: sources(&found.capture_sources),
            saturated: s.saturated(),
            meta: meta(&found.meta),
        }));
    }
    let found = service.mcp_error_classes(&project.ctx, &query).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let targets = found
        .targets
        .iter()
        .map(|t| wire::McpTargetErrorRow {
            method: t.method.to_string(),
            target_name: t.target_name.to_string(),
            error_class: t.error_class.into(),
            calls: s.u64(t.calls),
        })
        .collect();
    Ok(Json(wire::McpErrors {
        view: view(false),
        stats: Some(stats(&found.totals, &mut s)),
        classes: class_counts(&found.classes, &mut s),
        targets,
        breakdown: Vec::new(),
        truncated: false,
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}

/// Get MCP clients
///
/// With `view=summary` (the default), the clients of one side by name from the tool rollup. With
/// `view=breakdown`, the calls per client name and version from the operations. Client names and versions are
/// what the clients reported about themselves.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/clients",
    operation_id = "get_mcp_clients",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams, McpViewParams),
    responses(
        (status = 200, description = "The clients.", body = wire::McpClientList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_mcp_clients(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpClientList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    let (breakdown, limit) = (breakdown(&mut params), limit(&mut params));
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let mut s = Saturation::default();
    if breakdown {
        let found = service.mcp_client_versions(&project.ctx, &query, limit).await;
        let found = found.map_err(|e| ApiError::query(rid, &e))?;
        let versions = found
            .versions
            .iter()
            .map(|v| wire::McpClientVersionRow {
                client_name: v.client_name.to_string(),
                client_version: v.client_version.to_string(),
                calls: s.u64(v.calls),
                last_seen: v.last_seen.to_string(),
            })
            .collect();
        return Ok(Json(wire::McpClientList {
            view: view(true),
            self_reported: true,
            data: Vec::new(),
            versions,
            truncated: found.truncated,
            capture_sources: sources(&found.capture_sources),
            saturated: s.saturated(),
            meta: meta(&found.meta),
        }));
    }
    let found = service.mcp_clients(&project.ctx, &query).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(wire::McpClientList {
        view: view(false),
        self_reported: true,
        data: found.clients.iter().map(|c| client(c, &mut s)).collect(),
        versions: Vec::new(),
        truncated: false,
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}

/// List MCP users
///
/// The users of one side, last seen first: calls, failed calls, distinct sessions and first and last seen per
/// user id (from the SDK's identify hook) or hashed token subject, from the operations.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/users",
    operation_id = "list_mcp_users",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams, McpListParams),
    responses(
        (status = 200, description = "The users.", body = wire::McpUserList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_mcp_users(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpUserList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    let limit = limit(&mut params);
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let found = service.mcp_users(&project.ctx, &query, limit).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    let data = found
        .users
        .iter()
        .map(|u| wire::McpUserRow {
            user_id: u.key.user_id.clone(),
            subject_hash: (u.key.subject_hash != [0; 16]).then(|| hex(&u.key.subject_hash)),
            identity_source: u.identity_source.into(),
            calls: s.u64(u.calls),
            errors: s.u64(u.errors),
            sessions: s.u64(u.sessions),
            first_seen: u.first_seen.to_string(),
            last_seen: u.last_seen.to_string(),
        })
        .collect();
    Ok(Json(wire::McpUserList {
        data,
        truncated: found.truncated,
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}

/// List MCP sessions
///
/// The MCP sessions of one side, last seen first: calls, failed calls and first and last seen per group key,
/// with what the group key is (contract 9.4), from the operations.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/sessions",
    operation_id = "list_mcp_sessions",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams, McpListParams),
    responses(
        (status = 200, description = "The sessions.", body = wire::McpSessionList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_mcp_sessions(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpSessionList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    let limit = limit(&mut params);
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let found = service.mcp_sessions(&project.ctx, &query, limit).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    let data = found
        .sessions
        .iter()
        .map(|session| wire::McpSessionRow {
            group_key: session.group_key.to_string(),
            group_key_kind: session.kind.into(),
            calls: s.u64(session.calls),
            errors: s.u64(session.errors),
            client_name: session.client_name.to_string(),
            first_seen: session.first_seen.to_string(),
            last_seen: session.last_seen.to_string(),
        })
        .collect();
    Ok(Json(wire::McpSessionList {
        data,
        truncated: found.truncated,
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}

/// List MCP operations
///
/// The operations of one side, newest first, narrowed by target, method, client, error class, session or
/// user. Later pages follow `next_cursor` with the same parameters.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/operations",
    operation_id = "list_mcp_operations",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams, McpOperationsParams),
    responses(
        (status = 200, description = "One page of operations.", body = wire::McpOperationPage),
        (status = 400, description = "`cursor_invalid`: restart from the first page.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_mcp_operations(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpOperationPage>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    let limit = limit(&mut params);
    let cursor = params.optional("cursor");
    let error_class = match params.optional("error_class") {
        None => None,
        Some(text) => {
            let class = ErrorClass::parse(&text);
            if class.is_none() {
                params.error("error_class", "invalid", "expected an error class name");
            }
            class
        }
    };
    let filter = McpOpsFilter {
        target: params.optional("target"),
        method: params.optional("method"),
        client: params.optional("client"),
        error_class,
        errors_only: boolean(&mut params, "errors_only"),
        session: params.optional("session"),
        user: params.optional("user"),
    };
    params.finish()?;
    let mut page = McpOperationsQuery {
        query: common.query(&project)?,
        filter,
        limit,
        cursor: None,
    };
    let cursors = state.cursors(&project);
    if let Some(text) = &cursor {
        let opened = cursors.open_mcp_operations(text, &page);
        page.cursor = Some(opened.map_err(cursor_invalid(rid))?);
    }
    let service = state.service(rid)?;
    let found = service.mcp_operations(&project.ctx, &page).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let next_cursor = found
        .next
        .map(|next| cursors.seal_mcp_operations(&page, next));
    Ok(Json(wire::McpOperationPage {
        data: found.operations.iter().map(operation).collect(),
        has_more: next_cursor.is_some(),
        next_cursor,
        capture_sources: sources(&found.capture_sources),
        meta: meta(&found.meta),
    }))
}

fn content(content: Option<&crucible_query::McpContent>) -> Option<wire::McpContentPreview> {
    content.map(|c| {
        let (bytes, _) = wire::SafeU64::saturating(c.bytes);
        wire::McpContentPreview {
            preview: c.preview.to_string(),
            bytes,
            truncated: c.bytes > crucible_query::MCP_CONTENT_PREVIEW,
        }
    })
}

/// Get an MCP operation
///
/// One operation: every field of its row, the first 16 KiB of its captured arguments and result, the other
/// side's row of the call in the same trace, and the multi-round exchange it belongs to.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/operations/{trace_id}/{span_id}",
    operation_id = "get_mcp_operation",
    tag = "mcp",
    params(
        ("project_id" = String, Path, description = "The project id (a UUID)."),
        ("trace_id" = String, Path, description = "The trace id: 32 hex characters."),
        ("span_id" = String, Path, description = "The span id: 16 hex characters."),
        McpParams,
    ),
    responses(
        (status = 200, description = "The operation.", body = wire::McpOperationDetail),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: no such operation in the window, or the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_mcp_operation(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpOperationDetail>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    let trace_id = params.trace_id(&project.path);
    let span_id = project
        .path
        .get("span_id")
        .and_then(|text| SpanId::parse(text).ok());
    if span_id.is_none() {
        params.error("span_id", "invalid_id", "expected 16 hex characters");
    }
    params.finish()?;
    let query = common.query(&project)?;
    let (Some(trace_id), Some(span_id)) = (trace_id, span_id) else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let service = state.service(rid)?;
    let found = service
        .mcp_operation(&project.ctx, &query, (trace_id, span_id))
        .await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let operation = found.operation.as_ref().map(|full| wire::McpOperation {
        operation: operation(&full.row),
        side: full.side.into(),
        instance_id: full.instance_id.to_string(),
        protocol_version: full.protocol_version.to_string(),
        jsonrpc_id: full.jsonrpc_id.to_string(),
        http_status: full.http_status.and_then(|s| u16::try_from(s).ok()),
        oauth_client_id: full.oauth_client_id.to_string(),
        tenant: full.tenant.to_string(),
        args: content(full.args.as_ref()),
        result: content(full.result.as_ref()),
        other_side: full.other_side.as_ref().map(operation),
        chain: full
            .chain
            .iter()
            .map(|link| wire::McpChainLink {
                span_id: link.span_id.to_string(),
                start_time: link.start_time.to_string(),
                round: link.round,
                method: link.method.to_string(),
                target_name: link.target_name.to_string(),
                result_type: link.result_type.into(),
            })
            .collect(),
    });
    Ok(Json(wire::McpOperationDetail {
        operation,
        meta: meta(&found.meta),
    }))
}

/// List MCP deploys
///
/// The deploys seen in the window (contract 9.7), newest first. Deploys are recorded from server-side
/// operations, so each server's capture source is chosen among its deploys' own.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/deploys",
    operation_id = "list_mcp_deploys",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams),
    responses(
        (status = 200, description = "The deploys.", body = wire::McpDeployList),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn list_mcp_deploys(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpDeployList>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    params.finish()?;
    let query = common.query(&project)?;
    let service = state.service(rid)?;
    let found = service.mcp_deploys(&project.ctx, &query).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(wire::McpDeployList {
        data: found.deploys.iter().map(deploy).collect(),
        capture_sources: sources(&found.capture_sources),
        meta: meta(&found.meta),
    }))
}

/// Compare MCP deploys
///
/// One server's tools on two sides: two deploys inside the window (`baseline_deploy` and `candidate_deploy`),
/// or a baseline window against the window (`baseline_from` and `baseline_to`). Per tool, the calls, errors
/// and latency percentiles of each side, and the tools added, removed or listed with a changed schema.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/mcp/compare",
    operation_id = "get_mcp_compare",
    tag = "mcp",
    params(("project_id" = String, Path, description = "The project id (a UUID)."), McpParams, McpCompareParams),
    responses(
        (status = 200, description = "The compare.", body = wire::McpCompare),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the caller cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_mcp_compare(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<wire::McpCompare>, ApiError> {
    let rid = project.request_id;
    let mut params = Params::new(rid, &uri)?;
    let common = Common::take(&mut params);
    if common.server.is_none() {
        params.error("server", "required", "a compare reads one server; name it");
    }
    let deploys = (
        params.optional("baseline_deploy"),
        params.optional("candidate_deploy"),
    );
    let windows = (
        params.optional_time("baseline_from"),
        params.optional_time("baseline_to"),
    );
    // Two deploys, or a baseline window whose candidate is the request's window.
    let baseline = match (deploys, windows) {
        ((Some(baseline), Some(candidate)), (None, None)) => {
            Some((CompareSide::Deploy(baseline), Some(candidate)))
        }
        ((None, None), (Some(from), Some(to))) => match TimeWindow::new(from, to) {
            Ok(window) => Some((CompareSide::Window(window), None)),
            Err(_) => {
                let message = "`baseline_to` must be after `baseline_from`";
                params.error("baseline_to", "invalid_window", message);
                None
            }
        },
        _ => {
            params.error(
                "baseline_deploy",
                "required",
                "give baseline_deploy and candidate_deploy, or baseline_from and baseline_to",
            );
            None
        }
    };
    params.finish()?;
    let query = common.query(&project)?;
    let Some((baseline, candidate)) = baseline else {
        return Err(ApiError::validation(rid, Vec::new()));
    };
    let candidate = match candidate {
        Some(deploy) => CompareSide::Deploy(deploy),
        None => CompareSide::Window(query.window),
    };
    if let CompareSide::Window(window) = &baseline {
        check_window(rid, *window, project.settings.max_window_days)?;
    }
    let server_name = query.server.clone().unwrap_or_default();
    let compare = McpCompareQuery {
        query,
        baseline,
        candidate,
    };
    let service = state.service(rid)?;
    let found = service.mcp_compare(&project.ctx, &compare).await;
    let found = found.map_err(|e| ApiError::query(rid, &e))?;
    let mut s = Saturation::default();
    let tools = found
        .tools
        .iter()
        .map(|t| wire::McpToolDeltaRow {
            tool_name: t.tool_name.to_string(),
            baseline: stats(&t.baseline, &mut s),
            candidate: stats(&t.candidate, &mut s),
        })
        .collect();
    let names = |list: &[std::sync::Arc<str>]| list.iter().map(ToString::to_string).collect();
    Ok(Json(wire::McpCompare {
        server_name,
        tools,
        added: names(&found.added),
        removed: names(&found.removed),
        changed: names(&found.changed),
        capture_sources: sources(&found.capture_sources),
        saturated: s.saturated(),
        meta: meta(&found.meta),
    }))
}
