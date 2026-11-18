//! Claimed rows (contract 11.7): due order, `lease_until`, the attempt counter and skip-locked.

use std::time::Duration;

use anyhow::{Result, ensure};
use crucible_control::{Claim, NewJob, Queue};
use crucible_records::Uuid;

use super::{Ctl, Transcript, id};

const QUEUE: Queue = Queue::NotificationOutbox;
const LEASE: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_millis(400);

fn shape(claims: &[Claim]) -> Vec<(String, u32)> {
    claims
        .iter()
        .map(|claim| (claim.payload.clone(), claim.attempt))
        .collect()
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let claims = s.claims();
    let now = s.db_now().await?;
    let job = |n: u64, due_in_secs: i64| NewJob {
        id: id(n),
        dedup_key: format!("alert-{n}"),
        payload: format!("job {n}"),
        run_at: now.saturating_add_micros(due_in_secs * 1_000_000),
    };
    // Due order is by time then id: job 2 is the oldest, job 4 is not due yet.
    for new in [job(1, -10), job(2, -20), job(3, -5), job(4, 3_600)] {
        t.outcome("enqueue", claims.enqueue(QUEUE, &new).await);
    }
    let mut clash = job(5, -1);
    clash.dedup_key = "alert-1".to_owned();
    t.outcome(
        "enqueue same dedup key",
        claims.enqueue(QUEUE, &clash).await,
    );
    t.outcome("enqueue same id", claims.enqueue(QUEUE, &job(1, -1)).await);

    let first = claims.claim(QUEUE, 1, LEASE).await?;
    ensure!(
        first.iter().all(|claim| claim.lease_until > now),
        "a claim's lease ends after the database clock"
    );
    t.note("claim one", shape(&first));
    let rest = claims.claim(QUEUE, 10, LEASE).await?;
    t.note("claim the rest", shape(&rest));
    t.outcome(
        "nothing left to claim",
        claims.claim(QUEUE, 10, LEASE).await.map(|c| shape(&c)),
    );
    let by = |payload: &str| {
        first
            .iter()
            .chain(&rest)
            .find(|claim| claim.payload == payload)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{payload} was not claimed"))
    };
    let (two, one, three) = (by("job 2")?, by("job 1")?, by("job 3")?);

    t.outcome("complete", claims.complete(&two).await);
    t.outcome("complete again", claims.complete(&two).await);
    t.outcome("fail after complete", claims.fail(&two, now).await);
    // A failed row is due again at the time given, and its next claim is attempt 2.
    t.outcome(
        "fail with a retry in the past",
        claims
            .fail(&one, now.saturating_add_micros(-1_000_000))
            .await,
    );
    let again = claims.claim(QUEUE, 10, SHORT).await?;
    t.note("claim after the failure", shape(&again));
    t.outcome(
        "the older attempt cannot complete",
        claims.complete(&one).await,
    );

    // A claim whose lease ran out is claimed again; the late claimer cannot finish the newer attempt.
    ctl.sleep(SHORT * 2).await;
    let reclaimed = claims.claim(QUEUE, 10, LEASE).await?;
    t.note("claim after the lease ran out", shape(&reclaimed));
    for stale in &again {
        t.outcome("late complete", claims.complete(stale).await);
    }
    for live in &reclaimed {
        t.outcome("complete", claims.complete(live).await);
    }
    t.outcome(
        "job 3 is given back",
        claims
            .fail(&three, now.saturating_add_micros(-2_000_000))
            .await,
    );

    // Skip locked: a row claimed by an open transaction is passed over, not waited for.
    for new in [job(6, -30), job(7, -29)] {
        claims.enqueue(QUEUE, &new).await?;
    }
    let tx = s.begin().await?;
    let held = tx.claims().claim(QUEUE, 1, LEASE).await?;
    t.note("claimed inside a transaction", shape(&held));
    let others = claims.claim(QUEUE, 10, LEASE).await?;
    t.note("claimed outside meanwhile", shape(&others));
    tx.rollback().await;
    let after = claims.claim(QUEUE, 10, LEASE).await?;
    t.note("claimed after the rollback", shape(&after));
    let ids: Vec<Uuid> = after.iter().map(|claim| claim.id).collect();
    ensure!(
        ids == held.iter().map(|claim| claim.id).collect::<Vec<_>>(),
        "a rolled-back claim leaves its row claimable, with the attempt counter untouched"
    );
    Ok(())
}
