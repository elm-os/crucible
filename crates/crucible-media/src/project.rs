//! The stored objects of a deleted project (contract 13.4): project deletion removes every object under the
//! project's key prefix, `<prefix>/<project_id>/`, once the project's media rows are gone.

use crucible_records::{ProjectId, ProjectScope};

use crate::rules::key_root;
use crate::service::{Media, MediaError};

/// The most keys one batch delete names (contract 2.10).
const DELETE_BATCH: usize = 1_000;

/// Where every key of a project starts.
fn project_root(prefix: &str, project_id: ProjectId) -> String {
    format!("{}{project_id}/", key_root(prefix))
}

impl Media {
    /// Deletes every stored object under the project's key prefix, a listing page at a time, and returns how
    /// many keys it deleted. It runs once the project's media rows are gone, as the collector deletes a row
    /// before its object. A listing may lag behind recent writes (contract 2.10), so an object stored moments
    /// before can be missed, and an upload through a presigned URL handed out before the deletion can land
    /// after it; neither has a row, so the weekly orphan scan deletes them.
    pub async fn remove_project(&self, scope: ProjectScope) -> Result<u64, MediaError> {
        let root = project_root(&self.config().prefix, scope.project_id());
        let mut after: Option<String> = None;
        let mut deleted = 0;
        loop {
            let page = self.objects.list(&root, after.as_deref()).await?;
            let keys: Vec<String> = page.objects.into_iter().map(|o| o.key).collect();
            for chunk in keys.chunks(DELETE_BATCH) {
                self.objects.delete_batch(chunk).await?;
                deleted += chunk.len() as u64;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(deleted),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::{MediaObjectId, Uuid};
    use proptest::prelude::*;

    use super::*;
    use crate::rules::object_key;

    proptest! {
        /// Every key of a project starts with the project's root, and no key of another project does.
        #[test]
        fn a_project_root_holds_its_keys_only(
            prefix in "[a-z0-9-]{0,12}",
            millis in 0u64..4_102_444_800_000,
            rand: u128,
            project: [u8; 16],
            other: [u8; 16],
        ) {
            let (project, other) = (ProjectId::from_bytes(project), ProjectId::from_bytes(other));
            let id = MediaObjectId::from_uuid(Uuid::from_v7_parts(millis, rand));
            prop_assert!(object_key(&prefix, project, id).starts_with(&project_root(&prefix, project)));
            prop_assert_eq!(
                object_key(&prefix, other, id).starts_with(&project_root(&prefix, project)),
                other == project
            );
        }
    }
}
