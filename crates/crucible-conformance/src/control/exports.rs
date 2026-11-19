//! Export jobs (contracts 11.1, 11.7 and 12.8): the rows of `export_jobs`, which are also the claimed queue of
//! the export job. Every statement of `ExportJobRepo`: inserts and their constraints, the reads by id, by
//! deduplication key, by project and by id list, progress, commit and failure under the claim's attempt, and
//! the expiry a deletion request makes.

use std::time::Duration;

use anyhow::{Result, ensure};
use crucible_control::{Claim, ExportCommit, ExportFormat, ExportJob, NewExport, Queue, TxError};
use crucible_records::{ExportJobId, ProjectId, Timestamp};

use super::{Ctl, Transcript, id, project};

const LEASE: Duration = Duration::from_secs(30);

/// A job as both stores must report it: no raw database time.
fn shape(job: &ExportJob) -> String {
    format!(
        "{} {} {} {:?} attempts {} lease {} spans {} bytes {} skipped {} file {:?} checksum {:?} expires {} \
         error {:?}",
        job.new.dedup_key,
        job.new.format,
        job.new.selector,
        job.status,
        job.attempts,
        job.lease_until.is_some(),
        job.spans,
        job.bytes,
        job.skipped,
        job.file,
        job.checksum,
        job.expires_at.is_some(),
        job.error,
    )
}

fn shapes(result: Result<Vec<ExportJob>, TxError>) -> Result<Vec<String>, TxError> {
    result.map(|jobs| jobs.iter().map(shape).collect())
}

fn new_export(n: u64, project_id: ProjectId, created_at: Timestamp) -> NewExport {
    NewExport {
        id: ExportJobId::from_uuid(id(n)),
        project_id,
        format: if n.is_multiple_of(2) {
            ExportFormat::OtlpJson
        } else {
            ExportFormat::CrucibleJsonl
        },
        selector: format!("{{\"n\":{n}}}"),
        options: "{}".to_owned(),
        dedup_key: format!("{project_id}:key-{n}"),
        created_by: format!("user:{n}"),
        created_at,
        as_of: created_at,
    }
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let (one, two) = (project(301), project(302));
    s.projects().insert(&one).await?;
    s.projects().insert(&two).await?;
    let exports = s.export_jobs();
    let now = s.db_now().await?;
    let ago = |secs: i64| now.saturating_add_micros(-secs * 1_000_000);

    // Three exports of project one, created a minute, a second and an hour ago; one of project two.
    for (n, project_id, created) in [
        (1, one.id, ago(60)),
        (2, one.id, ago(1)),
        (3, one.id, ago(3_600)),
        (4, two.id, ago(5)),
    ] {
        t.outcome(
            "insert",
            exports.insert(&new_export(n, project_id, created)).await,
        );
    }
    let mut clash = new_export(5, one.id, ago(1));
    clash.dedup_key = format!("{}:key-1", one.id);
    t.outcome(
        "insert a taken deduplication key",
        exports.insert(&clash).await,
    );
    t.outcome(
        "insert a taken id",
        exports.insert(&new_export(1, one.id, ago(1))).await,
    );
    let missing = ProjectId::from_uuid(id(399));
    t.outcome(
        "insert for a missing project",
        exports.insert(&new_export(6, missing, ago(1))).await,
    );

    let job1 = ExportJobId::from_uuid(id(1));
    t.outcome(
        "get",
        exports
            .get(one.id, job1)
            .await
            .map(|j| j.as_ref().map(shape)),
    );
    t.outcome(
        "get under another project",
        exports.get(two.id, job1).await.map(|j| j.is_some()),
    );
    let key = format!("{}:key-2", one.id);
    t.outcome(
        "get by deduplication key",
        exports
            .get_by_dedup_key(one.id, &key)
            .await
            .map(|j| j.as_ref().map(shape)),
    );
    t.outcome(
        "get by deduplication key under another project",
        exports
            .get_by_dedup_key(two.id, &key)
            .await
            .map(|j| j.is_some()),
    );
    t.outcome("list, newest first", shapes(exports.list(one.id, 10).await));
    t.outcome("list, at most two", shapes(exports.list(one.id, 2).await));
    t.outcome(
        "count in the last ten minutes",
        exports.count_since(one.id, ago(600)).await,
    );
    let ids: Vec<ExportJobId> = [4, 1, 9, 2].map(|n| ExportJobId::from_uuid(id(n))).to_vec();
    t.outcome(
        "get many, ordered by id",
        shapes(exports.get_many(&ids).await),
    );
    t.outcome("get many of none", shapes(exports.get_many(&[]).await));

    // The rows are the export job's queue: claimed oldest first, then progress and the outcome under the
    // claim's attempt.
    let claims = s.claims();
    let first = claims.claim(Queue::ExportJobs, 1, LEASE).await?;
    ensure!(first.len() == 1, "one export is claimed");
    let claim: &Claim = &first[0];
    t.note(
        "claimed",
        (claim.id == id(3), claim.attempt, &claim.payload),
    );
    t.outcome("progress", exports.progress(claim, 10, 1_000).await);
    let stale = Claim {
        attempt: claim.attempt + 1,
        ..claim.clone()
    };
    t.outcome(
        "progress of another attempt",
        exports.progress(&stale, 11, 1_100).await,
    );
    t.outcome(
        "the claimed row",
        exports
            .get(one.id, ExportJobId::from_uuid(id(3)))
            .await
            .map(|j| j.as_ref().map(shape)),
    );
    let commit = ExportCommit {
        file: format!("{}/{}.1.jsonl", one.id, ExportJobId::from_uuid(id(3))),
        checksum: "ab".repeat(32),
        spans: 12,
        bytes: 1_234,
        skipped: 1,
        expires_at: now.saturating_add_micros(86_400_000_000),
    };
    t.outcome(
        "commit of another attempt",
        exports.commit(&stale, &commit).await,
    );
    t.outcome("commit", exports.commit(claim, &commit).await);
    t.outcome("commit again", exports.commit(claim, &commit).await);
    t.outcome(
        "fail after the commit",
        exports.fail(claim, "query_timeout").await,
    );
    t.outcome("complete", claims.complete(claim).await);

    let second = claims.claim(Queue::ExportJobs, 1, LEASE).await?;
    ensure!(second.len() == 1, "a second export is claimed");
    t.note("claimed next", (second[0].id == id(1), second[0].attempt));
    t.outcome("fail", exports.fail(&second[0], "query_too_broad").await);
    t.outcome(
        "commit after the failure",
        exports.commit(&second[0], &commit).await,
    );
    t.outcome(
        "the failed row",
        exports
            .get(one.id, job1)
            .await
            .map(|j| j.as_ref().map(shape)),
    );

    // A deletion request accepted now expires every export of project one created before it.
    t.outcome(
        "expire before now",
        shapes(exports.expire_before(one.id, now, now).await),
    );
    t.outcome(
        "expire again",
        shapes(exports.expire_before(one.id, now, now).await),
    );
    t.outcome(
        "project two is untouched",
        exports
            .get(two.id, ExportJobId::from_uuid(id(4)))
            .await
            .map(|j| j.as_ref().map(shape)),
    );
    let third = claims.claim(Queue::ExportJobs, 10, LEASE).await?;
    // Export 4 (project two) is due first, then export 2, which the expiry closed.
    for claim in &third {
        let which = if claim.id == id(2) {
            "export 2"
        } else {
            "export 4"
        };
        t.outcome(
            &format!("commit of {which} after the expiry"),
            exports.commit(claim, &commit).await,
        );
    }
    Ok(())
}
