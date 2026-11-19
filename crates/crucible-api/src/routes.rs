//! The telemetry routes (contract 12.6) and their OpenAPI operations, one module per route group.
//! Each group exposes its routes and the OpenAPI document of its paths, and is registered here with its `mod`
//! line, one line in [`router`] and one in [`document`]. Each handler checks its parameters before it asks for
//! the query service, so a request without a store configured is still authenticated and validated before it
//! is answered with `telemetry_store_unavailable`.

mod auth;
mod data_bounds;
mod facts;
mod filter_values;
mod lifecycle;
mod metrics;
mod org;
mod payload;
mod projects;
mod search;
mod sessions;
mod span_list;
mod spans;
pub(crate) mod sql;
mod trace_detail;
mod traces;
mod users;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::header::ALLOW;
use axum::response::Response;
use crucible_api_types::ProblemCode;
use crucible_cursor::CursorError;
use crucible_records::RequestId;
use utoipa::OpenApi as _;
use utoipa::openapi::OpenApi;

use crate::access::ApiState;
use crate::problem::{ApiError, problem, request_id};

/// The routes of every group, relative to the base path. The groups' paths are disjoint, so their order does
/// not matter; the list is kept sorted.
pub(crate) fn router() -> Router<ApiState> {
    Router::new()
        .merge(auth::routes())
        .merge(data_bounds::routes())
        .merge(facts::routes())
        .merge(filter_values::routes())
        .merge(lifecycle::routes())
        .merge(metrics::routes())
        .merge(org::routes())
        .merge(payload::routes())
        .merge(projects::routes())
        .merge(search::routes())
        .merge(sessions::routes())
        .merge(span_list::routes())
        .merge(spans::routes())
        .merge(sql::routes())
        .merge(trace_detail::routes())
        .merge(traces::routes())
        .merge(users::routes())
}

/// `doc` with the paths of every group, and the schemas they use. The document's maps are sorted by key, so the
/// order of the groups does not change it; the list is kept sorted.
pub(crate) fn document(mut doc: OpenApi) -> OpenApi {
    doc.merge(auth::Paths::openapi());
    doc.merge(data_bounds::Paths::openapi());
    doc.merge(facts::Paths::openapi());
    doc.merge(filter_values::Paths::openapi());
    doc.merge(lifecycle::Paths::openapi());
    doc.merge(metrics::Paths::openapi());
    doc.merge(org::Paths::openapi());
    doc.merge(payload::Paths::openapi());
    doc.merge(projects::Paths::openapi());
    doc.merge(search::Paths::openapi());
    doc.merge(sessions::Paths::openapi());
    doc.merge(span_list::Paths::openapi());
    doc.merge(spans::Paths::openapi());
    doc.merge(sql::Paths::openapi());
    doc.merge(trace_detail::Paths::openapi());
    doc.merge(traces::Paths::openapi());
    doc.merge(users::Paths::openapi());
    doc
}

/// The answer of a POST route to any other method: the fact writes and the metrics query take POST only.
async fn post_only(State(state): State<ApiState>, request: Request) -> Response {
    let id = request_id(request.extensions(), &state.env);
    let mut response = problem(
        id,
        ProblemCode::MethodNotAllowed,
        "the resource takes POST with a JSON body",
    );
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("POST"));
    response
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
