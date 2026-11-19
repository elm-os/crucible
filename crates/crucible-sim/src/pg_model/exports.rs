//! The export-job repository of the model (migration V5): the rows of `export_jobs` are claim rows of
//! [`Queue::ExportJobs`] that also carry the export's own columns.

use async_trait::async_trait;
use crucible_control::{
    Claim, ClaimStatus, ExportCommit, ExportJob, ExportJobRepo, NewExport, Queue, TxError,
};
use crucible_records::{ExportJobId, ProjectId, Timestamp};

use super::{Cx, JobRow, JobStatus, Pool, Row, Runner, Step, Tbl, job_key};

/// The columns of an export row beyond the claim columns.
#[derive(Clone, Debug)]
pub(super) struct ExportCols {
    pub(super) new: NewExport,
    spans: u64,
    bytes: u64,
    skipped: u64,
    file: Option<String>,
    checksum: Option<String>,
    expires_at: Option<Timestamp>,
    error: Option<String>,
}

fn key(id: ExportJobId) -> Vec<u8> {
    job_key(Queue::ExportJobs, id.uuid())
}

/// The export as the repository returns it.
fn view(job: &JobRow) -> Option<ExportJob> {
    let export = job.export.as_ref()?;
    Some(ExportJob {
        new: export.new.clone(),
        status: match job.status {
            JobStatus::Pending => ClaimStatus::Pending,
            JobStatus::Done => ClaimStatus::Done,
            JobStatus::Parked => ClaimStatus::Parked,
        },
        attempts: job.attempts,
        lease_until: job.lease_until,
        spans: export.spans,
        bytes: export.bytes,
        skipped: export.skipped,
        file: export.file.clone(),
        checksum: export.checksum.clone(),
        expires_at: export.expires_at,
        error: export.error.clone(),
    })
}

/// Every export row, in key order.
fn exports(cx: &Cx<'_>) -> Vec<JobRow> {
    all!(cx, Tbl::Jobs, Row::Job)
        .into_iter()
        .filter(|job| job.queue == Queue::ExportJobs && job.export.is_some())
        .collect()
}

fn of_project(job: &JobRow, project_id: ProjectId) -> bool {
    job.export
        .as_ref()
        .is_some_and(|export| export.new.project_id == project_id)
}

/// Changes the row of the claim when it is still pending under the claim's attempt and `allowed` holds for its
/// export columns.
fn under_claim(
    cx: &mut Cx<'_>,
    claim: &Claim,
    allowed: impl Fn(&ExportCols) -> bool,
    change: impl FnOnce(&mut ExportCols),
) -> Step<bool> {
    let key = job_key(Queue::ExportJobs, claim.id);
    match one!(cx, Tbl::Jobs, &key, Row::Job) {
        Some(mut job)
            if job.attempts == claim.attempt
                && job.status == JobStatus::Pending
                && job.export.as_deref().is_some_and(&allowed) =>
        {
            if let Some(export) = job.export.as_deref_mut() {
                change(export);
            }
            cx.put(Tbl::Jobs, key, Some(Row::Job(job)))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Whether an attempt may still record its outcome: no file, no error, not expired.
fn open(export: &ExportCols) -> bool {
    export.file.is_none() && export.error.is_none() && export.expires_at.is_none()
}

#[async_trait]
impl ExportJobRepo for Runner {
    async fn insert(&self, export: &NewExport) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            let row = JobRow {
                queue: Queue::ExportJobs,
                id: export.id.uuid(),
                dedup_key: export.dedup_key.clone(),
                payload: export.options.clone(),
                attempts: 0,
                next_attempt_at: export.created_at,
                status: JobStatus::Pending,
                lease_until: None,
                export: Some(Box::new(ExportCols {
                    new: export.clone(),
                    spans: 0,
                    bytes: 0,
                    skipped: 0,
                    file: None,
                    checksum: None,
                    expires_at: None,
                    error: None,
                })),
            };
            cx.insert(Tbl::Jobs, key(export.id), Row::Job(row), "export_jobs_pkey")
        })
        .await
    }

    async fn get(
        &self,
        project_id: ProjectId,
        id: ExportJobId,
    ) -> Result<Option<ExportJob>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(cx, Tbl::Jobs, &key(id), Row::Job)
                .filter(|job| of_project(job, project_id))
                .and_then(|job| view(&job)))
        })
        .await
    }

    async fn get_by_dedup_key(
        &self,
        project_id: ProjectId,
        dedup_key: &str,
    ) -> Result<Option<ExportJob>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(exports(cx)
                .iter()
                .find(|job| job.dedup_key == dedup_key && of_project(job, project_id))
                .and_then(view))
        })
        .await
    }

    async fn list(&self, project_id: ProjectId, limit: u32) -> Result<Vec<ExportJob>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut found: Vec<ExportJob> = exports(cx)
                .iter()
                .filter(|job| of_project(job, project_id))
                .filter_map(view)
                .collect();
            found.sort_by_key(|job| std::cmp::Reverse((job.new.created_at, job.new.id)));
            found.truncate(limit as usize);
            Ok(found)
        })
        .await
    }

    async fn count_since(&self, project_id: ProjectId, since: Timestamp) -> Result<u64, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(exports(cx)
                .iter()
                .filter(|job| of_project(job, project_id))
                .filter_map(|job| job.export.as_ref())
                .filter(|export| export.new.created_at >= since)
                .count() as u64)
        })
        .await
    }

    async fn get_many(&self, ids: &[ExportJobId]) -> Result<Vec<ExportJob>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut ids = ids.to_vec();
            ids.sort();
            ids.dedup();
            Ok(ids
                .into_iter()
                .filter_map(|id| one!(cx, Tbl::Jobs, &key(id), Row::Job))
                .filter_map(|job| view(&job))
                .collect())
        })
        .await
    }

    async fn progress(&self, claim: &Claim, spans: u64, bytes: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            under_claim(
                cx,
                claim,
                |_| true,
                |export| {
                    export.spans = spans;
                    export.bytes = bytes;
                },
            )
        })
        .await
    }

    async fn commit(&self, claim: &Claim, commit: &ExportCommit) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            under_claim(cx, claim, open, |export| {
                export.file = Some(commit.file.clone());
                export.checksum = Some(commit.checksum.clone());
                export.spans = commit.spans;
                export.bytes = commit.bytes;
                export.skipped = commit.skipped;
                export.expires_at = Some(commit.expires_at);
            })
        })
        .await
    }

    async fn fail(&self, claim: &Claim, error: &str) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            under_claim(cx, claim, open, |export| {
                export.error = Some(error.to_owned());
            })
        })
        .await
    }

    async fn expire_before(
        &self,
        project_id: ProjectId,
        before: Timestamp,
        now: Timestamp,
    ) -> Result<Vec<ExportJob>, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut expired = Vec::new();
            for mut job in exports(cx) {
                let Some(export) = job.export.as_deref_mut() else {
                    continue;
                };
                let live = export.expires_at.is_none_or(|at| at > now);
                if export.new.project_id != project_id || export.new.as_of >= before || !live {
                    continue;
                }
                export.expires_at = Some(now);
                let id = ExportJobId::from_uuid(job.id);
                if let Some(view) = view(&job) {
                    expired.push(view);
                }
                cx.put(Tbl::Jobs, key(id), Some(Row::Job(job)))?;
            }
            expired.sort_by_key(|job| job.new.id);
            Ok(expired)
        })
        .await
    }
}
