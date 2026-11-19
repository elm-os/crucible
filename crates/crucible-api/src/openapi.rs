//! The OpenAPI 3.1 document, generated from the handlers (contract 12.1). `crucible openapi` prints it and it
//! is committed at `specs/openapi/crucible-v1.json`; a test fails when the two differ. The document holds the
//! info, the security scheme and the tags; each route group adds its paths (`routes::document`).

use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::routes;

/// The `project_key` security scheme: a project's secret key as a Bearer token (contract 12.2).
struct ProjectKey;

impl Modify for ProjectKey {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let scheme = HttpBuilder::new()
            .scheme(HttpAuthScheme::Bearer)
            .description(Some(
                "A project's secret key (`csk_...`) as `Authorization: Bearer <key>`.",
            ))
            .build();
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme("project_key", SecurityScheme::Http(scheme));
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                "crucible_session",
                "The session cookie set by sign-in (`__Host-crucible_session` over HTTPS). Requests other than                  GET and HEAD also send `X-Crucible-CSRF` with the session's CSRF token.",
            ))),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Crucible",
        description = "The REST API of Crucible, observability and analytics for AI agents and MCP servers. \
                       Errors are `application/problem+json` documents with a stable `code`.",
        license(name = "MIT"),
    ),
    modifiers(&ProjectKey),
    tags(
        (name = "auth", description = "Sign-in, sessions and the signed-in user's account."),
        (name = "facts", description = "Scores, tags, flags and corrections written about traces, spans and sessions."),
        (name = "lifecycle", description = "Retention settings and deletion requests."),
        (name = "metrics", description = "Time series over the project's spans and traces, their schema and filter values."),
        (name = "org", description = "The organization, its members, invitations and sign-in providers."),
        (name = "projects", description = "Projects, project members and API keys."),
        (name = "search", description = "Full-text search over the spans' inputs, outputs and metadata."),
        (name = "sql", description = "The read-only SQL workspace: runs, exports, history and saved queries."),
        (name = "traces", description = "Traces, their spans and their payloads, and the project's data bounds."),
    ),
)]
struct ApiDoc;

/// The OpenAPI document. `info.version` is the binary's version: every crate shares the workspace version.
pub fn openapi() -> utoipa::openapi::OpenApi {
    routes::document(ApiDoc::openapi())
}

/// The OpenAPI document as pretty-printed JSON with a final newline, as committed.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    let mut json = openapi().to_pretty_json()?;
    json.push('\n');
    Ok(json)
}
