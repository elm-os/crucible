//! The export-job repository over Postgres (migration V3): the rows of `export_jobs`, which is also the queue the
//! export job claims (contract 11.7).

use async_trait::async_trait;
use crucible_control::{
    Claim, ClaimStatus, ExportCommit, ExportFormat, ExportJob, ExportJobRepo, NewExport, TxError,
};
use crucible_records::{ExportJobId, ProjectId, Timestamp};

use crate::sql::{Cols, P, int};
use crate::store::{Handle, every, first};

const EXPORT: &str = "id, project_id, format, selector, payload, dedup_key, created_by, created_at, as_of, \
     status, attempts, lease_until, spans, bytes, skipped, file, checksum, expires_at, error";

/// The conditions under which an attempt may still record its outcome.
const UNDER_CLAIM: &str = "id = $1 AND attempts = $2 AND status = 'pending' AND file IS NULL \
     AND error IS NULL AND expires_at IS NULL";

fn export(c: &mut Cols<'_>) -> Result<ExportJob, TxError> {
    Ok(ExportJob {
        new: NewExport {
            id: ExportJobId::from_uuid(c.uuid()?),
            project_id: ProjectId::from_uuid(c.uuid()?),
            format: c.parsed(ExportFormat::parse)?,
            selector: c.text()?,
            options: c.text()?,
            dedup_key: c.text()?,
            created_by: c.text()?,
            created_at: c.ts()?,
            as_of: c.ts()?,
        },
        status: c.parsed(ClaimStatus::parse)?,
        attempts: c.u32()?,
        lease_until: c.opt_ts()?,
        spans: c.u64()?,
        bytes: c.u64()?,
        skipped: c.u64()?,
        file: c.opt_text()?,
        checksum: c.opt_text()?,
        expires_at: c.opt_ts()?,
        error: c.opt_text()?,
    })
}

#[async_trait]
impl ExportJobRepo for Handle {
    async fn insert(&self, export: &NewExport) -> Result<(), TxError> {
        self.write(
            "INSERT INTO export_jobs (id, project_id, format, selector, created_by, created_at, as_of, \
             dedup_key, payload, attempts, next_attempt_at, status, lease_until, spans, bytes, skipped, file, \
             checksum, expires_at, error) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 0, $6, 'pending', NULL, 0, 0, 0, NULL, NULL, NULL, NULL) \
             RETURNING 1",
            &[
                P::Uuid(export.id.uuid()),
                P::Uuid(export.project_id.uuid()),
                P::Text(export.format.as_str()),
                P::Text(&export.selector),
                P::Text(&export.created_by),
                P::Ts(export.created_at),
                P::Ts(export.as_of),
                P::Text(&export.dedup_key),
                P::Text(&export.options),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get(
        &self,
        project_id: ProjectId,
        id: ExportJobId,
    ) -> Result<Option<ExportJob>, TxError> {
        let rows = self
            .read(
                &format!("SELECT {EXPORT} FROM export_jobs WHERE project_id = $1 AND id = $2"),
                &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid())],
            )
            .await?;
        first(&rows, export)
    }

    async fn get_by_dedup_key(
        &self,
        project_id: ProjectId,
        dedup_key: &str,
    ) -> Result<Option<ExportJob>, TxError> {
        let rows = self
            .read(
                &format!(
                    "SELECT {EXPORT} FROM export_jobs WHERE project_id = $1 AND dedup_key = $2"
                ),
                &[P::Uuid(project_id.uuid()), P::Text(dedup_key)],
            )
            .await?;
        first(&rows, export)
    }

    async fn list(&self, project_id: ProjectId, limit: u32) -> Result<Vec<ExportJob>, TxError> {
        let rows = self
            .read(
                &format!(
                    "SELECT {EXPORT} FROM export_jobs WHERE project_id = $1 \
                     ORDER BY created_at DESC, id DESC LIMIT $2"
                ),
                &[P::Uuid(project_id.uuid()), P::Int(i64::from(limit))],
            )
            .await?;
        every(&rows, export)
    }

    async fn count_since(&self, project_id: ProjectId, since: Timestamp) -> Result<u64, TxError> {
        let rows = self
            .read(
                "SELECT count(*) FROM export_jobs WHERE project_id = $1 AND created_at >= $2",
                &[P::Uuid(project_id.uuid()), P::Ts(since)],
            )
            .await?;
        Ok(first(&rows, |c| c.u64())?.unwrap_or(0))
    }

    async fn get_many(&self, ids: &[ExportJobId]) -> Result<Vec<ExportJob>, TxError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let rows = self
            .read(
                &format!("SELECT {EXPORT} FROM export_jobs WHERE id = ANY($1::uuid[]) ORDER BY id"),
                &[P::TextArray(texts.iter().map(String::as_str).collect())],
            )
            .await?;
        every(&rows, export)
    }

    async fn progress(&self, claim: &Claim, spans: u64, bytes: u64) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE export_jobs SET spans = $3, bytes = $4 \
                 WHERE id = $1 AND attempts = $2 AND status = 'pending' RETURNING 1",
                &[
                    P::Uuid(claim.id),
                    P::Int4(claim.attempt),
                    int(spans)?,
                    int(bytes)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn commit(&self, claim: &Claim, commit: &ExportCommit) -> Result<bool, TxError> {
        let changed = self
            .write(
                &format!(
                    "UPDATE export_jobs SET file = $3, checksum = $4, spans = $5, bytes = $6, skipped = $7, \
                     expires_at = $8 WHERE {UNDER_CLAIM} RETURNING 1"
                ),
                &[
                    P::Uuid(claim.id),
                    P::Int4(claim.attempt),
                    P::Text(&commit.file),
                    P::Text(&commit.checksum),
                    int(commit.spans)?,
                    int(commit.bytes)?,
                    int(commit.skipped)?,
                    P::Ts(commit.expires_at),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn fail(&self, claim: &Claim, error: &str) -> Result<bool, TxError> {
        let changed = self
            .write(
                &format!("UPDATE export_jobs SET error = $3 WHERE {UNDER_CLAIM} RETURNING 1"),
                &[P::Uuid(claim.id), P::Int4(claim.attempt), P::Text(error)],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn expire_before(
        &self,
        project_id: ProjectId,
        before: Timestamp,
        now: Timestamp,
    ) -> Result<Vec<ExportJob>, TxError> {
        let rows = self
            .query_main(
                true,
                &format!(
                    "UPDATE export_jobs SET expires_at = $3 WHERE project_id = $1 AND as_of < $2 \
                     AND (expires_at IS NULL OR expires_at > $3) RETURNING {EXPORT}"
                ),
                &[P::Uuid(project_id.uuid()), P::Ts(before), P::Ts(now)],
            )
            .await?;
        let mut expired = every(&rows, export)?;
        expired.sort_by_key(|job| job.new.id);
        Ok(expired)
    }
}
