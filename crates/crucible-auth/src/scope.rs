//! The one place that mints `ProjectScope` values (contract section 1.4).
//!
//! `ProjectScope::mint` is a disallowed method everywhere else; the CI allow-attribute guard permits the allow
//! below in this file only.

use crucible_records::{NilProjectId, ProjectId, ProjectScope};

/// Mints a scope for a project id that an authentication path has verified.
#[allow(clippy::disallowed_methods)] // The sanctioned call site of ProjectScope::mint.
pub(crate) fn mint(project_id: ProjectId) -> Result<ProjectScope, NilProjectId> {
    ProjectScope::mint(project_id)
}

/// The scope of a background job that works on a project without a request: an alert evaluation reads the
/// project's rollups. The caller shows the project row it read from the control store; a deleted project and
/// the nil project have no scope.
pub fn job_scope(project: &crucible_control::Project) -> Option<ProjectScope> {
    if project.deleted_at.is_some() {
        return None;
    }
    mint(project.id).ok()
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
