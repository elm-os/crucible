//! Export files on the `exports` disk root (contracts 2.4 and 12.8). An attempt writes a temporary file of its
//! own, `<project>/<export>.<attempt>.part`; the commit is `sync_data`, a rename to the attempt's final name
//! `<project>/<export>.<attempt>.<extension>` and `sync_dir`, and only then does the row record the file. A
//! crash before the rename leaves a temporary file and no committed one; a crash between the rename and the row
//! leaves a file no row names. The sweep removes both, and files whose export expired. With an object store the
//! file goes there instead ([`crate::objects`]).

use std::sync::Arc;

use bytes::Bytes;
use crucible_control::{ClaimStatus, ExportFormat, ExportJob, Repos, TxError};
use crucible_env::{Disk, DiskError, DiskPath, File, ObjectError, OpenMode, Root};
use crucible_records::{ExportJobId, ProjectId, Timestamp};

use crate::objects::{ExportObjects, ObjectDraft, object_of};

/// The file name extension of a format.
pub(crate) const fn extension(format: ExportFormat, gzip: bool) -> &'static str {
    match (format, gzip) {
        (ExportFormat::CrucibleJsonl, false) => "jsonl",
        (ExportFormat::CrucibleJsonl, true) => "jsonl.gz",
        (ExportFormat::OtlpJson, false) => "otlp.jsonl",
        (ExportFormat::OtlpJson, true) => "otlp.jsonl.gz",
    }
}

fn project_dir(project_id: ProjectId) -> Result<DiskPath, DiskError> {
    DiskPath::new(Root::Exports, &project_id.to_string())
}

/// What a file name in a project's directory, or an export object's key, says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Named {
    pub(crate) id: ExportJobId,
    pub(crate) attempt: u32,
    pub(crate) temporary: bool,
}

fn parse_name(name: &str) -> Option<Named> {
    let mut parts = name.splitn(3, '.');
    let id = ExportJobId::parse(parts.next()?).ok()?;
    let attempt = parts.next()?.parse().ok()?;
    let rest = parts.next()?;
    Some(Named {
        id,
        attempt,
        temporary: rest == "part",
    })
}

/// The file an attempt is writing.
pub(crate) struct Draft {
    disk: Arc<dyn Disk>,
    dir: DiskPath,
    temporary: DiskPath,
    committed: String,
    file: File,
    offset: u64,
}

impl std::fmt::Debug for Draft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Draft")
            .field("temporary", &self.temporary)
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

impl Draft {
    /// Opens the temporary file of an attempt, replacing one a crashed run of the same attempt left.
    pub(crate) async fn open(
        disk: Arc<dyn Disk>,
        job: &ExportJob,
        attempt: u32,
        gzip: bool,
    ) -> Result<Self, DiskError> {
        let (project_id, id) = (job.new.project_id, job.new.id);
        let dir = project_dir(project_id)?;
        disk.create_dir_all(&dir).await?;
        let temporary = dir.join(&format!("{id}.{attempt}.part"))?;
        let file = match disk.open(&temporary, OpenMode::CreateNew).await {
            Err(DiskError::AlreadyExists) => {
                disk.remove(&temporary).await?;
                disk.open(&temporary, OpenMode::CreateNew).await?
            }
            opened => opened?,
        };
        let committed = format!(
            "{project_id}/{id}.{attempt}.{}",
            extension(job.new.format, gzip)
        );
        Ok(Self {
            disk,
            dir,
            temporary,
            committed,
            file,
            offset: 0,
        })
    }

    /// Appends bytes.
    pub(crate) async fn append(&mut self, chunk: Bytes) -> Result<(), DiskError> {
        let len = chunk.len() as u64;
        if len > 0 {
            self.file.append(self.offset, vec![chunk]).await?;
            self.offset += len;
        }
        Ok(())
    }

    /// Makes the file durable under its final name, and returns that name, relative to the exports root.
    pub(crate) async fn commit(self) -> Result<String, DiskError> {
        self.file.sync_data().await?;
        drop(self.file);
        let committed = DiskPath::new(Root::Exports, &self.committed)?;
        self.disk.rename(&self.temporary, &committed).await?;
        self.disk.sync_dir(&self.dir).await?;
        Ok(self.committed)
    }

    /// Removes the temporary file.
    pub(crate) async fn abandon(self) {
        drop(self.file);
        if let Err(error) = self.disk.remove(&self.temporary).await {
            tracing::debug!(%error, file = %self.temporary, "removing an abandoned export file failed; the sweep removes it");
        }
    }
}

/// Removes a committed file, relative to the exports root. A file that is not there is already removed.
pub(crate) async fn remove(disk: &dyn Disk, file: &str) -> Result<(), DiskError> {
    let path = DiskPath::new(Root::Exports, file)?;
    match disk.remove(&path).await {
        Ok(()) | Err(DiskError::NotFound) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes a committed file that no row records: from this node's disk, or from the object store. Without the
/// store an object stays for the sweep of a node that has it.
pub(crate) async fn discard(
    disk: &dyn Disk,
    objects: Option<&ExportObjects>,
    file: &str,
) -> Result<(), FilesError> {
    match (object_of(file), objects) {
        (Some(key), Some(objects)) => objects.delete(key).await?,
        (Some(_), None) => {}
        (None, _) => remove(disk, file).await?,
    }
    Ok(())
}

/// Whether a file of the exports root, or an export object, should go: its export is gone, expired or failed, or
/// the file belongs to an attempt that is neither the committed one nor one still holding its claim. A file of the
/// attempt that holds the claim stays, also once committed and before the row records it.
pub(crate) fn stale(named: Named, path: &str, row: Option<&ExportJob>, now: Timestamp) -> bool {
    let Some(job) = row else { return true };
    if job.expires_at.is_some_and(|at| at <= now) || job.error.is_some() {
        return true;
    }
    let running = job.status == ClaimStatus::Pending
        && job.file.is_none()
        && job.attempts == named.attempt
        && job.lease_until.is_some_and(|until| until > now);
    if named.temporary {
        !running
    } else {
        !running && job.file.as_deref() != Some(path)
    }
}

/// What a sweep removed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Swept {
    /// Files removed.
    pub files: u64,
    /// Project directories removed because they were empty.
    pub directories: u64,
}

/// Why a sweep, a purge or a file's commit stopped.
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    /// The disk failed.
    #[error("the exports directory failed: {0}")]
    Disk(#[from] DiskError),
    /// The control store failed.
    #[error(transparent)]
    Store(#[from] TxError),
    /// The object store failed.
    #[error("the object store failed: {0}")]
    Objects(#[from] ObjectError),
}

/// Where an attempt writes its file: this node's exports root, or the object store.
#[derive(Debug)]
pub(crate) enum Sink {
    /// A temporary file on the disk, renamed at the commit.
    Disk(Draft),
    /// An object that appears whole at the commit.
    Object(ObjectDraft),
}

impl Sink {
    /// Opens the file of an attempt: on the object store when the deployment has one, else on the disk.
    pub(crate) async fn open(
        disk: Arc<dyn Disk>,
        objects: Option<&ExportObjects>,
        job: &ExportJob,
        attempt: u32,
        gzip: bool,
    ) -> Result<Self, FilesError> {
        Ok(match objects {
            Some(objects) => Self::Object(objects.draft(job, attempt, gzip)),
            None => Self::Disk(Draft::open(disk, job, attempt, gzip).await?),
        })
    }

    /// Appends bytes.
    pub(crate) async fn append(&mut self, chunk: Bytes) -> Result<(), FilesError> {
        match self {
            Self::Disk(draft) => Ok(draft.append(chunk).await?),
            Self::Object(draft) => Ok(draft.append(chunk).await?),
        }
    }

    /// Makes the file whole and durable, and returns the row's reference to it.
    pub(crate) async fn commit(self) -> Result<String, FilesError> {
        match self {
            Self::Disk(draft) => Ok(draft.commit().await?),
            Self::Object(draft) => Ok(draft.commit().await?),
        }
    }

    /// Gives the file up.
    pub(crate) async fn abandon(self) {
        match self {
            Self::Disk(draft) => draft.abandon().await,
            Self::Object(draft) => draft.abandon().await,
        }
    }
}

/// Removes the files of this node's exports root that no export needs (see [`stale`]). `now` is compared with
/// expiry times and claim ends.
pub async fn sweep(
    repos: &dyn Repos,
    disk: &dyn Disk,
    now: Timestamp,
) -> Result<Swept, FilesError> {
    let mut swept = Swept::default();
    let root = DiskPath::root_dir(Root::Exports);
    let projects = match disk.list(&root).await {
        Err(DiskError::NotFound) => return Ok(swept),
        listed => listed?,
    };
    for project in projects {
        let Ok(project_id) = ProjectId::parse(&project.name) else {
            continue;
        };
        let dir = project_dir(project_id)?;
        let entries = disk.list(&dir).await?;
        let named: Vec<(String, Named)> = entries
            .iter()
            .filter_map(|e| parse_name(&e.name).map(|n| (e.name.clone(), n)))
            .collect();
        let mut ids: Vec<ExportJobId> = named.iter().map(|(_, n)| n.id).collect();
        ids.sort();
        ids.dedup();
        let rows = repos.export_jobs().get_many(&ids).await?;
        let mut removed = 0;
        for (name, n) in &named {
            let path = format!("{project_id}/{name}");
            let row = rows
                .iter()
                .find(|job| job.new.id == n.id && job.new.project_id == project_id);
            if stale(*n, &path, row, now) {
                remove(disk, &path).await?;
                removed += 1;
            }
        }
        if removed > 0 {
            disk.sync_dir(&dir).await?;
            swept.files += removed;
        }
        if removed == entries.len() as u64 && disk.remove(&dir).await.is_ok() {
            swept.directories += 1;
        }
    }
    Ok(swept)
}

/// What a purge did.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Purged {
    /// The exports it expired, as they are now.
    pub expired: Vec<ExportJob>,
    /// The files of those exports it removed from this node's exports root.
    pub files_removed: u64,
}

/// The export side of a deletion request (contracts 13.3 step 8 and 13.4): every export of the project created
/// before `requested_at` (the request's acceptance) is expired at `now`, so its download answers
/// `export_expired`, and its file, which may hold deleted spans, is removed from this node's exports root. An
/// export of a deployment with several nodes may have its file on another node, which removes it at its next
/// sweep. Exports created at or after `requested_at` hold new data and are kept.
///
/// `repos` may be a transaction, for example the fenced transaction of the deletion executor; the files are
/// removed after the expiry is written, so a purge that fails can be run again. A file on the object store is
/// never removed here, because a transaction must not wait on the store: once the expiry has committed, pass
/// [`Purged::expired`] to [`ExportObjects::remove`]; the sweep removes what that misses.
pub async fn purge(
    repos: &dyn Repos,
    disk: &dyn Disk,
    project_id: ProjectId,
    requested_at: Timestamp,
    now: Timestamp,
) -> Result<Purged, FilesError> {
    let expired = repos
        .export_jobs()
        .expire_before(project_id, requested_at, now)
        .await?;
    let mut files_removed = 0;
    for job in &expired {
        if let Some(file) = job.file.as_deref().filter(|file| object_of(file).is_none()) {
            remove(disk, file).await?;
            files_removed += 1;
        }
    }
    if files_removed > 0 {
        disk.sync_dir(&project_dir(project_id)?).await?;
    }
    Ok(Purged {
        expired,
        files_removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_read_back() {
        let id = ExportJobId::from_bytes([7; 16]);
        assert_eq!(
            parse_name(&format!("{id}.3.part")),
            Some(Named {
                id,
                attempt: 3,
                temporary: true
            })
        );
        assert_eq!(
            parse_name(&format!("{id}.12.otlp.jsonl.gz")),
            Some(Named {
                id,
                attempt: 12,
                temporary: false
            })
        );
        assert_eq!(parse_name("notes.txt"), None);
        assert_eq!(parse_name(&format!("{id}.x.jsonl")), None);
    }
}
