//! The export service of the API and the MCP server (contracts 11.9, 12.5, 12.6 and 12.8): creating an export
//! under the project's rate limit and the request's `Idempotency-Key`, reading exports, and opening a committed
//! file for download.

use std::sync::Arc;

use bytes::Bytes;
use crucible_control::{ControlStore, ExportFormat, ExportJob, NewExport, TxError};
use crucible_env::{DiskError, DiskPath, Env, File, OpenMode, Root};
use crucible_records::{ExportJobId, ProjectScope, Timestamp};

use crate::files::extension;
use crate::job::ExportState;
use crate::spec::{Options, Selector};

/// The exports a project may create per hour (contract 11.9).
pub const EXPORTS_PER_HOUR: u64 = 5;
const HOUR_MICROS: i64 = 3_600_000_000;

/// A request to create an export.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateExport {
    /// The format.
    pub format: ExportFormat,
    /// What it holds.
    pub selector: Selector,
    /// How it is written.
    pub options: Options,
    /// Who asks, for example `user:<id>` or `api_key:<id>`: the principal of the `Idempotency-Key`.
    pub created_by: String,
    /// The request's `Idempotency-Key`, already validated.
    pub idempotency_key: Option<String>,
}

/// A created export.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Created {
    /// A new export.
    New(ExportJob),
    /// The export an earlier identical request under the same `Idempotency-Key` created.
    Replay(ExportJob),
}

/// Why an export was not created.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// The project created [`EXPORTS_PER_HOUR`] exports in the last hour.
    #[error("the project created {EXPORTS_PER_HOUR} exports in the last hour")]
    RateLimited {
        /// Seconds until the oldest of them is an hour old.
        retry_after_s: u32,
    },
    /// The `Idempotency-Key` was used for another request.
    #[error("the Idempotency-Key was used for another request")]
    KeyReused,
    /// The control store failed.
    #[error(transparent)]
    Store(#[from] TxError),
}

/// A committed file opened for download.
#[derive(Debug)]
pub struct Download {
    handle: File,
    /// The name a browser saves it under.
    pub file_name: String,
    /// The file's length.
    pub len: u64,
    /// SHA-256 of the file, lowercase hex.
    pub checksum: String,
    /// Whether it is gzip-compressed.
    pub gzip: bool,
}

/// Why a file cannot be downloaded.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// No export of the project has the id.
    #[error("no export of the project has this id")]
    NotFound,
    /// The export has no file yet, or ended without one.
    #[error("the export has no file: it is {0}")]
    NotReady(&'static str),
    /// The file expired, or a deletion request the export predates expired it.
    #[error("the export file expired")]
    Expired,
    /// The file is not on this node's disk.
    #[error("the export file is not on this node")]
    Missing,
    /// The control store failed.
    #[error(transparent)]
    Store(#[from] TxError),
    /// The disk failed.
    #[error("reading the export file failed: {0}")]
    Disk(#[from] DiskError),
}

/// The export service over the control store and this node's exports root.
#[derive(Clone)]
pub struct Exports {
    env: Env,
    store: Arc<dyn ControlStore>,
}

impl std::fmt::Debug for Exports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exports").finish_non_exhaustive()
    }
}

impl Exports {
    /// The service.
    pub fn new(env: Env, store: Arc<dyn ControlStore>) -> Self {
        Self { env, store }
    }

    /// The wall clock now: what states and expiries are read against.
    pub fn now(&self) -> Timestamp {
        self.env.clock.wall_now()
    }

    /// Creates an export of the scope's project with a fixed `as_of` of now, under the rate limit and the
    /// `Idempotency-Key`. A replay neither counts toward the limit nor creates a row.
    pub async fn create(&self, scope: ProjectScope, request: CreateExport) -> Result<Created, CreateError> {
        let project_id = scope.project_id();
        let exports = self.store.export_jobs();
        let dedup_key = request
            .idempotency_key
            .as_ref()
            .map(|key| format!("{project_id}:{}:{key}", request.created_by));
        let selector = request.selector.to_text();
        let options = request.options.to_text();
        let same = |job: &ExportJob| {
            job.new.format == request.format && job.new.selector == selector && job.new.options == options
        };
        if let Some(key) = &dedup_key {
            if let Some(job) = exports.get_by_dedup_key(project_id, key).await? {
                return if same(&job) { Ok(Created::Replay(job)) } else { Err(CreateError::KeyReused) };
            }
        }
        let now = self.now();
        let recent = exports.list(project_id, EXPORTS_PER_HOUR as u32).await?;
        let in_hour: Vec<&ExportJob> = recent
            .iter()
            .filter(|job| job.new.created_at > now.saturating_add_micros(-HOUR_MICROS))
            .collect();
        if in_hour.len() as u64 >= EXPORTS_PER_HOUR {
            let oldest = in_hour.iter().map(|job| job.new.created_at).min().unwrap_or(now);
            let wait = oldest.saturating_add_micros(HOUR_MICROS).micros() - now.micros();
            let retry_after_s = u32::try_from((wait.max(1) + 999_999) / 1_000_000).unwrap_or(u32::MAX);
            return Err(CreateError::RateLimited { retry_after_s });
        }
        let id = ExportJobId::from_uuid(self.env.new_id());
        let new = NewExport {
            id,
            project_id,
            format: request.format,
            selector: selector.clone(),
            options: options.clone(),
            dedup_key: dedup_key.clone().unwrap_or_else(|| id.to_string()),
            created_by: request.created_by.clone(),
            created_at: now,
            as_of: now,
        };
        match exports.insert(&new).await {
            Ok(()) => {}
            // Another request under the same key won the race: answer as a replay of it.
            Err(TxError::UniqueViolation(constraint)) if constraint == "export_jobs_dedup_key_key" => {
                if let Some(job) = exports.get_by_dedup_key(project_id, &new.dedup_key).await? {
                    return if same(&job) { Ok(Created::Replay(job)) } else { Err(CreateError::KeyReused) };
                }
                return Err(TxError::UniqueViolation(constraint).into());
            }
            Err(error) => return Err(error.into()),
        }
        let job = exports.get(project_id, id).await?.ok_or(TxError::Invalid(
            "the export that was just created is not there".to_owned(),
        ))?;
        Ok(Created::New(job))
    }

    /// The export of the scope's project with this id.
    pub async fn get(&self, scope: ProjectScope, id: ExportJobId) -> Result<Option<ExportJob>, TxError> {
        self.store.export_jobs().get(scope.project_id(), id).await
    }

    /// The scope's project's newest exports, newest first.
    pub async fn list(&self, scope: ProjectScope, limit: u32) -> Result<Vec<ExportJob>, TxError> {
        self.store.export_jobs().list(scope.project_id(), limit).await
    }

    /// The committed file of an export, ready to be read.
    pub async fn open(&self, scope: ProjectScope, id: ExportJobId) -> Result<Download, DownloadError> {
        let job = self.get(scope, id).await?.ok_or(DownloadError::NotFound)?;
        let state = ExportState::of(&job, self.now());
        let file = match (state, &job.file) {
            (ExportState::Expired, _) => return Err(DownloadError::Expired),
            (ExportState::Ready, Some(file)) => file.clone(),
            (state, _) => return Err(DownloadError::NotReady(state.as_str())),
        };
        let path = DiskPath::new(Root::Exports, &file)?;
        let handle = match self.env.disk.open(&path, OpenMode::Read).await {
            Err(DiskError::NotFound) => return Err(DownloadError::Missing),
            opened => opened?,
        };
        let gzip = file.ends_with(".gz");
        Ok(Download {
            handle,
            file_name: format!("crucible-export-{id}.{}", extension(job.new.format, gzip)),
            len: job.bytes,
            checksum: job.checksum.unwrap_or_default(),
            gzip,
        })
    }

}

impl Download {
    /// Up to `len` bytes of the file from `offset`; fewer only at its end.
    pub async fn read(&self, offset: u64, len: usize) -> Result<Bytes, DiskError> {
        self.handle.read_at(offset, len).await
    }
}
