//! The permission of every route (contract 11.6): the public allowlist, session-only routes, org permissions and
//! project permissions. A test walks the OpenAPI document against this table and the routes against their
//! answers, so a route without an entry fails CI; `cargo xtask authz-matrix` renders the table into
//! `docs/architecture/authz-matrix.md`.

use crucible_auth::roles::{OrgPermission, ProjectPermission};

/// Who may call a route.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Anyone: the public allowlist of contract 12.2.
    Public,
    /// Any signed-in user.
    Session,
    /// A signed-in user with this org permission.
    Org(OrgPermission),
    /// A session or API key with this permission on the path's project.
    Project(ProjectPermission),
}

impl Access {
    /// The matrix cell.
    pub fn describe(self) -> String {
        match self {
            Self::Public => "public".to_owned(),
            Self::Session => "any session".to_owned(),
            Self::Org(p) => format!("org `{}`", p.name()),
            Self::Project(p) => format!("project `{}`", p.name()),
        }
    }
}

/// A route and who may call it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RouteAccess {
    /// The method, upper case.
    pub method: &'static str,
    /// The path as the OpenAPI document writes it.
    pub path: &'static str,
    /// Who may call it.
    pub access: Access,
}

const fn route(method: &'static str, path: &'static str, access: Access) -> RouteAccess {
    RouteAccess {
        method,
        path,
        access,
    }
}

use Access::{Org, Project, Public, Session};

/// Every route under `/api/v1`, ordered by path then method.
pub const ROUTES: &[RouteAccess] = &[
    route("GET", "/api/v1/account", Session),
    route("PATCH", "/api/v1/account", Session),
    route("POST", "/api/v1/auth/invitations/accept", Public),
    route("POST", "/api/v1/auth/login", Public),
    route("POST", "/api/v1/auth/logout", Session),
    route("GET", "/api/v1/auth/oauth/{provider_id}/callback", Public),
    route("GET", "/api/v1/auth/oauth/{provider_id}/start", Public),
    route("POST", "/api/v1/auth/password/change", Session),
    route("POST", "/api/v1/auth/password/reset-confirm", Public),
    route("POST", "/api/v1/auth/password/reset-request", Public),
    route("GET", "/api/v1/auth/providers", Public),
    route("GET", "/api/v1/auth/session", Session),
    route("POST", "/api/v1/auth/setup", Public),
    route("GET", "/api/v1/org", Org(OrgPermission::Read)),
    route("PATCH", "/api/v1/org", Org(OrgPermission::Settings)),
    route(
        "GET",
        "/api/v1/org/auth-providers",
        Org(OrgPermission::Settings),
    ),
    route(
        "POST",
        "/api/v1/org/auth-providers",
        Org(OrgPermission::Settings),
    ),
    route(
        "DELETE",
        "/api/v1/org/auth-providers/{provider_id}",
        Org(OrgPermission::Settings),
    ),
    route(
        "PATCH",
        "/api/v1/org/auth-providers/{provider_id}",
        Org(OrgPermission::Settings),
    ),
    route("GET", "/api/v1/org/invitations", Org(OrgPermission::Read)),
    route(
        "POST",
        "/api/v1/org/invitations",
        Org(OrgPermission::Members),
    ),
    route(
        "DELETE",
        "/api/v1/org/invitations/{invitation_id}",
        Org(OrgPermission::Members),
    ),
    route("GET", "/api/v1/org/members", Org(OrgPermission::Read)),
    route(
        "DELETE",
        "/api/v1/org/members/{user_id}",
        Org(OrgPermission::Members),
    ),
    route(
        "PATCH",
        "/api/v1/org/members/{user_id}",
        Org(OrgPermission::Members),
    ),
    route(
        "POST",
        "/api/v1/org/transfer-ownership",
        Org(OrgPermission::TransferOwnership),
    ),
    route("GET", "/api/v1/projects", Session),
    route("POST", "/api/v1/projects", Org(OrgPermission::Projects)),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}",
        Org(OrgPermission::Projects),
    ),
    route("GET", "/api/v1/projects/{project_id}", Session),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}",
        Project(ProjectPermission::Manage),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/alerts",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/alerts",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/alerts/preview",
        Project(ProjectPermission::Read),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/alerts/{alert_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/alerts/{alert_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}/alerts/{alert_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/alerts/{alert_id}/history",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/api-keys",
        Project(ProjectPermission::Manage),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/api-keys",
        Project(ProjectPermission::Manage),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/api-keys/{key_id}",
        Project(ProjectPermission::Manage),
    ),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}/api-keys/{key_id}",
        Project(ProjectPermission::Manage),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/channels",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/channels",
        Project(ProjectPermission::Channels),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/channels/{channel_id}",
        Project(ProjectPermission::Channels),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/channels/{channel_id}",
        Project(ProjectPermission::Channels),
    ),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}/channels/{channel_id}",
        Project(ProjectPermission::Channels),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/channels/{channel_id}/test",
        Project(ProjectPermission::Channels),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/dashboards",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/dashboards",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/dashboards/{dashboard_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/dashboards/{dashboard_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}/dashboards/{dashboard_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/duplicate",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/widgets",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/widgets/{widget_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}/dashboards/{dashboard_id}/widgets/{widget_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/data-bounds",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/deletion-requests",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/deletion-requests",
        Project(ProjectPermission::DeleteTraces),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/deletion-requests/{request_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/exports",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/exports",
        Project(ProjectPermission::Export),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/exports/{export_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/exports/{export_id}/download",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/corrections",
        Project(ProjectPermission::WriteFacts),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/facts",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/feedback",
        Project(ProjectPermission::WriteFacts),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/filter-values",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/flags",
        Project(ProjectPermission::WriteFacts),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/members",
        Project(ProjectPermission::Manage),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/members",
        Project(ProjectPermission::Manage),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/members/{user_id}",
        Project(ProjectPermission::Manage),
    ),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}/members/{user_id}",
        Project(ProjectPermission::Manage),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/metrics/query",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/metrics/schema",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/retention",
        Project(ProjectPermission::Read),
    ),
    route(
        "PUT",
        "/api/v1/projects/{project_id}/retention",
        Project(ProjectPermission::Manage),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/search",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/sessions",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/sessions/{session_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/spans",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/spans/{trace_id}/{span_id}/payload",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/sql/export",
        Project(ProjectPermission::RunSql),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/sql/run",
        Project(ProjectPermission::RunSql),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/sql/runs",
        Project(ProjectPermission::Read),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/sql/runs/{run_id}",
        Project(ProjectPermission::RunSavedQuery),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/sql/saved-queries",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/sql/saved-queries",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "DELETE",
        "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "PATCH",
        "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}",
        Project(ProjectPermission::EditContent),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/sql/saved-queries/{query_id}/run",
        Project(ProjectPermission::RunSavedQuery),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/sql/schema",
        Project(ProjectPermission::Read),
    ),
    route(
        "POST",
        "/api/v1/projects/{project_id}/tags",
        Project(ProjectPermission::WriteFacts),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/traces",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/traces/{trace_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/traces/{trace_id}/spans",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/users",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/users/{user_id}",
        Project(ProjectPermission::Read),
    ),
    route(
        "GET",
        "/api/v1/projects/{project_id}/users/{user_id}/sessions",
        Project(ProjectPermission::Read),
    ),
];

/// The authorization matrix as Markdown, as committed at `docs/architecture/authz-matrix.md`.
pub fn authz_matrix() -> String {
    let mut out = String::from(
        "# Authorization matrix\n\n\
         Generated by `cargo xtask authz-matrix` from `crates/crucible-api/src/authz.rs`; do not edit. A test fails \
         when a route under `/api/v1` has no entry, and the route walk checks that each route refuses callers \
         without its permission (contract 11.6).\n\n\
         ## Routes\n\n\
         | Method | Path | Who may call it |\n|---|---|---|\n",
    );
    for route in ROUTES {
        out.push_str(&format!(
            "| {} | `{}` | {} |\n",
            route.method,
            route.path.trim_start_matches("/api/v1/"),
            route.access.describe()
        ));
    }
    out.push_str(
        "\n## Project permissions\n\n\
         | Permission | Allows | Lowest project role | API key scope |\n|---|---|---|---|\n",
    );
    for p in ProjectPermission::ALL {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            p.name(),
            p.describe(),
            p.min_role().map_or("none (keys only)", |r| r.as_str()),
            p.key_scope().map_or("none (keys never)", |s| s.as_str()),
        ));
    }
    out.push_str(
        "\n## Org permissions\n\n| Permission | Allows | Lowest org role |\n|---|---|---|\n",
    );
    for p in OrgPermission::ALL {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            p.name(),
            p.describe(),
            p.min_role().as_str()
        ));
    }
    out.push_str(
        "\nOrg owners and admins are project admins on every project. With `project_access = all` (the default) \
         org members and viewers have their org role on every project; with `explicit`, only through a project \
         membership. The effective project role is the higher of the two. API keys are project principals: they \
         never manage keys, members or settings.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn every_documented_operation_has_an_entry_and_every_entry_an_operation() {
        let doc = crate::openapi();
        let operations: BTreeSet<(String, String)> = doc
            .paths
            .paths
            .iter()
            .flat_map(|(path, item)| {
                [
                    ("GET", item.get.is_some()),
                    ("POST", item.post.is_some()),
                    ("PATCH", item.patch.is_some()),
                    ("PUT", item.put.is_some()),
                    ("DELETE", item.delete.is_some()),
                ]
                .into_iter()
                .filter(|(_, present)| *present)
                .map(move |(method, _)| (method.to_owned(), path.clone()))
            })
            .collect();
        let table: BTreeSet<(String, String)> = ROUTES
            .iter()
            .map(|r| (r.method.to_owned(), r.path.to_owned()))
            .collect();
        assert_eq!(table.len(), ROUTES.len(), "a route is listed twice");
        let missing: Vec<_> = operations.difference(&table).collect();
        let stale: Vec<_> = table.difference(&operations).collect();
        assert!(
            missing.is_empty(),
            "routes without an authorization entry: {missing:?}"
        );
        assert!(stale.is_empty(), "entries without a route: {stale:?}");
    }

    #[test]
    fn the_committed_matrix_is_current() {
        let committed = include_str!("../../../docs/architecture/authz-matrix.md");
        assert_eq!(
            committed.replace("\r\n", "\n"),
            authz_matrix(),
            "run `cargo xtask authz-matrix`"
        );
    }
}
