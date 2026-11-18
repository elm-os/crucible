//! `GET projects/{project_id}/data-bounds`: where the project's data begins and ends (contract 8.7, 8.8 and
//! 12.6).

use axum::extract::State;
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{DataBounds, Problem};
use crucible_query as query;
use utoipa::OpenApi;

use crate::access::{ApiState, Project};
use crate::convert::meta;
use crate::params::Params;
use crate::problem::ApiError;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route("/projects/{project_id}/data-bounds", get(get_data_bounds))
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(get_data_bounds))]
pub(super) struct Paths;

/// Get data bounds
///
/// Where the project's data begins and ends inside its retention, with the retention horizon, the widest
/// window a request may ask for and the server's clock.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/data-bounds",
    operation_id = "get_data_bounds",
    tag = "traces",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    responses(
        (status = 200, description = "The data bounds.", body = DataBounds),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn get_data_bounds(
    State(state): State<ApiState>,
    project: Project,
    uri: Uri,
) -> Result<Json<DataBounds>, ApiError> {
    let rid = project.request_id;
    Params::new(rid, &uri)?.finish()?;
    let service = state.service(rid)?;
    let bounds = service.data_bounds(&project.ctx).await;
    let bounds = bounds.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(data_bounds(&bounds, project.settings.max_window_days)))
}

fn range(range: Option<query::MinuteRange>) -> wire::TimeRange {
    wire::TimeRange {
        first: range.map(|r| r.first.to_string()),
        last: range.map(|r| r.last.to_string()),
    }
}

fn data_bounds(bounds: &query::DataBounds, max_window_days: u32) -> wire::DataBounds {
    wire::DataBounds {
        spans: range(bounds.spans),
        mcp: wire::McpBounds {
            client: range(bounds.mcp_client),
            server: range(bounds.mcp_server),
        },
        retention_horizon: bounds.retention_horizon.map(|t| t.to_string()),
        max_window_days,
        server_time: bounds.meta.as_of.to_string(),
        meta: meta(&bounds.meta),
    }
}
