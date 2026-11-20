//! Dashboards and their widgets: `projects/{project_id}/dashboards/...` (contracts 11.1 and 12.6).
//!
//! Reading needs `read`; creating, changing, duplicating and deleting need `edit_content`, which no API key holds
//! (contract 11.3). `{dashboard_id}` is a dashboard's id or a built-in dashboard's key. Built-in dashboards are
//! read-only: a change to one is refused with `conflict` and says to duplicate it. The dashboard's version covers
//! its widgets, so the widget routes take the dashboard's `ETag` and answer with its next one. Without a control
//! plane the built-in dashboards are still served, and every write says how to turn the control plane on.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::{get, post};
use axum::{Json, Router};
use crucible_api_types as wire;
use crucible_api_types::{FieldError, Problem, ProblemCode, WidgetInput, WidgetKind};
use crucible_control as control;
use crucible_control::ControlStore;
use crucible_records::{DashboardId, RequestId, WidgetId};
use utoipa::OpenApi;

use super::alerts::store_problem;
use crate::access::{ApiState, EditContent, Read, Scoped};
use crate::control::{ApiJson, etag, if_match};
use crate::dashboards::{
    Draft, builtin_summary, check_description, check_name, check_widgets, starter, starters,
    stored_widget, summary, wire_dashboard, wire_widget,
};
use crate::params::Params;
use crate::problem::{ApiError, field_error};

/// How many times adding a widget reads the dashboard again when another change lands first.
const ADD_ATTEMPTS: usize = 3;

/// The group's routes, relative to the base path.
pub(super) fn routes() -> Router<ApiState> {
    let base = "/projects/{project_id}/dashboards";
    Router::new()
        .route(base, get(list_dashboards).post(create_dashboard))
        .route(
            &format!("{base}/{{dashboard_id}}"),
            get(get_dashboard)
                .patch(patch_dashboard)
                .delete(delete_dashboard),
        )
        .route(
            &format!("{base}/{{dashboard_id}}/duplicate"),
            post(duplicate_dashboard),
        )
        .route(
            &format!("{base}/{{dashboard_id}}/widgets"),
            post(create_widget),
        )
        .route(
            &format!("{base}/{{dashboard_id}}/widgets/{{widget_id}}"),
            axum::routing::patch(patch_widget).delete(delete_widget),
        )
}

/// The OpenAPI paths of the group.
#[derive(OpenApi)]
#[openapi(paths(
    list_dashboards,
    create_dashboard,
    get_dashboard,
    patch_dashboard,
    delete_dashboard,
    duplicate_dashboard,
    create_widget,
    patch_widget,
    delete_widget,
))]
pub(super) struct Paths;

/// What `{dashboard_id}` names.
enum Target {
    Stored(DashboardId),
    Builtin(wire::Dashboard),
}

fn target<P>(scoped: &Scoped<P>) -> Result<Target, ApiError> {
    let text = scoped
        .path
        .get("dashboard_id")
        .map(String::as_str)
        .unwrap_or_default();
    if let Ok(id) = DashboardId::parse(text) {
        return Ok(Target::Stored(id));
    }
    starter(text)
        .map(Target::Builtin)
        .ok_or_else(|| ApiError::not_found(scoped.request_id))
}

/// The stored dashboard a write names: a built-in one is read-only.
fn stored<P>(scoped: &Scoped<P>) -> Result<DashboardId, ApiError> {
    match target(scoped)? {
        Target::Stored(id) => Ok(id),
        Target::Builtin(dashboard) => Err(ApiError::new(
            scoped.request_id,
            ProblemCode::Conflict,
            format!(
                "the built-in dashboard `{}` is read-only; duplicate it (POST .../dashboards/{}/duplicate) and \
                 change the copy",
                dashboard.id, dashboard.id
            ),
        )),
    }
}

fn store(state: &ApiState, rid: RequestId) -> Result<&Arc<dyn ControlStore>, ApiError> {
    state.dashboards.as_ref().ok_or_else(|| {
        ApiError::new(
            rid,
            ProblemCode::FeatureDisabled,
            "dashboards are stored in the control plane; set postgres.url (CRUCIBLE_POSTGRES_URL). The built-in \
             dashboards are served without it",
        )
    })
}

fn stale(rid: RequestId) -> ApiError {
    ApiError::new(
        rid,
        ProblemCode::PreconditionFailed,
        "the dashboard changed since it was read; read it again and repeat the change with its new ETag",
    )
}

fn unreadable(rid: RequestId, cause: &str) -> ApiError {
    tracing::error!(request_id = %rid, %cause, "a stored dashboard could not be read");
    ApiError::new(rid, ProblemCode::InternalError, "internal error")
}

fn validated(rid: RequestId, errors: Vec<FieldError>) -> Result<(), ApiError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::validation(rid, errors))
    }
}

async fn read(
    store: &dyn ControlStore,
    rid: RequestId,
    project: crucible_records::ProjectId,
    id: DashboardId,
) -> Result<control::DashboardRows, ApiError> {
    store
        .dashboards()
        .get(project, id)
        .await
        .map_err(|e| store_problem(rid, &e))?
        .ok_or_else(|| ApiError::not_found(rid))
}

/// The answer to a compare-and-set that did not apply: the dashboard is gone, or it changed.
async fn missed(
    store: &dyn ControlStore,
    rid: RequestId,
    project: crucible_records::ProjectId,
    id: DashboardId,
) -> ApiError {
    match store.dashboards().get(project, id).await {
        Ok(Some(_)) => stale(rid),
        Ok(None) => ApiError::not_found(rid),
        Err(error) => store_problem(rid, &error),
    }
}

/// The widgets of `inputs` as stored, with new ids for new widgets. An id must name a widget of `current`, once.
fn assign_ids(
    rid: RequestId,
    env: &crucible_env::Env,
    inputs: &[WidgetInput],
    current: &[control::Widget],
) -> Result<Vec<WidgetId>, ApiError> {
    let mut ids = Vec::new();
    let mut errors = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        let member = format!("widgets/{i}/id");
        match &input.id {
            None => ids.push(WidgetId::from_uuid(env.new_id())),
            Some(text) => match WidgetId::parse(text) {
                Ok(id) if !current.iter().any(|w| w.id == id) => {
                    let message = "not a widget of this dashboard; leave `id` out to add a widget";
                    errors.push(field_error(&member, "unknown", message));
                }
                Ok(id) if ids.contains(&id) => {
                    errors.push(field_error(
                        &member,
                        "duplicate",
                        "the widget is listed twice",
                    ));
                }
                Ok(id) => ids.push(id),
                Err(_) => {
                    let message = "not a widget id; leave `id` out to add a widget";
                    errors.push(field_error(&member, "invalid", message));
                }
            },
        }
    }
    validated(rid, errors)?;
    Ok(ids)
}

/// Checks a whole set of widgets from a request body's `widgets`.
fn check_inputs(rid: RequestId, inputs: &[WidgetInput]) -> Result<(), ApiError> {
    let places: Vec<String> = (0..inputs.len()).map(|i| format!("widgets/{i}/")).collect();
    let drafts: Vec<Draft<'_>> = inputs
        .iter()
        .zip(&places)
        .map(|(input, place)| Draft {
            at: Some(place),
            query: input.query.as_ref(),
            display: &input.display,
            layout: input.layout,
        })
        .collect();
    validated(rid, check_widgets(rid, &drafts, "widgets")?)
}

fn stored_inputs(ids: &[WidgetId], inputs: &[WidgetInput]) -> Vec<control::Widget> {
    ids.iter()
        .zip(inputs)
        .map(|(id, input)| stored_widget(*id, input.query.as_ref(), &input.display, input.layout))
        .collect()
}

/// The stored widgets of a dashboard as drafts that the request does not change.
fn kept(widgets: &[wire::Widget]) -> impl Iterator<Item = Draft<'_>> {
    widgets.iter().map(|widget| Draft {
        at: None,
        query: widget.query.as_ref(),
        display: &widget.display,
        layout: widget.layout,
    })
}

fn wire_rows(rid: RequestId, rows: &control::DashboardRows) -> Result<wire::Dashboard, ApiError> {
    wire_dashboard(rows).map_err(|cause| unreadable(rid, &cause))
}

fn widget_id<P>(scoped: &Scoped<P>) -> Result<WidgetId, ApiError> {
    scoped
        .path
        .get("widget_id")
        .and_then(|text| WidgetId::parse(text).ok())
        .ok_or_else(|| ApiError::not_found(scoped.request_id))
}

/// The copy's default name: the original's, shortened so that " (copy)" fits.
fn copy_name(name: &str) -> String {
    let base: String = name.chars().take(113).collect();
    format!("{} (copy)", base.trim_end())
}

/// List dashboards
///
/// The project's dashboards, oldest first, then the built-in starter dashboards, which are read-only and carry
/// a `builtin_key`. Without a control plane, the built-in dashboards only.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/dashboards",
    operation_id = "list_dashboards",
    tag = "dashboards",
    params(("project_id" = String, Path, description = "The project id.")),
    responses(
        (status = 200, description = "The dashboards.", body = wire::DashboardList),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []), ("project_key" = [])),
)]
pub(crate) async fn list_dashboards(
    State(state): State<ApiState>,
    scoped: Scoped<Read>,
    uri: Uri,
) -> Result<Json<wire::DashboardList>, ApiError> {
    let rid = scoped.request_id;
    Params::new(rid, &uri)?.finish()?;
    let mut data = match &state.dashboards {
        Some(store) => store
            .dashboards()
            .list(scoped.scope.project_id())
            .await
            .map_err(|e| store_problem(rid, &e))?
            .iter()
            .map(summary)
            .collect(),
        None => Vec::new(),
    };
    data.extend(starters().iter().map(builtin_summary));
    Ok(Json(wire::DashboardList { data }))
}

/// Create a dashboard
///
/// A dashboard with its name and, optionally, its widgets. Each widget is checked against the rules of its kind
/// and the metrics schema, and no two may overlap.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/dashboards",
    operation_id = "create_dashboard",
    tag = "dashboards",
    params(("project_id" = String, Path, description = "The project id.")),
    request_body = wire::DashboardCreate,
    responses(
        (status = 201, description = "The dashboard; `ETag` is its version.", body = wire::Dashboard),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`, or `feature_disabled` without a control plane.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`: `errors` names each member to change.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_dashboard(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    ApiJson(body): ApiJson<wire::DashboardCreate>,
) -> Result<(StatusCode, HeaderMap, Json<wire::Dashboard>), ApiError> {
    let rid = scoped.request_id;
    let store = store(&state, rid)?;
    let description = body.description.unwrap_or_default();
    let inputs = body.widgets.unwrap_or_default();
    let mut errors: Vec<FieldError> = check_name(&body.name)
        .into_iter()
        .chain(check_description(&description))
        .collect();
    for (i, input) in inputs.iter().enumerate() {
        if input.id.is_some() {
            let message = "a new dashboard's widgets take no id; leave `id` out";
            errors.push(field_error(&format!("widgets/{i}/id"), "invalid", message));
        }
    }
    validated(rid, errors)?;
    check_inputs(rid, &inputs)?;
    let ids = assign_ids(rid, &state.env, &inputs, &[])?;
    let now = state.env.clock.wall_now();
    let rows = control::DashboardRows {
        dashboard: control::Dashboard {
            id: DashboardId::from_uuid(state.env.new_id()),
            project_id: scoped.scope.project_id(),
            name: body.name,
            description,
            version: 1,
            created_at: now,
            updated_at: now,
        },
        widgets: stored_inputs(&ids, &inputs),
    };
    store
        .dashboards()
        .insert(&rows.dashboard, &rows.widgets)
        .await
        .map_err(|e| store_problem(rid, &e))?;
    Ok((
        StatusCode::CREATED,
        etag(rows.dashboard.version),
        Json(wire_rows(rid, &rows)?),
    ))
}

/// Get a dashboard
///
/// A dashboard with its widgets in reading order. `{dashboard_id}` is a dashboard's id or a built-in
/// dashboard's `builtin_key`.
#[utoipa::path(
    get,
    path = "/api/v1/projects/{project_id}/dashboards/{dashboard_id}",
    operation_id = "get_dashboard",
    tag = "dashboards",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("dashboard_id" = String, Path, description = "The dashboard id, or a built-in dashboard's key."),
    ),
    responses(
        (status = 200, description = "The dashboard; `ETag` is its version.", body = wire::Dashboard),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "`control_plane_unavailable`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []), ("project_key" = [])),
)]
pub(crate) async fn get_dashboard(
    State(state): State<ApiState>,
    scoped: Scoped<Read>,
    uri: Uri,
) -> Result<(HeaderMap, Json<wire::Dashboard>), ApiError> {
    let rid = scoped.request_id;
    Params::new(rid, &uri)?.finish()?;
    let dashboard = match target(&scoped)? {
        Target::Builtin(dashboard) => dashboard,
        Target::Stored(id) => {
            let store = state
                .dashboards
                .as_ref()
                .ok_or_else(|| ApiError::not_found(rid))?;
            let rows = read(store.as_ref(), rid, scoped.scope.project_id(), id).await?;
            wire_rows(rid, &rows)?
        }
    };
    Ok((etag(u64::from(dashboard.version)), Json(dashboard)))
}

/// Change a dashboard
///
/// Changes the name, the description, or the whole set of widgets in one change: a widget with an `id` is
/// rewritten, one without is added, and one the list leaves out is deleted. The result is checked whole, so
/// widgets can trade places.
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/dashboards/{dashboard_id}",
    operation_id = "patch_dashboard",
    tag = "dashboards",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("dashboard_id" = String, Path, description = "The dashboard id."),
        ("If-Match" = String, Header, description = "The dashboard's `ETag`."),
    ),
    request_body = wire::DashboardPatch,
    responses(
        (status = 200, description = "The dashboard; `ETag` is its new version.", body = wire::Dashboard),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: a built-in dashboard is read-only.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_dashboard(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::DashboardPatch>,
) -> Result<(HeaderMap, Json<wire::Dashboard>), ApiError> {
    let rid = scoped.request_id;
    let id = stored(&scoped)?;
    let store = store(&state, rid)?;
    let expected = if_match(&headers, rid)?;
    let project = scoped.scope.project_id();
    let current = read(store.as_ref(), rid, project, id).await?;
    if current.dashboard.version != expected {
        return Err(stale(rid));
    }
    let mut errors = Vec::new();
    errors.extend(body.name.as_deref().and_then(check_name));
    errors.extend(body.description.as_deref().and_then(check_description));
    validated(rid, errors)?;
    let widgets = match &body.widgets {
        Some(inputs) => {
            let ids = assign_ids(rid, &state.env, inputs, &current.widgets)?;
            check_inputs(rid, inputs)?;
            stored_inputs(&ids, inputs)
        }
        None => current.widgets.clone(),
    };
    let dashboard = control::Dashboard {
        name: body.name.unwrap_or(current.dashboard.name),
        description: body.description.unwrap_or(current.dashboard.description),
        updated_at: state.env.clock.wall_now(),
        ..current.dashboard
    };
    write(store.as_ref(), rid, dashboard, widgets, expected).await
}

/// Writes the dashboard and its widgets under `expected` and answers with what is stored.
async fn write(
    store: &dyn ControlStore,
    rid: RequestId,
    dashboard: control::Dashboard,
    widgets: Vec<control::Widget>,
    expected: u64,
) -> Result<(HeaderMap, Json<wire::Dashboard>), ApiError> {
    let changed = store
        .dashboards()
        .update_if_version(&dashboard, &widgets, expected)
        .await
        .map_err(|e| store_problem(rid, &e))?;
    if !changed {
        return Err(missed(store, rid, dashboard.project_id, dashboard.id).await);
    }
    let rows = control::DashboardRows {
        dashboard: control::Dashboard {
            version: expected + 1,
            ..dashboard
        },
        widgets,
    };
    Ok((etag(rows.dashboard.version), Json(wire_rows(rid, &rows)?)))
}

/// Delete a dashboard
///
/// Deletes the dashboard with its widgets.
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/dashboards/{dashboard_id}",
    operation_id = "delete_dashboard",
    tag = "dashboards",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("dashboard_id" = String, Path, description = "The dashboard id."),
        ("If-Match" = String, Header, description = "The dashboard's `ETag`."),
    ),
    responses(
        (status = 204, description = "Deleted."),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: a built-in dashboard is read-only.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_dashboard(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let rid = scoped.request_id;
    let id = stored(&scoped)?;
    let store = store(&state, rid)?;
    let expected = if_match(&headers, rid)?;
    let project = scoped.scope.project_id();
    let deleted = store
        .dashboards()
        .delete_if_version(project, id, expected)
        .await
        .map_err(|e| store_problem(rid, &e))?;
    if !deleted {
        return Err(missed(store.as_ref(), rid, project, id).await);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Duplicate a dashboard
///
/// Copies a dashboard, or a built-in dashboard named by its key, into a new dashboard of the project, with new
/// widget ids. The copy can be changed.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/duplicate",
    operation_id = "duplicate_dashboard",
    tag = "dashboards",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("dashboard_id" = String, Path, description = "The dashboard id, or a built-in dashboard's key."),
    ),
    request_body = wire::DashboardDuplicate,
    responses(
        (status = 201, description = "The copy; `ETag` is its version.", body = wire::Dashboard),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`, or `feature_disabled` without a control plane.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn duplicate_dashboard(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    ApiJson(body): ApiJson<wire::DashboardDuplicate>,
) -> Result<(StatusCode, HeaderMap, Json<wire::Dashboard>), ApiError> {
    let rid = scoped.request_id;
    let target = target(&scoped)?;
    let store = store(&state, rid)?;
    let project = scoped.scope.project_id();
    let source = match target {
        Target::Builtin(dashboard) => dashboard,
        Target::Stored(id) => wire_rows(rid, &read(store.as_ref(), rid, project, id).await?)?,
    };
    let name = body.name.unwrap_or_else(|| copy_name(&source.name));
    validated(rid, check_name(&name).into_iter().collect())?;
    let inputs: Vec<WidgetInput> = source
        .widgets
        .iter()
        .map(|w| WidgetInput {
            id: None,
            query: w.query.clone(),
            display: w.display.clone(),
            layout: w.layout,
        })
        .collect();
    check_inputs(rid, &inputs)?;
    let ids = assign_ids(rid, &state.env, &inputs, &[])?;
    let now = state.env.clock.wall_now();
    let rows = control::DashboardRows {
        dashboard: control::Dashboard {
            id: DashboardId::from_uuid(state.env.new_id()),
            project_id: project,
            name,
            description: source.description,
            version: 1,
            created_at: now,
            updated_at: now,
        },
        widgets: stored_inputs(&ids, &inputs),
    };
    store
        .dashboards()
        .insert(&rows.dashboard, &rows.widgets)
        .await
        .map_err(|e| store_problem(rid, &e))?;
    Ok((
        StatusCode::CREATED,
        etag(rows.dashboard.version),
        Json(wire_rows(rid, &rows)?),
    ))
}

/// Add a widget
///
/// Adds one widget where it does not overlap another. It takes no `If-Match`: when another change lands first,
/// the widget is checked again against it. The answer's `ETag` is the dashboard's new version.
#[utoipa::path(
    post,
    path = "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/widgets",
    operation_id = "create_widget",
    tag = "dashboards",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("dashboard_id" = String, Path, description = "The dashboard id."),
    ),
    request_body = wire::WidgetInput,
    responses(
        (status = 201, description = "The widget; `ETag` is the dashboard's new version.", body = wire::Widget),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: a built-in dashboard is read-only, or the dashboard kept changing.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn create_widget(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    ApiJson(body): ApiJson<WidgetInput>,
) -> Result<(StatusCode, HeaderMap, Json<wire::Widget>), ApiError> {
    let rid = scoped.request_id;
    let id = stored(&scoped)?;
    let store = store(&state, rid)?;
    if body.id.is_some() {
        let message = "a new widget takes no id; leave `id` out";
        return Err(ApiError::validation(
            rid,
            vec![field_error("id", "invalid", message)],
        ));
    }
    let project = scoped.scope.project_id();
    let widget_id = WidgetId::from_uuid(state.env.new_id());
    let added = stored_widget(widget_id, body.query.as_ref(), &body.display, body.layout);
    for _ in 0..ADD_ATTEMPTS {
        let current = read(store.as_ref(), rid, project, id).await?;
        let shown = wire_rows(rid, &current)?;
        let drafts: Vec<Draft<'_>> = kept(&shown.widgets)
            .chain([Draft {
                at: Some(""),
                query: body.query.as_ref(),
                display: &body.display,
                layout: body.layout,
            }])
            .collect();
        validated(rid, check_widgets(rid, &drafts, "")?)?;
        let mut widgets = current.widgets;
        widgets.push(added.clone());
        let dashboard = control::Dashboard {
            updated_at: state.env.clock.wall_now(),
            ..current.dashboard
        };
        let expected = dashboard.version;
        let written = store
            .dashboards()
            .update_if_version(&dashboard, &widgets, expected)
            .await
            .map_err(|e| store_problem(rid, &e))?;
        if written {
            let widget = wire_widget(&added).map_err(|cause| unreadable(rid, &cause))?;
            return Ok((StatusCode::CREATED, etag(expected + 1), Json(widget)));
        }
    }
    Err(ApiError::new(
        rid,
        ProblemCode::Conflict,
        "the dashboard kept changing while the widget was added; try again",
    ))
}

/// Change a widget
///
/// Changes the members in the body. A `markdown` display drops the query. The result is checked against the
/// dashboard's other widgets. The answer's `ETag` is the dashboard's new version.
#[utoipa::path(
    patch,
    path = "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/widgets/{widget_id}",
    operation_id = "patch_widget",
    tag = "dashboards",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("dashboard_id" = String, Path, description = "The dashboard id."),
        ("widget_id" = String, Path, description = "The widget id."),
        ("If-Match" = String, Header, description = "The dashboard's `ETag`."),
    ),
    request_body = wire::WidgetPatch,
    responses(
        (status = 200, description = "The widget; `ETag` is the dashboard's new version.", body = wire::Widget),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: a built-in dashboard is read-only.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "`validation_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn patch_widget(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<wire::WidgetPatch>,
) -> Result<(HeaderMap, Json<wire::Widget>), ApiError> {
    let rid = scoped.request_id;
    let id = stored(&scoped)?;
    let widget_id = widget_id(&scoped)?;
    let store = store(&state, rid)?;
    let expected = if_match(&headers, rid)?;
    let current = read(store.as_ref(), rid, scoped.scope.project_id(), id).await?;
    if current.dashboard.version != expected {
        return Err(stale(rid));
    }
    let mut shown = wire_rows(rid, &current)?.widgets;
    let at = shown
        .iter()
        .position(|w| w.id == widget_id.to_string())
        .ok_or_else(|| ApiError::not_found(rid))?;
    let mut changed = shown.remove(at);
    if let Some(display) = body.display {
        changed.display = display;
    }
    if body.query.is_some() {
        changed.query = body.query;
    }
    if changed.display.kind == WidgetKind::Markdown {
        changed.query = None;
    }
    if let Some(layout) = body.layout {
        changed.layout = layout;
    }
    let drafts: Vec<Draft<'_>> = kept(&shown)
        .chain([Draft {
            at: Some(""),
            query: changed.query.as_ref(),
            display: &changed.display,
            layout: changed.layout,
        }])
        .collect();
    validated(rid, check_widgets(rid, &drafts, "")?)?;
    let widgets: Vec<control::Widget> = current
        .widgets
        .iter()
        .map(|w| {
            if w.id == widget_id {
                stored_widget(
                    widget_id,
                    changed.query.as_ref(),
                    &changed.display,
                    changed.layout,
                )
            } else {
                w.clone()
            }
        })
        .collect();
    let dashboard = control::Dashboard {
        updated_at: state.env.clock.wall_now(),
        ..current.dashboard
    };
    let (headers, _) = write(store.as_ref(), rid, dashboard, widgets, expected).await?;
    Ok((headers, Json(changed)))
}

/// Delete a widget
///
/// Removes one widget. The answer's `ETag` is the dashboard's new version.
#[utoipa::path(
    delete,
    path = "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/widgets/{widget_id}",
    operation_id = "delete_widget",
    tag = "dashboards",
    params(
        ("project_id" = String, Path, description = "The project id."),
        ("dashboard_id" = String, Path, description = "The dashboard id."),
        ("widget_id" = String, Path, description = "The widget id."),
        ("If-Match" = String, Header, description = "The dashboard's `ETag`."),
    ),
    responses(
        (status = 204, description = "Removed; `ETag` is the dashboard's new version."),
        (status = 403, description = "`forbidden`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: a built-in dashboard is read-only.", body = Problem, content_type = "application/problem+json"),
        (status = 412, description = "`precondition_failed`.", body = Problem, content_type = "application/problem+json"),
        (status = 428, description = "`precondition_required`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = [])),
)]
pub(crate) async fn delete_widget(
    State(state): State<ApiState>,
    scoped: Scoped<EditContent>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap), ApiError> {
    let rid = scoped.request_id;
    let id = stored(&scoped)?;
    let widget_id = widget_id(&scoped)?;
    let store = store(&state, rid)?;
    let expected = if_match(&headers, rid)?;
    let current = read(store.as_ref(), rid, scoped.scope.project_id(), id).await?;
    if current.dashboard.version != expected {
        return Err(stale(rid));
    }
    if !current.widgets.iter().any(|w| w.id == widget_id) {
        return Err(ApiError::not_found(rid));
    }
    let widgets: Vec<control::Widget> = current
        .widgets
        .iter()
        .filter(|w| w.id != widget_id)
        .cloned()
        .collect();
    let dashboard = control::Dashboard {
        updated_at: state.env.clock.wall_now(),
        ..current.dashboard
    };
    let (headers, _) = write(store.as_ref(), rid, dashboard, widgets, expected).await?;
    Ok((StatusCode::NO_CONTENT, headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboards::reading_order;

    #[test]
    fn a_copy_name_fits_the_name_rule() {
        assert_eq!(copy_name("Overview"), "Overview (copy)");
        let long = "x".repeat(120);
        assert!(check_name(&copy_name(&long)).is_none());
        assert_eq!(copy_name(&long).chars().count(), 120);
    }

    #[test]
    fn widgets_sort_by_row_then_column() {
        let widget = |id: &str, x: u32, y: u32| wire::Widget {
            id: id.to_owned(),
            query: None,
            display: wire::WidgetDisplay {
                kind: WidgetKind::Markdown,
                title: id.to_owned(),
                chart: None,
                text: Some("x".to_owned()),
            },
            layout: wire::WidgetLayout { x, y, w: 2, h: 1 },
        };
        let mut widgets = [widget("c", 0, 1), widget("b", 4, 0), widget("a", 0, 0)];
        widgets.sort_by(reading_order);
        let ids: Vec<&str> = widgets.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }
}
