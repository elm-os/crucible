//! Principal resolution and the permission extractor (contract 11.6 and 12.2), for the one principal of
//! milestone 1: a project's secret key presented as `Authorization: Bearer`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use crucible_api_types::ProblemCode;
use crucible_auth::KeyVerifier;
use crucible_cursor::KeyRing;
use crucible_env::Env;
use crucible_plan::Caller;
use crucible_query::{QueryService, ReadContext};
use crucible_records::{ApiKeyId, ProjectId, RequestId};
use crucible_sqlws::SqlWorkspace;

use crate::cursor::Cursors;
use crate::problem::{ApiError, request_id};
use crate::settings::{ProjectSettings, ProjectSettingsSource};

/// What every handler shares.
#[derive(Clone, Debug)]
pub(crate) struct ApiState {
    pub(crate) env: Env,
    /// `None` until a telemetry store is configured: requests are still authenticated and validated, then
    /// answered with `telemetry_store_unavailable`.
    pub(crate) service: Option<QueryService>,
    pub(crate) keys: Arc<dyn KeyVerifier>,
    pub(crate) settings: Arc<dyn ProjectSettingsSource>,
    /// The cursor-signing keys.
    pub(crate) cursors: Arc<KeyRing>,
    /// The SQL workspace; `None` when it is not configured.
    pub(crate) sql: Option<SqlWorkspace>,
}

impl ApiState {
    /// The cursors of a request of `project`, served now.
    pub(crate) fn cursors(&self, project: &Project) -> Cursors<'_> {
        Cursors {
            ring: &self.cursors,
            project_id: project.ctx.scope().project_id(),
            now: self.env.clock.wall_now(),
        }
    }

    /// The query service, or the problem that says why there is none.
    pub(crate) fn service(&self, request_id: RequestId) -> Result<&QueryService, ApiError> {
        self.service.as_ref().ok_or_else(|| {
            ApiError::new(
                request_id,
                ProblemCode::TelemetryStoreUnavailable,
                "no telemetry store is configured",
            )
        })
    }
}

/// The secret of `Authorization: Bearer <secret>`. The cookie and every other scheme are ignored.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, secret) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| secret.trim())
}

/// A request authorized to read one project: the permission extractor of contract 11.6. Its only source of a
/// `ProjectScope` is the verified key; the path's `{project_id}` must name the key's project, and anything
/// else, a malformed id included, is `not_found`, so a caller learns nothing about projects it cannot see.
/// The read runs in the `agent` class with the project's retention.
#[derive(Debug)]
pub(crate) struct Project {
    pub(crate) request_id: RequestId,
    pub(crate) ctx: ReadContext,
    pub(crate) settings: ProjectSettings,
    /// The key that authenticated the request.
    pub(crate) api_key_id: ApiKeyId,
    /// The path parameters, `project_id` included.
    pub(crate) path: BTreeMap<String, String>,
}

impl FromRequestParts<ApiState> for Project {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let request_id = request_id(&parts.extensions, &state.env);
        let principal = bearer(&parts.headers)
            .and_then(|secret| state.keys.verify(secret))
            .ok_or_else(|| ApiError::unauthenticated(request_id))?;
        let Path(path) = Path::<BTreeMap<String, String>>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::not_found(request_id))?;
        let project_id = principal.scope.project_id();
        let named = path
            .get("project_id")
            .and_then(|id| ProjectId::parse(id).ok());
        if named != Some(project_id) {
            return Err(ApiError::not_found(request_id));
        }
        let settings = state.settings.settings(project_id);
        Ok(Self {
            request_id,
            ctx: ReadContext::new(principal.scope, settings.retention, Caller::Agent),
            settings,
            api_key_id: principal.api_key_id,
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn only_a_bearer_secret_is_read() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        for (value, expected) in [
            ("Bearer csk_1", Some("csk_1")),
            ("bearer  csk_1 ", Some("csk_1")),
            ("BEARER csk_1", Some("csk_1")),
            ("Basic Y3BrOmNzaw==", None),
            ("Bearer", None),
            ("csk_1", None),
        ] {
            headers.insert(AUTHORIZATION, HeaderValue::from_static(value));
            assert_eq!(bearer(&headers), expected, "{value}");
        }
    }
}
