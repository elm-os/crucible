//! Who may do what in the SQL workspace: the SQL rows of the capability matrix (contract 11.6), as one pure
//! function.

use crucible_control::{SavedQuery, SqlActor, SqlVisibility};

/// The caller's effective project role.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SqlRole {
    /// Reads, and runs project-visible saved queries with their declared parameters.
    Viewer,
    /// Also runs ad hoc SQL, exports, and creates and edits saved queries.
    Member,
    /// Everything a member may.
    Admin,
}

/// What a caller wants to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SqlAction<'a> {
    /// Read the schema browser, the workspace status and the run history.
    Read,
    /// Run ad hoc SQL.
    RunAdHoc,
    /// Export a result.
    Export,
    /// Cancel a run that `runner` started.
    Cancel {
        /// Who started the run.
        runner: SqlActor,
    },
    /// See a saved query.
    ViewSaved(&'a SavedQuery),
    /// Run a saved query with its declared parameters.
    RunSaved(&'a SavedQuery),
    /// Create a saved query.
    CreateSaved,
    /// Change or delete a saved query.
    EditSaved(&'a SavedQuery),
}

/// Whether `actor`, with `role` on the project, may do `action`.
///
/// - Everyone who can read the project reads the schema and the history and sees project-visible saved
///   queries; a private saved query is its owner's alone.
/// - A viewer runs project-visible saved queries and nothing else.
/// - Members and admins run ad hoc SQL, export, and create saved queries. They edit their own saved queries;
///   an admin also edits any project-visible one.
/// - A run is cancelled by whoever started it, or by an admin.
pub fn allowed(role: SqlRole, actor: SqlActor, action: SqlAction<'_>) -> bool {
    let writer = role >= SqlRole::Member;
    let visible =
        |query: &SavedQuery| query.visibility == SqlVisibility::Project || query.owner == actor;
    match action {
        SqlAction::Read => true,
        SqlAction::RunAdHoc | SqlAction::Export | SqlAction::CreateSaved => writer,
        SqlAction::Cancel { runner } => runner == actor || role == SqlRole::Admin,
        SqlAction::ViewSaved(query) => visible(query),
        SqlAction::RunSaved(query) => {
            visible(query) && (writer || query.visibility == SqlVisibility::Project)
        }
        SqlAction::EditSaved(query) => {
            writer
                && (query.owner == actor
                    || (role == SqlRole::Admin && query.visibility == SqlVisibility::Project))
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::{ApiKeyId, ProjectId, SavedQueryId, Timestamp, UserId, Uuid};
    use proptest::prelude::*;

    use super::*;

    fn user(n: u8) -> SqlActor {
        SqlActor::User(UserId::from_uuid(Uuid::from_bytes([n; 16])))
    }

    fn query(owner: SqlActor, visibility: SqlVisibility) -> SavedQuery {
        SavedQuery {
            id: SavedQueryId::from_uuid(Uuid::from_bytes([9; 16])),
            project_id: ProjectId::from_uuid(Uuid::from_bytes([1; 16])),
            owner,
            name: "q".to_owned(),
            description: String::new(),
            visibility,
            sql: "SELECT 1".to_owned(),
            parameters: Vec::new(),
            version: 1,
            created_at: Timestamp::from_micros(0),
            updated_at: Timestamp::from_micros(0),
        }
    }

    #[test]
    fn the_matrix_rows_of_contract_11_6() {
        let (me, other) = (user(1), user(2));
        let shared = query(other, SqlVisibility::Project);
        let private = query(other, SqlVisibility::Private);
        let mine = query(me, SqlVisibility::Private);
        for role in [SqlRole::Admin, SqlRole::Member] {
            assert!(allowed(role, me, SqlAction::RunAdHoc));
            assert!(allowed(role, me, SqlAction::Export));
            assert!(allowed(role, me, SqlAction::CreateSaved));
            assert!(allowed(role, me, SqlAction::RunSaved(&shared)));
            assert!(allowed(role, me, SqlAction::EditSaved(&mine)));
        }
        // A viewer: only project-visible saved queries.
        assert!(!allowed(SqlRole::Viewer, me, SqlAction::RunAdHoc));
        assert!(!allowed(SqlRole::Viewer, me, SqlAction::Export));
        assert!(!allowed(SqlRole::Viewer, me, SqlAction::CreateSaved));
        assert!(allowed(SqlRole::Viewer, me, SqlAction::RunSaved(&shared)));
        assert!(allowed(SqlRole::Viewer, me, SqlAction::ViewSaved(&shared)));
        assert!(allowed(SqlRole::Viewer, me, SqlAction::Read));
        assert!(!allowed(SqlRole::Viewer, me, SqlAction::EditSaved(&shared)));
        // A viewer's own private query (kept from a time with a higher role) stays visible and is not runnable.
        assert!(allowed(SqlRole::Viewer, me, SqlAction::ViewSaved(&mine)));
        assert!(!allowed(SqlRole::Viewer, me, SqlAction::RunSaved(&mine)));
        // Private queries are their owner's alone, whatever the role.
        for role in [SqlRole::Admin, SqlRole::Member, SqlRole::Viewer] {
            assert!(!allowed(role, me, SqlAction::ViewSaved(&private)));
            assert!(!allowed(role, me, SqlAction::RunSaved(&private)));
            assert!(!allowed(role, me, SqlAction::EditSaved(&private)));
        }
        // Editing another person's shared query takes an admin.
        assert!(allowed(SqlRole::Admin, me, SqlAction::EditSaved(&shared)));
        assert!(!allowed(SqlRole::Member, me, SqlAction::EditSaved(&shared)));
        // Cancelling.
        assert!(allowed(
            SqlRole::Member,
            me,
            SqlAction::Cancel { runner: me }
        ));
        assert!(!allowed(
            SqlRole::Member,
            me,
            SqlAction::Cancel { runner: other }
        ));
        assert!(allowed(
            SqlRole::Admin,
            me,
            SqlAction::Cancel { runner: other }
        ));
    }

    proptest! {
        /// A higher role never loses a capability, and nobody but the owner ever reaches a private query.
        #[test]
        fn roles_are_monotonic_and_private_stays_private(
            actor in 0u8..3, owner in 0u8..3, project_visible: bool, action in 0u8..7, key_owner: bool,
        ) {
            let me = user(actor);
            let owner = if key_owner {
                SqlActor::ApiKey(ApiKeyId::from_uuid(Uuid::from_bytes([owner; 16])))
            } else {
                user(owner)
            };
            let visibility = if project_visible { SqlVisibility::Project } else { SqlVisibility::Private };
            let saved = query(owner, visibility);
            let action = match action {
                0 => SqlAction::Read,
                1 => SqlAction::RunAdHoc,
                2 => SqlAction::Export,
                3 => SqlAction::ViewSaved(&saved),
                4 => SqlAction::RunSaved(&saved),
                5 => SqlAction::CreateSaved,
                _ => SqlAction::EditSaved(&saved),
            };
            let (viewer, member, admin) = (
                allowed(SqlRole::Viewer, me, action),
                allowed(SqlRole::Member, me, action),
                allowed(SqlRole::Admin, me, action),
            );
            prop_assert!(!viewer || member);
            prop_assert!(!member || admin);
            let touches_saved = matches!(action, SqlAction::ViewSaved(_) | SqlAction::RunSaved(_) | SqlAction::EditSaved(_));
            if touches_saved && !project_visible && owner != me {
                prop_assert!(!admin);
            }
        }
    }
}
