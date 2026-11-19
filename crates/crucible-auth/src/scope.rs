//! The one place that mints `ProjectScope` values (contract section 1.4).
//!
//! `ProjectScope::mint` is a disallowed method everywhere else; the CI allow-attribute guard permits the allow
//! below in this file only.

use crucible_control::Project;
use crucible_records::{NilProjectId, ProjectId, ProjectScope};

/// Mints a scope for a project id that an authentication path has verified.
#[allow(clippy::disallowed_methods)] // The sanctioned call site of ProjectScope::mint.
pub(crate) fn mint(project_id: ProjectId) -> Result<ProjectScope, NilProjectId> {
    ProjectScope::mint(project_id)
}

/// The scope of a project row read from the control store, for the background jobs that act on every stored
/// project without a request (the retention and deletion executors, contract 13). The row is the witness: a
/// job cannot scope itself to a project the control plane does not hold.
pub fn stored_project_scope(project: &Project) -> Result<ProjectScope, NilProjectId> {
    mint(project.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_the_nil_project() {
        assert_eq!(mint(ProjectId::from_bytes([0; 16])), Err(NilProjectId));
        let id = ProjectId::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b").unwrap();
        assert_eq!(mint(id).unwrap().project_id(), id);
    }
}
