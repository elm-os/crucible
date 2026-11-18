//! `POST projects/{project_id}/search`: full-text search over the spans' payloads (contract 8.7 and 12.6).

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::http::header::ALLOW;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{FieldError, Problem, ProblemCode, SearchPage, SearchRequest};
use crucible_plan::{SEARCH_MAX_LIMIT, SearchField, TokenMode};
use crucible_query as query;
use crucible_query::{SEARCH_MAX_TOKENS, SearchQuery, search_tokens};
use crucible_records::{RequestId, TimeWindow, Timestamp};
use utoipa::OpenApi;

use super::cursor_invalid;
use crate::access::{ApiState, Project};
use crate::convert::{meta, span};
use crate::cursor::Cursors;
use crate::params::check_window;
use crate::problem::{ApiError, problem, request_id};

/// The longest query, in bytes.
const MAX_QUERY_BYTES: usize = 1_024;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    Router::new().route(
        "/projects/{project_id}/search",
        post(search).fallback(post_only),
    )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(search))]
pub(super) struct Paths;

/// Search span payloads
///
/// The spans that started in the window whose input, output or metadata hold the query, newest first, each
/// with the matching part of its fields. Search reads the store only: a span acknowledged moments ago may be
/// missing, and the page then says `consistency: lagging`.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/search",
    operation_id = "search",
    tag = "search",
    params(("project_id" = String, Path, description = "The project id (a UUID).")),
    request_body(content = SearchRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "One page of hits.", body = SearchPage),
        (status = 400, description = "`cursor_invalid`.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "`unauthenticated`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`: the key cannot see the project.", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "`payload_too_large`: the body is above 1 MiB.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`, `query_too_broad` or `query_memory_exceeded`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`telemetry_store_unavailable`.", body = Problem, content_type = "application/problem+json"),
        (status = 504, description = "`query_timeout`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("project_key" = [])),
)]
pub(crate) async fn search(
    State(state): State<ApiState>,
    project: Project,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<SearchPage>, ApiError> {
    let rid = project.request_id;
    let body = body.map_err(|_| {
        ApiError::new(
            rid,
            ProblemCode::PayloadTooLarge,
            "the request body could not be read; send at most 1 MiB",
        )
    })?;
    let request = parse(rid, &body)?;
    let (query, cursor) = checked(rid, request)?;
    check_window(rid, query.window, project.settings.max_window_days)?;
    let cursors = state.cursors(&project);
    let cursor = cursor.map(|text| cursors.open_search(&text, &query));
    let cursor = cursor.transpose().map_err(cursor_invalid(rid))?;
    let service = state.service(rid)?;
    let query = SearchQuery { cursor, ..query };
    let page = service.search(&project.ctx, query.clone()).await;
    let page = page.map_err(|e| ApiError::query(rid, &e))?;
    Ok(Json(search_page(&page, &query, &cursors)))
}

/// The search route takes POST only.
async fn post_only(State(state): State<ApiState>, request: Request) -> Response {
    let id = request_id(request.extensions(), &state.env);
    let mut response = problem(
        id,
        ProblemCode::MethodNotAllowed,
        "search takes POST with a JSON body",
    );
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("POST"));
    response
}

/// The body as a search request. A body that is not one is refused with the JSON pointer of the member that
/// failed.
fn parse(rid: RequestId, body: &[u8]) -> Result<SearchRequest, ApiError> {
    let invalid = |pointer: String, detail: &dyn std::fmt::Display| {
        ApiError::validation(
            rid,
            vec![FieldError {
                pointer,
                code: "invalid".to_owned(),
                message: format!("the body must be a JSON search request: {detail}"),
            }],
        )
    };
    let mut json = serde_json::Deserializer::from_slice(body);
    let request: SearchRequest = serde_path_to_error::deserialize(&mut json).map_err(|error| {
        let mut pointer = String::new();
        for segment in error.path().iter() {
            match segment {
                serde_path_to_error::Segment::Map { key } => {
                    pointer.push('/');
                    pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                }
                serde_path_to_error::Segment::Seq { index } => {
                    pointer.push('/');
                    pointer.push_str(&index.to_string());
                }
                _ => {}
            }
        }
        invalid(pointer, error.inner())
    })?;
    json.end().map_err(|error| invalid(String::new(), &error))?;
    Ok(request)
}

fn error(errors: &mut Vec<FieldError>, name: &str, code: &str, message: &str) {
    errors.push(FieldError {
        pointer: format!("/{name}"),
        code: code.to_owned(),
        message: message.to_owned(),
    });
}

/// An RFC 3339 time inside the valid range of contract 1.2.
fn time(errors: &mut Vec<FieldError>, name: &str, text: &str) -> Option<Timestamp> {
    match Timestamp::parse_rfc3339(text) {
        Ok(time) if time >= Timestamp::EPOCH && time <= Timestamp::MAX_EXCLUSIVE => Some(time),
        Ok(_) => {
            error(
                errors,
                name,
                "out_of_range",
                "expected a time from 1970 to 2100",
            );
            None
        }
        Err(_) => {
            error(
                errors,
                name,
                "invalid_timestamp",
                "expected an RFC 3339 date-time",
            );
            None
        }
    }
}

const fn plan_field(field: wire::SearchField) -> SearchField {
    match field {
        wire::SearchField::Input => SearchField::Input,
        wire::SearchField::Output => SearchField::Output,
        wire::SearchField::Metadata => SearchField::Metadata,
    }
}

const fn wire_field(field: SearchField) -> wire::SearchField {
    match field {
        SearchField::Input => wire::SearchField::Input,
        SearchField::Output => wire::SearchField::Output,
        SearchField::Metadata => wire::SearchField::Metadata,
    }
}

/// The search the request asks for, and its cursor's text; every problem of the request at once.
fn checked(
    rid: RequestId,
    request: SearchRequest,
) -> Result<(SearchQuery, Option<String>), ApiError> {
    let mut errors = Vec::new();
    let (from, to) = (
        time(&mut errors, "from", &request.from),
        time(&mut errors, "to", &request.to),
    );
    let window = match (from, to) {
        (Some(from), Some(to)) => {
            let window = TimeWindow::new(from, to).ok();
            if window.is_none() {
                error(
                    &mut errors,
                    "to",
                    "invalid_window",
                    "`to` must be after `from`",
                );
            }
            window
        }
        _ => None,
    };
    let mode = match request.mode.unwrap_or_default() {
        wire::SearchMode::All => TokenMode::All,
        wire::SearchMode::Any => TokenMode::Any,
        wire::SearchMode::Phrase => TokenMode::Phrase,
    };
    let fields: Vec<SearchField> = request
        .fields
        .unwrap_or_else(|| vec![wire::SearchField::Input, wire::SearchField::Output])
        .into_iter()
        .map(plan_field)
        .collect();
    if fields.is_empty() {
        error(
            &mut errors,
            "fields",
            "required",
            "name at least one of `input`, `output` and `metadata`, or leave `fields` out",
        );
    }
    if fields
        .iter()
        .enumerate()
        .any(|(at, field)| fields[..at].contains(field))
    {
        error(&mut errors, "fields", "repeated", "name each field once");
    }
    if request.query.len() > MAX_QUERY_BYTES {
        let message = format!("the query is longer than {MAX_QUERY_BYTES} bytes; shorten it");
        error(&mut errors, "query", "too_long", &message);
    } else if !fields.is_empty() {
        // Text fields and metadata take different tokens: a kind without any is not searched, but one kind
        // must have some, and none too many.
        let kinds = [SearchField::Input, SearchField::Metadata];
        let counts: Vec<usize> = kinds
            .into_iter()
            .filter(|kind| fields.iter().any(|field| field.lowered() == kind.lowered()))
            .map(|kind| search_tokens(&request.query, kind, mode).len())
            .collect();
        if counts.iter().all(|count| *count == 0) {
            let message = "the query has nothing to search for: `input` and `output` need a letter or a                            digit, `metadata` a `key=value` entry";
            error(&mut errors, "query", "no_tokens", message);
        } else if counts.iter().any(|count| *count > SEARCH_MAX_TOKENS) {
            let message = format!("the query has more than {SEARCH_MAX_TOKENS} tokens; shorten it");
            error(&mut errors, "query", "too_many_tokens", &message);
        }
    }
    if let Some(limit) = request.limit
        && (limit == 0 || limit > SEARCH_MAX_LIMIT)
    {
        let message = format!("expected an integer from 1 to {SEARCH_MAX_LIMIT}");
        error(&mut errors, "limit", "out_of_range", &message);
    }
    match window {
        Some(window) if errors.is_empty() => Ok((
            SearchQuery {
                window,
                text: request.query,
                mode,
                fields,
                limit: request.limit,
                cursor: None,
            },
            request.cursor,
        )),
        _ => Err(ApiError::validation(rid, errors)),
    }
}

fn hit(hit: &query::SearchHit) -> wire::SearchHit {
    wire::SearchHit {
        trace_id: hit.span.summary.trace_id.to_string(),
        span: span(&hit.span),
        matches: hit
            .matches
            .iter()
            .map(|found| wire::SearchMatch {
                field: wire_field(found.field),
                fragments: found
                    .fragments
                    .iter()
                    .map(|fragment| wire::SearchFragment {
                        text: fragment.text.clone(),
                        highlighted: fragment.highlighted,
                    })
                    .collect(),
                cut_before: found.cut_before,
                cut_after: found.cut_after,
            })
            .collect(),
    }
}

fn search_page(page: &query::SearchPage, query: &SearchQuery, cursors: &Cursors<'_>) -> SearchPage {
    SearchPage {
        data: page.data.iter().map(hit).collect(),
        next_cursor: page
            .next_cursor
            .as_ref()
            .map(|cursor| cursors.seal_search(cursor, query)),
        has_more: page.has_more,
        meta: meta(&page.meta),
    }
}
