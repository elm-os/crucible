//! Scopes for tests in other crates. Enabled by the `test-support` feature, which product builds never enable.

use crucible_records::{NilProjectId, ProjectId, ProjectScope};

/// A scope for `project_id`, for tests only.
pub fn scope(project_id: ProjectId) -> Result<ProjectScope, NilProjectId> {
    crate::scope::mint(project_id)
}
