//! The telemetry routes of milestone 1 (contract 12.6) and their OpenAPI operations, one module per route group.
//! Each group exposes its routes and the OpenAPI document of its paths, and is registered here with its `mod`
//! line, one line in [`router`] and one in [`document`]. Each handler checks its parameters before it asks for
//! the query service, so a request without a store configured is still authenticated and validated before it
//! is answered with `telemetry_store_unavailable`.

mod data_bounds;
mod facts;
mod filter_values;
mod metrics;
mod spans;
mod trace_detail;
mod traces;

use axum::Router;
use crucible_api_types::ProblemCode;
use crucible_cursor::CursorError;
use crucible_records::RequestId;
use utoipa::OpenApi as _;
use utoipa::openapi::OpenApi;

use crate::access::ApiState;
use crate::problem::ApiError;

/// The routes of every group, relative to the base path. The groups' paths are disjoint, so their order does
/// not matter; the list is kept sorted.
pub(crate) fn router() -> Router<ApiState> {
    Router::new()
        .merge(data_bounds::routes())
        .merge(facts::routes())
        .merge(filter_values::routes())
        .merge(metrics::routes())
        .merge(spans::routes())
        .merge(trace_detail::routes())
        .merge(traces::routes())
}

/// `doc` with the paths of every group, and the schemas they use. The document's maps are sorted by key, so the
/// order of the groups does not change it; the list is kept sorted.
pub(crate) fn document(mut doc: OpenApi) -> OpenApi {
    doc.merge(data_bounds::Paths::openapi());
    doc.merge(facts::Paths::openapi());
    doc.merge(filter_values::Paths::openapi());
    doc.merge(metrics::Paths::openapi());
    doc.merge(spans::Paths::openapi());
    doc.merge(trace_detail::Paths::openapi());
    doc.merge(traces::Paths::openapi());
    doc
}

/// The methods a path under the base path takes, for the `Allow` header of a wrong method.
pub(crate) fn allowed_methods(path: &str) -> &'static str {
    let post = facts::POST_PATHS
        .iter()
        .chain(&metrics::POST_PATHS)
        .any(|suffix| path.ends_with(suffix));
    if post { "POST" } else { "GET, HEAD" }
}

fn cursor_invalid(request_id: RequestId) -> impl FnOnce(CursorError) -> ApiError {
    move |error| {
        ApiError::new(
            request_id,
            ProblemCode::CursorInvalid,
            format!("{error}; restart from the first page"),
        )
    }
}
