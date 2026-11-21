//! Export files on the object store (contract 12.8), when the deployment has one: the file of an attempt is the
//! object `<prefix>/<project>/exports/<export>/<attempt>/crucible-export-<export>.<extension>`, under the
//! project's key prefix, so that every node can hand out a presigned link to it and the browser saves it under
//! its own name.
//!
//! - **Writing.** An attempt holds its file's bytes until they make a part of [`PART_BYTES`], then uploads the
//!   parts of a multipart upload; a file that never fills a part is stored with one request at the commit. Either
//!   way the object appears whole at the commit and never in part, so a crash leaves no object, at most an upload
//!   the store holds until the bucket's lifecycle rule removes it.
//! - **Reading.** A download is a presigned GET that lives [`DOWNLOAD_TTL`] and saves the file as an attachment.
//! - **Removing.** [`sweep_objects`] removes the objects that no export needs, as the disk sweep does on each node,
//!   run by one node at a time ([`crate::object_sweep_job`]); [`ExportObjects::remove`] removes the objects of the
//!   exports a deletion request expired.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crucible_control::{ExportJob, Repos};
use crucible_env::{Disposition, MIN_PART_BYTES, ObjectError, ObjectStore, Presigned};
use crucible_records::{ExportJobId, ProjectId, Timestamp};

use crate::files::{FilesError, Named, extension, stale};

/// How long a presigned download of an export file lives (contract 13.5: at most 5 minutes).
pub const DOWNLOAD_TTL: Duration = Duration::from_secs(300);
/// The bytes of one part of a multipart upload.
pub const PART_BYTES: usize = 8 << 20;
/// How a row's file reference names an object: this marker and the object's key.
const OBJECT_REF: &str = "object:";
/// The most keys one batch delete names.
const DELETE_BATCH: usize = 1_000;

// A part is never smaller than the store allows.
const _: () = assert!(PART_BYTES >= MIN_PART_BYTES);

/// The object key a row's file reference names, or `None` for a file on a node's disk.
pub(crate) fn object_of(file: &str) -> Option<&str> {
    file.strip_prefix(OBJECT_REF)
}

/// The content type of a file: what the disk download sends, and what the object is stored with.
pub(crate) const fn content_type(gzip: bool) -> &'static str {
    if gzip {
        "application/gzip"
    } else {
        "application/x-ndjson"
    }
}

/// The object store of a deployment and its key prefix (`object_store.prefix`): where export files go when the
/// deployment has one.
#[derive(Clone)]
pub struct ExportObjects {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl fmt::Debug for ExportObjects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportObjects")
            .field("store", &self.store)
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl ExportObjects {
    /// Export files on `store`, under `prefix`.
    pub fn new(store: Arc<dyn ObjectStore>, prefix: impl Into<String>) -> Self {
        Self {
            store,
            prefix: prefix.into(),
        }
    }

    /// Where every key of a project starts: its export files and its media.
    fn project_root(&self, project_id: ProjectId) -> String {
        let lead = if self.prefix.is_empty() { "" } else { "/" };
        format!("{}{lead}{project_id}/", self.prefix)
    }

    /// Where every export key of a project starts.
    fn project_dir(&self, project_id: ProjectId) -> String {
        format!("{}exports/", self.project_root(project_id))
    }

    /// The key of an attempt's file.
    fn key(&self, job: &ExportJob, attempt: u32, gzip: bool) -> String {
        let id = job.new.id;
        format!(
            "{}{id}/{attempt}/crucible-export-{id}.{}",
            self.project_dir(job.new.project_id),
            extension(job.new.format, gzip)
        )
    }

    /// What a key under a project's export keys says; `None` for any other key, which the sweep leaves alone.
    fn named(&self, project_id: ProjectId, key: &str) -> Option<Named> {
        let rest = key.strip_prefix(&self.project_dir(project_id))?;
        let mut parts = rest.split('/');
        let id = ExportJobId::parse(parts.next()?).ok()?;
        let attempt = parts.next()?.parse().ok()?;
        let name = parts.next()?;
        let expected = format!("crucible-export-{id}.");
        (name.starts_with(&expected) && parts.next().is_none()).then_some(Named {
            id,
            attempt,
            temporary: false,
        })
    }

    /// Deletes one object. Deleting a key that holds nothing succeeds.
    pub(crate) async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        self.store.delete(key).await
    }

    /// A presigned download of a row's object, saved as an attachment.
    pub(crate) fn link(&self, key: &str) -> Result<Presigned, ObjectError> {
        self.store
            .presign_get(key, Disposition::Attachment, DOWNLOAD_TTL)
    }

    /// The writer of an attempt's file.
    pub(crate) fn draft(&self, job: &ExportJob, attempt: u32, gzip: bool) -> ObjectDraft {
        ObjectDraft {
            store: Arc::clone(&self.store),
            key: self.key(job, attempt, gzip),
            content_type: content_type(gzip),
            held: Vec::new(),
            upload: None,
        }
    }

    /// Removes the objects of `jobs`' files, for example the exports a deletion request expired, and returns how
    /// many it removed. Files on a node's disk are left to that node. A file that is already gone counts as removed.
    pub async fn remove(&self, jobs: &[ExportJob]) -> Result<u64, ObjectError> {
        let keys: Vec<String> = jobs
            .iter()
            .filter_map(|job| job.file.as_deref().and_then(object_of))
            .map(str::to_owned)
            .collect();
        for batch in keys.chunks(DELETE_BATCH) {
            self.store.delete_batch(batch).await?;
        }
        Ok(keys.len() as u64)
    }

    /// Removes every object under a deleted project's key prefix, page by page, and returns how many it removed:
    /// its export files, which the sweep, listing live projects only, never reaches again, and any media its
    /// deletion left behind (contract 13.4). Nothing under a deleted project's prefix is kept.
    pub async fn remove_project(&self, project_id: ProjectId) -> Result<u64, ObjectError> {
        let root = self.project_root(project_id);
        let mut after: Option<String> = None;
        let mut removed = 0;
        loop {
            let page = self.store.list(&root, after.as_deref()).await?;
            let keys: Vec<String> = page.objects.into_iter().map(|o| o.key).collect();
            for batch in keys.chunks(DELETE_BATCH) {
                self.store.delete_batch(batch).await?;
                removed += batch.len() as u64;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(removed),
            }
        }
    }
}

/// The file of an attempt on its way to the object store.
pub(crate) struct ObjectDraft {
    store: Arc<dyn ObjectStore>,
    key: String,
    content_type: &'static str,
    /// The bytes not yet uploaded.
    held: Vec<u8>,
    /// The multipart upload, once the file filled a part: its id and the `ETag`s of the parts so far.
    upload: Option<(String, Vec<String>)>,
}

impl fmt::Debug for ObjectDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectDraft")
            .field("key", &self.key)
            .field("held", &self.held.len())
            .field("parts", &self.upload.as_ref().map(|(_, parts)| parts.len()))
            .finish_non_exhaustive()
    }
}

impl ObjectDraft {
    /// Appends bytes, uploading a part each time the held bytes fill one.
    pub(crate) async fn append(&mut self, chunk: Bytes) -> Result<(), ObjectError> {
        self.held.extend_from_slice(&chunk);
        while self.held.len() >= PART_BYTES {
            let rest = self.held.split_off(PART_BYTES);
            let part = std::mem::replace(&mut self.held, rest);
            self.upload_part(Bytes::from(part)).await?;
        }
        Ok(())
    }

    async fn upload_part(&mut self, part: Bytes) -> Result<(), ObjectError> {
        if self.upload.is_none() {
            let id = self
                .store
                .start_upload(&self.key, self.content_type)
                .await?;
            self.upload = Some((id, Vec::new()));
        }
        let Some((id, etags)) = &mut self.upload else {
            return Err(ObjectError::Invalid("the upload did not start".to_owned()));
        };
        let number = u16::try_from(etags.len() + 1)
            .ok()
            .filter(|n| *n <= 10_000)
            .ok_or_else(|| {
                ObjectError::Invalid("the export is larger than 10,000 parts".to_owned())
            })?;
        let etag = self.store.upload_part(&self.key, id, number, part).await?;
        etags.push(etag);
        Ok(())
    }

    /// Makes the object appear whole under its key, and returns the row's reference to it. An upload that cannot
    /// be finished is abandoned.
    pub(crate) async fn commit(mut self) -> Result<String, ObjectError> {
        let rest = Bytes::from(std::mem::take(&mut self.held));
        if self.upload.is_none() {
            self.store
                .put_small(&self.key, self.content_type, rest)
                .await?;
            return Ok(format!("{OBJECT_REF}{}", self.key));
        }
        let finished = async {
            if !rest.is_empty() {
                self.upload_part(rest).await?;
            }
            match &self.upload {
                Some((id, etags)) => self.store.finish_upload(&self.key, id, etags).await,
                None => Err(ObjectError::Invalid("the upload did not start".to_owned())),
            }
        }
        .await;
        match finished {
            Ok(()) => Ok(format!("{OBJECT_REF}{}", self.key)),
            Err(error) => {
                self.abandon().await;
                Err(error)
            }
        }
    }

    /// Abandons the upload, if one started, so that the store frees its parts.
    pub(crate) async fn abandon(self) {
        if let Some((id, _)) = &self.upload
            && let Err(error) = self.store.abort_upload(&self.key, id).await
        {
            tracing::debug!(%error, key = %self.key, "abandoning an export upload failed; the bucket's lifecycle rule removes it");
        }
    }
}

/// Removes the export objects that no export needs: those whose export is gone, expired or failed, and those of an
/// attempt that is neither the one the row recorded nor one still holding its claim. Returns how many it removed.
/// It lists each project's export keys, so it never walks the deployment's media.
pub async fn sweep_objects(
    repos: &dyn Repos,
    objects: &ExportObjects,
    now: Timestamp,
) -> Result<u64, FilesError> {
    let mut removed = 0;
    for project in repos.projects().list().await? {
        let dir = objects.project_dir(project.id);
        let mut after: Option<String> = None;
        loop {
            let page = objects.store.list(&dir, after.as_deref()).await?;
            let named: Vec<(&str, Named)> = page
                .objects
                .iter()
                .filter_map(|o| {
                    objects
                        .named(project.id, &o.key)
                        .map(|n| (o.key.as_str(), n))
                })
                .collect();
            let mut ids: Vec<ExportJobId> = named.iter().map(|(_, n)| n.id).collect();
            ids.sort();
            ids.dedup();
            let rows = if ids.is_empty() {
                Vec::new()
            } else {
                repos.export_jobs().get_many(&ids).await?
            };
            let gone: Vec<String> = named
                .iter()
                .filter(|(key, n)| {
                    let row = rows
                        .iter()
                        .find(|job| job.new.id == n.id && job.new.project_id == project.id);
                    stale(*n, &format!("{OBJECT_REF}{key}"), row, now)
                })
                .map(|(key, _)| (*key).to_owned())
                .collect();
            for batch in gone.chunks(DELETE_BATCH) {
                objects.store.delete_batch(batch).await?;
            }
            removed += gone.len() as u64;
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use crucible_control::{ClaimStatus, ExportFormat, NewExport};
    use crucible_sim::object_model::{ObjectFaults, SimObjectStore};

    use super::*;

    const PROJECT: ProjectId = ProjectId::from_bytes([0x5a; 16]);

    fn job() -> ExportJob {
        let id = ExportJobId::from_bytes([0x0e; 16]);
        ExportJob {
            new: NewExport {
                id,
                project_id: PROJECT,
                format: ExportFormat::OtlpJson,
                selector: String::new(),
                options: String::new(),
                dedup_key: id.to_string(),
                created_by: "user:test".to_owned(),
                created_at: Timestamp::EPOCH,
                as_of: Timestamp::EPOCH,
            },
            status: ClaimStatus::Pending,
            attempts: 1,
            lease_until: None,
            spans: 0,
            skipped: 0,
            bytes: 0,
            file: None,
            checksum: None,
            expires_at: None,
            error: None,
        }
    }

    /// A store and export objects over it, on a simulated host.
    fn with_store<F, Fut>(then: F)
    where
        F: FnOnce(Arc<SimObjectStore>, ExportObjects) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        crucible_sim::run(5, |sim| async move {
            let host = sim.host("node", &sim.disk("node"), 0);
            let store = SimObjectStore::new(
                Arc::clone(&host.env.clock),
                sim.rng("objects"),
                ObjectFaults::default(),
            );
            let objects = ExportObjects::new(store.clone(), "media");
            then(store, objects).await;
        });
    }

    fn bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ seed)
            .collect()
    }

    #[test]
    fn keys_name_their_export_and_attempt_and_nothing_else() {
        with_store(|_, objects| async move {
            let job = job();
            let id = job.new.id;
            let key = objects.key(&job, 3, true);
            assert_eq!(
                key,
                format!("media/{PROJECT}/exports/{id}/3/crucible-export-{id}.otlp.jsonl.gz")
            );
            assert_eq!(
                objects.named(PROJECT, &key),
                Some(Named {
                    id,
                    attempt: 3,
                    temporary: false
                })
            );
            let other = ProjectId::from_bytes([0x5b; 16]);
            for foreign in [
                format!("media/{PROJECT}/2026/11/{id}"),
                format!("media/{PROJECT}/exports/{id}/3/notes.txt"),
                format!("media/{PROJECT}/exports/{id}/x/crucible-export-{id}.jsonl"),
                format!("media/{PROJECT}/exports/{id}/3/crucible-export-{id}.jsonl/more"),
                format!("media/{other}/exports/{id}/3/crucible-export-{id}.jsonl"),
            ] {
                assert_eq!(objects.named(PROJECT, &foreign), None, "{foreign}");
            }
            let bare = ExportObjects::new(Arc::clone(&objects.store), "");
            assert!(
                bare.key(&job, 1, false)
                    .starts_with(&format!("{PROJECT}/exports/"))
            );
        });
    }

    /// A file shorter than a part is one request at the commit; a longer one is a multipart upload of whole parts
    /// and a short last part. Either way nothing is visible before the commit.
    #[test]
    fn a_file_appears_whole_at_the_commit_in_one_request_or_in_parts() {
        with_store(|store, objects| async move {
            for len in [0, 10, PART_BYTES - 1, PART_BYTES, 2 * PART_BYTES + 1_234] {
                let mut draft = objects.draft(&job(), 1, false);
                let content = bytes(len, 7);
                for chunk in content.chunks(1 << 20) {
                    draft.append(Bytes::copy_from_slice(chunk)).await.unwrap();
                }
                let parts = draft.upload.as_ref().map_or(0, |(_, etags)| etags.len());
                assert_eq!(parts, len / PART_BYTES, "{len} bytes");
                assert!(
                    store.objects().is_empty(),
                    "{len} bytes: visible before the commit"
                );
                let reference = draft.commit().await.unwrap();
                let key = object_of(&reference).unwrap();
                let stored = store.objects().remove(key).unwrap();
                assert_eq!(stored.bytes.as_ref(), content.as_slice(), "{len} bytes");
                assert_eq!(stored.content_type, "application/x-ndjson");
                assert!(store.open_uploads().is_empty());
                objects.delete(key).await.unwrap();
            }
        });
    }

    /// A deleted project's objects go whole, export files and media alike, and no other project's do.
    #[test]
    fn removing_a_project_takes_every_object_under_its_prefix_and_nothing_else() {
        with_store(|store, objects| async move {
            let other = ProjectId::from_bytes([0x5b; 16]);
            let mut mine = Vec::new();
            for n in 0..(DELETE_BATCH + 7) {
                mine.push(format!(
                    "media/{PROJECT}/exports/{n}/1/crucible-export-{n}.jsonl"
                ));
            }
            mine.push(format!(
                "media/{PROJECT}/2026/11/0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
            ));
            let theirs = format!("media/{other}/exports/x/1/crucible-export-x.jsonl");
            for key in mine.iter().chain([&theirs]) {
                store
                    .put_small(key, "application/x-ndjson", Bytes::from_static(b"{}"))
                    .await
                    .unwrap();
            }
            let removed = objects.remove_project(PROJECT).await.unwrap();
            assert_eq!(removed, mine.len() as u64);
            let left: Vec<String> = store.objects().keys().cloned().collect();
            assert_eq!(left, [theirs]);
            assert_eq!(objects.remove_project(PROJECT).await.unwrap(), 0);
        });
    }

    #[test]
    fn an_abandoned_file_leaves_neither_an_object_nor_an_upload() {
        with_store(|store, objects| async move {
            let mut draft = objects.draft(&job(), 2, true);
            draft
                .append(Bytes::from(bytes(PART_BYTES + 5, 3)))
                .await
                .unwrap();
            assert_eq!(store.open_uploads().len(), 1);
            draft.abandon().await;
            assert!(store.open_uploads().is_empty());
            assert!(store.objects().is_empty());
        });
    }
}
