//! Roles, project access and the capability matrix (contract 11.6), and the key scopes of contract 11.3.
//!
//! The matrix here is the single source: the permission extractor checks it, and `cargo xtask authz-matrix`
//! renders it into `docs/architecture/authz-matrix.md`.

use crucible_control::{KeyScope, OrgRole, ProjectAccess, ProjectRole};

/// What a request does to a project (contract 11.6). Each is checked against the caller's effective project
/// role, or, for an API key, against the key's scopes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ProjectPermission {
    /// Read telemetry, MCP analytics, dashboards, alerts and saved queries.
    Read,
    /// Send telemetry (API keys only).
    Ingest,
    /// Write tags, flags, comments, feedback and corrections.
    WriteFacts,
    /// Create and edit dashboards, saved queries and alert rules.
    EditContent,
    /// Run ad hoc SQL.
    RunSql,
    /// Create exports.
    Export,
    /// Run a project-visible saved query with its declared parameters.
    RunSavedQuery,
    /// Manage channels (they hold secrets) and test-send.
    Channels,
    /// Manage API keys, project settings, retention, model prices and project members.
    Manage,
    /// Delete traces.
    DeleteTraces,
    /// Delete by user or session.
    DeleteByUser,
}

impl ProjectPermission {
    /// Every permission, in matrix order.
    pub const ALL: &'static [Self] = &[
        Self::Read,
        Self::Ingest,
        Self::WriteFacts,
        Self::EditContent,
        Self::RunSql,
        Self::Export,
        Self::RunSavedQuery,
        Self::Channels,
        Self::Manage,
        Self::DeleteTraces,
        Self::DeleteByUser,
    ];

    /// The stable name used in the authorization matrix and in route annotations.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Ingest => "ingest",
            Self::WriteFacts => "write_facts",
            Self::EditContent => "edit_content",
            Self::RunSql => "run_sql",
            Self::Export => "export",
            Self::RunSavedQuery => "run_saved_query",
            Self::Channels => "channels",
            Self::Manage => "manage",
            Self::DeleteTraces => "delete_traces",
            Self::DeleteByUser => "delete_by_user",
        }
    }

    /// What the permission allows, for the documentation table.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Read => "Read telemetry, MCP analytics, dashboards, alerts, saved queries",
            Self::Ingest => "Send telemetry",
            Self::WriteFacts => "Write tags, flags, comments, feedback, corrections",
            Self::EditContent => "Create and edit dashboards, saved queries, alert rules",
            Self::RunSql => "Run ad hoc SQL",
            Self::Export => "Create exports",
            Self::RunSavedQuery => "Run a project-visible saved query (declared parameters only)",
            Self::Channels => "Channels (they hold secrets), test-send",
            Self::Manage => "API keys, project settings, retention, model prices, project members",
            Self::DeleteTraces => "Delete traces",
            Self::DeleteByUser => "Delete by user or session",
        }
    }

    /// The lowest project role that has the permission; `None` when no role has it (ingest is for keys).
    pub const fn min_role(self) -> Option<ProjectRole> {
        match self {
            Self::Read | Self::RunSavedQuery => Some(ProjectRole::Viewer),
            Self::WriteFacts
            | Self::EditContent
            | Self::RunSql
            | Self::Export
            | Self::DeleteTraces => Some(ProjectRole::Member),
            Self::Channels | Self::Manage | Self::DeleteByUser => Some(ProjectRole::Admin),
            Self::Ingest => None,
        }
    }

    /// The scope an API key needs for the permission; `None` when keys never have it (contract 11.3: a key can
    /// never manage keys, members or settings).
    pub const fn key_scope(self) -> Option<KeyScope> {
        match self {
            Self::Read | Self::Export => Some(KeyScope::Read),
            Self::Ingest => Some(KeyScope::Ingest),
            Self::WriteFacts => Some(KeyScope::Write),
            Self::RunSql | Self::RunSavedQuery => Some(KeyScope::Sql),
            Self::DeleteTraces | Self::DeleteByUser => Some(KeyScope::Delete),
            Self::EditContent | Self::Channels | Self::Manage => None,
        }
    }

    /// Whether a member with `role` on the project has the permission.
    pub fn allowed_for(self, role: ProjectRole) -> bool {
        self.min_role()
            .is_some_and(|min| project_rank(role) >= project_rank(min))
    }

    /// Whether a key with `scopes` has the permission.
    pub fn allowed_for_key(self, scopes: &[KeyScope]) -> bool {
        self.key_scope()
            .is_some_and(|scope| scopes.contains(&scope))
    }
}

/// What a request does to the organization (contract 11.6).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum OrgPermission {
    /// Read the organization and list its members and invitations.
    Read,
    /// Org settings, sign-in providers and the password-login switch.
    Settings,
    /// Change members and invitations; an admin cannot change owners or make one.
    Members,
    /// Create or delete projects.
    Projects,
    /// Transfer ownership.
    TransferOwnership,
}

impl OrgPermission {
    /// Every permission, in matrix order.
    pub const ALL: &'static [Self] = &[
        Self::Read,
        Self::Settings,
        Self::Members,
        Self::Projects,
        Self::TransferOwnership,
    ];

    /// The stable name used in the authorization matrix and in route annotations.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "org_read",
            Self::Settings => "org_settings",
            Self::Members => "org_members",
            Self::Projects => "org_projects",
            Self::TransferOwnership => "org_transfer_ownership",
        }
    }

    /// What the permission allows, for the documentation table.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Read => "Read the organization; list members and invitations",
            Self::Settings => "Org settings, sign-in providers, password-login switch",
            Self::Members => {
                "Change members and invitations (an admin cannot change or make owners)"
            }
            Self::Projects => "Create or delete projects",
            Self::TransferOwnership => "Transfer ownership",
        }
    }

    /// The lowest org role that has the permission.
    pub const fn min_role(self) -> OrgRole {
        match self {
            Self::Read => OrgRole::Viewer,
            Self::Settings | Self::Members | Self::Projects => OrgRole::Admin,
            Self::TransferOwnership => OrgRole::Owner,
        }
    }

    /// Whether a member with `role` has the permission.
    pub fn allowed_for(self, role: OrgRole) -> bool {
        org_rank(role) >= org_rank(self.min_role())
    }
}

/// Why a project request is refused (contract 11.6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Denied {
    /// The caller cannot see the project, or a key names another project: 404 `not_found`.
    NotVisible,
    /// The project is visible but the role or scope is too low: 403 `forbidden`.
    TooLow,
}

/// The rank of an org role: higher is more.
pub const fn org_rank(role: OrgRole) -> u8 {
    match role {
        OrgRole::Owner => 3,
        OrgRole::Admin => 2,
        OrgRole::Member => 1,
        OrgRole::Viewer => 0,
    }
}

/// The rank of a project role: higher is more.
pub const fn project_rank(role: ProjectRole) -> u8 {
    match role {
        ProjectRole::Admin => 2,
        ProjectRole::Member => 1,
        ProjectRole::Viewer => 0,
    }
}

/// The effective project role of an org member (contract 11.6): org owners and admins are project admin
/// everywhere; for members and viewers, `project_access = all` gives their org role on every project and
/// `explicit` gives nothing; the higher of that and the project membership wins. `None` means the project is
/// not visible to them.
pub fn effective_project_role(
    org_role: OrgRole,
    access: ProjectAccess,
    membership: Option<ProjectRole>,
) -> Option<ProjectRole> {
    let from_org = match (org_role, access) {
        (OrgRole::Owner | OrgRole::Admin, _) => Some(ProjectRole::Admin),
        (OrgRole::Member, ProjectAccess::All) => Some(ProjectRole::Member),
        (OrgRole::Viewer, ProjectAccess::All) => Some(ProjectRole::Viewer),
        (OrgRole::Member | OrgRole::Viewer, ProjectAccess::Explicit) => None,
    };
    match (from_org, membership) {
        (Some(a), Some(b)) => Some(if project_rank(a) >= project_rank(b) {
            a
        } else {
            b
        }),
        (a, b) => a.or(b),
    }
}

/// The permissions a project role has, by name, for the session DTO (contract 12.7).
pub fn permission_names(role: ProjectRole) -> Vec<String> {
    ProjectPermission::ALL
        .iter()
        .filter(|permission| permission.allowed_for(role))
        .map(|permission| permission.name().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_matrix_is_the_contracts() {
        use ProjectPermission::*;
        let row = |role| {
            ProjectPermission::ALL
                .iter()
                .filter(|p| p.allowed_for(role))
                .copied()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            row(ProjectRole::Viewer),
            [Read, RunSavedQuery],
            "viewers read and run project-visible saved queries"
        );
        assert_eq!(
            row(ProjectRole::Member),
            [
                Read,
                WriteFacts,
                EditContent,
                RunSql,
                Export,
                RunSavedQuery,
                DeleteTraces
            ]
        );
        assert_eq!(
            row(ProjectRole::Admin),
            ProjectPermission::ALL
                .iter()
                .copied()
                .filter(|p| *p != Ingest)
                .collect::<Vec<_>>()
        );
        // Keys never manage keys, members or settings, and never edit content or channels.
        for p in [Manage, Channels, EditContent] {
            assert!(!p.allowed_for_key(KeyScope::ALL), "{p:?}");
        }
        assert!(Ingest.allowed_for_key(&[KeyScope::Ingest]));
        assert!(!Read.allowed_for_key(&[KeyScope::Ingest]));
        assert!(Export.allowed_for_key(&[KeyScope::Read]));
        assert!(RunSql.allowed_for_key(&[KeyScope::Sql]));
        assert!(DeleteByUser.allowed_for_key(&[KeyScope::Delete]));
    }

    #[test]
    fn the_org_matrix_is_the_contracts() {
        use OrgPermission::*;
        for role in [OrgRole::Member, OrgRole::Viewer] {
            assert!(Read.allowed_for(role));
            assert!(
                !Settings.allowed_for(role)
                    && !Members.allowed_for(role)
                    && !Projects.allowed_for(role)
            );
        }
        assert!(Settings.allowed_for(OrgRole::Admin) && Members.allowed_for(OrgRole::Admin));
        assert!(!TransferOwnership.allowed_for(OrgRole::Admin));
        assert!(
            OrgPermission::ALL
                .iter()
                .all(|p| p.allowed_for(OrgRole::Owner))
        );
    }

    #[test]
    fn the_effective_role_is_the_higher_of_org_and_membership() {
        use ProjectAccess::{All, Explicit};
        let (admin, member, viewer) =
            (ProjectRole::Admin, ProjectRole::Member, ProjectRole::Viewer);
        for access in [All, Explicit] {
            for membership in [None, Some(viewer)] {
                assert_eq!(
                    effective_project_role(OrgRole::Owner, access, membership),
                    Some(admin)
                );
                assert_eq!(
                    effective_project_role(OrgRole::Admin, access, membership),
                    Some(admin)
                );
            }
        }
        assert_eq!(
            effective_project_role(OrgRole::Member, All, None),
            Some(member)
        );
        assert_eq!(
            effective_project_role(OrgRole::Member, All, Some(viewer)),
            Some(member)
        );
        assert_eq!(
            effective_project_role(OrgRole::Viewer, All, Some(admin)),
            Some(admin)
        );
        assert_eq!(
            effective_project_role(OrgRole::Viewer, All, None),
            Some(viewer)
        );
        assert_eq!(
            effective_project_role(OrgRole::Member, Explicit, None),
            None
        );
        assert_eq!(
            effective_project_role(OrgRole::Viewer, Explicit, Some(member)),
            Some(member)
        );
        assert_eq!(
            permission_names(viewer),
            ["read".to_owned(), "run_saved_query".to_owned()]
        );
    }
}
