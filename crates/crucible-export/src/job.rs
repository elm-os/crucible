//! The export job (contracts 11.7 and 12.8): a consumer of the claimed queue `export_jobs`, and a per-node timer
//! that sweeps the exports root.
//!
//! - **Idempotent.** A run first reads the row and stops when the export already has a file, an error or an
//!   expiry. Every attempt writes its own temporary file and its own final name, and records its outcome only
//!   while the row is still pending under its attempt, so an attempt whose claim ran out records nothing over a
//!   newer one and removes the file it made.
//! - **Waits, bounded.** The reads cover the spans acknowledged at or before the export's `as_of`; while some of
//!   them are still on their way to the store the run waits, up to [`SHIPPING_WAIT`], before it gives the row
//!   back for a later attempt.
//! - **Fails once for good.** A guardrail refusal of the store (`query_timeout`, `query_memory_exceeded`,
//!   `query_too_broad`) or a run that outlasts its claim is recorded as the export's error, so the same doomed
//!   run does not repeat; the error code says to narrow the window.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crucible_control::{Claim, ControlStore, ExportCommit, ExportJob, Queue};
use crucible_env::{ClockExt as _, QueryError};
use crucible_jobs::{Cadence, ClaimCtx, ClaimPolicy, Job, JobError, JobResult, NodeCtx};
use crucible_plan::Caller;
use crucible_query::{ExportLimits, QueryService, QueryServiceError, ReadContext};
use crucible_records::{ExportJobId, Timestamp};
use crucible_schema::RetentionSettings;

use crate::files::{Draft, remove, sweep};
use crate::spec::{Options, Selector};
use crate::stream::{CHUNK_BYTES, ExportWriter, Pages, Progress, WriteError};

/// How long a claim on an export lasts: long enough for a large export; a run that needs longer fails with
/// `export_too_large` (default; revisit).
pub const EXPORT_LEASE: Duration = Duration::from_secs(30 * 60);
/// How much of the claim a run may use; the rest leaves time to record the outcome.
const RUN_BUDGET: Duration = Duration::from_secs(29 * 60);
/// How long a run waits for acknowledged spans to reach the store before it gives the row back.
pub const SHIPPING_WAIT: Duration = Duration::from_secs(60);
/// How long a committed file stays downloadable (default; revisit).
pub const EXPORT_TTL: Duration = Duration::from_secs(7 * 24 * 3_600);
/// How often each node sweeps its exports root.
pub const SWEEP_EVERY: Duration = Duration::from_secs(600);
/// How many chunks are written between two progress records.
const PROGRESS_EVERY: u32 = 8;

/// The export error codes a row records: stable, so that the API and the UI can say what to change.
pub mod codes {
    /// A read hit the export class's time limit.
    pub const QUERY_TIMEOUT: &str = "query_timeout";
    /// A read hit the export class's memory limit.
    pub const QUERY_MEMORY_EXCEEDED: &str = "query_memory_exceeded";
    /// A read was refused as too broad.
    pub const QUERY_TOO_BROAD: &str = "query_too_broad";
    /// The run did not end within its claim.
    pub const EXPORT_TOO_LARGE: &str = "export_too_large";
    /// The project was deleted before the export ran.
    pub const PROJECT_DELETED: &str = "project_deleted";
    /// The stored request does not read back (a defect).
    pub const INTERNAL_ERROR: &str = "internal_error";
}

/// What the export job reads with, set once the telemetry store is up: the query service. Until then a claimed
/// export is given back with a backoff.
#[derive(Clone, Debug, Default)]
pub struct ExportRuntime {
    service: Arc<OnceLock<QueryService>>,
}

impl ExportRuntime {
    /// A runtime without a query service yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The query service, once the telemetry store is up. Later calls change nothing.
    pub fn set_service(&self, service: QueryService) {
        let _ = self.service.set(service);
    }

    fn service(&self) -> Option<&QueryService> {
        self.service.get()
    }
}

/// The claimed consumer of `export_jobs`.
pub fn job(runtime: ExportRuntime) -> Job {
    let policy = ClaimPolicy {
        lease_for: EXPORT_LEASE,
        ..ClaimPolicy::default()
    };
    Job::claimed(
        "exports",
        Queue::ExportJobs,
        Cadence::every(Duration::from_secs(2)),
        policy,
        move |ctx| {
            let runtime = runtime.clone();
            async move { run(ctx, runtime).await }
        },
    )
}

/// The per-node sweep of the exports root: files no export needs, and files of expired exports. Without a
/// control plane there are no exports and nothing to sweep.
pub fn sweep_job() -> Job {
    Job::per_node("exports-sweep", Cadence::every(SWEEP_EVERY), |ctx: NodeCtx| async move {
        let Some(store) = ctx.control else { return Ok(()) };
        let swept = sweep(store.as_ref(), ctx.env.disk.as_ref(), ctx.env.clock.wall_now())
            .await
            .map_err(|error| JobError::Failed(error.to_string()))?;
        if swept.files > 0 {
            tracing::info!(job = "exports-sweep", files = swept.files, "removed export files that no export needs");
        }
        Ok(())
    })
}

fn failed(error: impl std::fmt::Display) -> JobError {
    JobError::Failed(error.to_string())
}

/// The error code of a read failure that no retry fixes.
fn guardrail(error: &WriteError) -> Option<&'static str> {
    match error {
        WriteError::Read(QueryServiceError::Store(QueryError::Timeout)) => Some(codes::QUERY_TIMEOUT),
        WriteError::Read(QueryServiceError::Store(QueryError::MemoryExceeded)) => {
            Some(codes::QUERY_MEMORY_EXCEEDED)
        }
        WriteError::Read(QueryServiceError::Store(QueryError::TooBroad)) => Some(codes::QUERY_TOO_BROAD),
        _ => None,
    }
}

/// Records why the export ended without a file. The row is then completed.
async fn give_up(store: Arc<dyn ControlStore>, claim: Claim, code: &'static str) -> JobResult {
    let recorded = store.export_jobs().fail(&claim, code).await?;
    tracing::info!(job = "exports", id = %claim.id, code, recorded, "the export ended without a file");
    Ok(())
}

async fn run(ctx: ClaimCtx, runtime: ExportRuntime) -> JobResult {
    let id = ExportJobId::from_uuid(ctx.claim.id);
    let Some(job) = ctx.store.export_jobs().get_many(&[id]).await?.into_iter().next() else {
        return Ok(());
    };
    if job.file.is_some() || job.error.is_some() || job.expires_at.is_some() {
        return Ok(());
    }
    let project = ctx.store.projects().get(job.new.project_id).await?;
    let Some(project) = project.filter(|p| p.deleted_at.is_none()) else {
        return give_up(Arc::clone(&ctx.store), ctx.claim.clone(), codes::PROJECT_DELETED).await;
    };
    let (Ok(selector), Ok(options)) = (Selector::from_text(&job.new.selector), Options::from_text(&job.new.options))
    else {
        return give_up(Arc::clone(&ctx.store), ctx.claim.clone(), codes::INTERNAL_ERROR).await;
    };
    let Some(service) = runtime.service() else {
        return Err(failed("the telemetry store is not up yet"));
    };
    let scope = crucible_auth::stored_project_scope(&project).map_err(failed)?;
    let retention = RetentionSettings {
        retention_days: project.retention_days,
        payload_retention_days: project.payload_retention_days,
        mcp_content_retention_days: project.mcp_content_retention_days,
    };
    let read = ReadContext::new(scope, retention, Caller::Session);
    let query = selector.query(job.new.as_of);

    // The spans acknowledged before `as_of` must be in the store before the file is written.
    let started = ctx.env.clock.now();
    while let Some(wait) = service.export_wait(&read, &query) {
        if ctx.env.clock.now().saturating_duration_since(started) >= SHIPPING_WAIT {
            return Err(failed(wait.describe()));
        }
        ctx.env.clock.sleep(Duration::from_secs(1)).await;
    }

    let mut draft = Draft::open(Arc::clone(&ctx.env.disk), &job, ctx.claim.attempt, options.gzip)
        .await
        .map_err(failed)?;
    let mut writer = ExportWriter::new(job.new.format, options);
    let written = ctx
        .env
        .clock
        .timeout(RUN_BUDGET, write_file(&ctx, service, &read, &query, &mut writer, &mut draft))
        .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            draft.abandon().await;
            return match guardrail(&error) {
                Some(code) => give_up(Arc::clone(&ctx.store), ctx.claim.clone(), code).await,
                None => Err(failed(error)),
            };
        }
        Err(_) => {
            draft.abandon().await;
            return give_up(Arc::clone(&ctx.store), ctx.claim.clone(), codes::EXPORT_TOO_LARGE).await;
        }
    }
    let (rest, summary) = writer.finish().map_err(failed)?;
    draft.append(rest).await.map_err(failed)?;
    let file = draft.commit().await.map_err(failed)?;
    let commit = ExportCommit {
        file: file.clone(),
        checksum: summary.sha256,
        spans: summary.spans,
        bytes: summary.bytes,
        skipped: summary.skipped,
        expires_at: expiry(ctx.env.clock.wall_now()),
    };
    match ctx.store.export_jobs().commit(&ctx.claim, &commit).await {
        Ok(true) => Ok(()),
        Ok(false) => {
            // A newer attempt, an expiry or a failure got there first: this file is nobody's.
            remove(ctx.env.disk.as_ref(), &file).await.map_err(failed)?;
            Ok(())
        }
        // The outcome is unknown: a later attempt finds the row committed, or redoes the export, and the sweep
        // removes whichever file the row does not name.
        Err(error) => Err(error.into()),
    }
}

/// When a file committed at `now` stops being downloadable.
pub fn expiry(now: Timestamp) -> Timestamp {
    now.saturating_add_micros(i64::try_from(EXPORT_TTL.as_micros()).unwrap_or(i64::MAX))
}

async fn write_file(
    ctx: &ClaimCtx,
    service: &QueryService,
    read: &ReadContext,
    query: &crucible_query::ExportQuery,
    writer: &mut ExportWriter,
    draft: &mut Draft,
) -> Result<(), WriteError> {
    let mut pages = Pages::new(service, read, query, ExportLimits::default());
    let mut chunks = 0u32;
    while pages.next(writer).await? {
        let Some(chunk) = writer.ready(CHUNK_BYTES) else { continue };
        draft.append(chunk).await.map_err(|e| WriteError::Sink(e.to_string()))?;
        chunks += 1;
        if chunks % PROGRESS_EVERY == 0 {
            let Progress { spans, bytes, .. } = writer.progress();
            let held = ctx
                .store
                .export_jobs()
                .progress(&ctx.claim, spans, bytes)
                .await
                .map_err(|e| WriteError::Sink(e.to_string()))?;
            if !held {
                return Err(WriteError::Sink("the export was claimed again or closed".to_owned()));
            }
        }
    }
    Ok(())
}

/// The life-cycle state of an export as a reader sees it at `now` (contract 12.8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportState {
    /// Waiting for a node to claim it.
    Queued,
    /// A node is writing it.
    Running,
    /// The file is committed and downloadable.
    Ready,
    /// It ended without a file; the row's error says why.
    Failed,
    /// Its file expired, or a deletion request it predates expired it.
    Expired,
}

impl ExportState {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    /// The state of `job` at `now`.
    pub fn of(job: &ExportJob, now: Timestamp) -> Self {
        if job.expires_at.is_some_and(|at| at <= now) {
            Self::Expired
        } else if job.file.is_some() {
            Self::Ready
        } else if job.error.is_some() || job.status == crucible_control::ClaimStatus::Parked {
            Self::Failed
        } else if job.lease_until.is_some_and(|until| until > now) {
            Self::Running
        } else {
            Self::Queued
        }
    }
}
