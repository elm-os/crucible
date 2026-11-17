//! `ProjectScope` (contract section 1.4).

use crate::id::ProjectId;

/// The nil project id cannot be scoped: it is reserved for marker rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the nil project id cannot be scoped")]
pub struct NilProjectId;

/// The only value that authorizes reading or writing project data.
///
/// The type lives in this leaf crate so that `crucible-plan` and the `Env` store traits can name it. Minting is
/// restricted instead of private: [`ProjectScope::mint`] is a disallowed method in `clippy.toml`, and the CI
/// allow-attribute guard permits the allow in exactly one file, `crates/crucible-auth/src/scope.rs`. Tests in
/// other crates obtain scopes from `crucible_auth::test_support`. There is no `Default` and no deserializer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProjectScope {
    project_id: ProjectId,
}

impl ProjectScope {
    /// Mints a scope. Restricted to `crucible-auth`; refuses the nil project id.
    pub const fn mint(project_id: ProjectId) -> Result<Self, NilProjectId> {
        if is_nil(project_id) {
            Err(NilProjectId)
        } else {
            Ok(Self { project_id })
        }
    }

    /// The project this scope authorizes.
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }
}

const fn is_nil(id: ProjectId) -> bool {
    let bytes = id.to_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0 {
            return false;
        }
        i += 1;
    }
    true
}
