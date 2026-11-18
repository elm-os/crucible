//! The statements of the job runners (contract 11.7 and 11.8): parking a claimed row at its attempt cap, and
//! pruning the key feed.

use std::time::Duration;

use anyhow::Result;
use crucible_control::{Claim, KeyChange, KeyChangeKind, NewJob, Queue};

use super::{Ctl, Transcript, api_key, id, project};

const QUEUE: Queue = Queue::NotificationOutbox;
const LEASE: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_millis(400);
/// Longer than this script runs.
const WIDE: Duration = Duration::from_secs(3_600);

fn shape(claims: &[Claim]) -> Vec<(String, u32)> {
    claims
        .iter()
        .map(|claim| (claim.payload.clone(), claim.attempt))
        .collect()
}

/// Feed rows without their sequence numbers and times.
fn rows(changes: &[KeyChange]) -> Vec<(String, KeyChangeKind, u64)> {
    changes
        .iter()
        .map(|change| (change.key.note.clone(), change.change, change.key_version))
        .collect()
}

async fn park(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let claims = s.claims();
    let now = s.db_now().await?;
    let job = |n: u64, due_in_secs: i64| NewJob {
        id: id(n),
        dedup_key: format!("alert-{n}"),
        payload: format!("job {n}"),
        run_at: now.saturating_add_micros(due_in_secs * 1_000_000),
    };
    for new in [job(1, -10), job(2, -5)] {
        claims.enqueue(QUEUE, &new).await?;
    }
    let first = claims.claim(QUEUE, 1, LEASE).await?;
    t.note("claim one", shape(&first));
    for claim in &first {
        t.outcome("park", claims.park(claim).await);
        t.outcome("park again", claims.park(claim).await);
        t.outcome("complete after park", claims.complete(claim).await);
        t.outcome("fail after park", claims.fail(claim, now).await);
    }
    // A parked row is never claimed again, also after its claim would have run out.
    let rest = claims.claim(QUEUE, 10, SHORT).await?;
    t.note("claim the rest", shape(&rest));
    ctl.sleep(SHORT * 2).await;
    let again = claims.claim(QUEUE, 10, LEASE).await?;
    t.note("claim after the lease ran out", shape(&again));
    for stale in &rest {
        t.outcome("the older attempt cannot park", claims.park(stale).await);
    }
    for live in &again {
        t.outcome("park the newer attempt", claims.park(live).await);
    }
    t.outcome(
        "nothing left to claim",
        claims.claim(QUEUE, 10, LEASE).await.map(|c| shape(&c)),
    );

    // Parking is part of its transaction.
    claims.enqueue(QUEUE, &job(3, -1)).await?;
    let tx = s.begin().await?;
    let held = tx.claims().claim(QUEUE, 1, LEASE).await?;
    for claim in &held {
        t.outcome("park inside a transaction", tx.claims().park(claim).await);
    }
    tx.rollback().await;
    t.outcome(
        "claimed after the rollback",
        claims.claim(QUEUE, 10, LEASE).await.map(|c| shape(&c)),
    );
    Ok(())
}

async fn prune(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let web = project(20);
    s.projects().insert(&web).await?;
    let keys = s.api_keys();
    let feed = s.key_feed();
    let (one, two) = (api_key(1, web.id), api_key(2, web.id));
    keys.insert(&one).await?;
    keys.insert(&two).await?;
    t.outcome("prune an empty window", feed.prune(WIDE).await);
    ctl.sleep(SHORT * 2).await;
    let mut changed = one.clone();
    changed.note = "key 1, changed".to_owned();
    keys.update_if_version(&changed, 1, KeyChangeKind::Changed)
        .await?;
    keys.insert(&api_key(3, web.id)).await?;

    // The two rows written before the pause are older than the window; the two after it are not.
    t.outcome(
        "prune the rows older than the pause",
        feed.prune(SHORT).await,
    );
    t.outcome(
        "feed after the prune",
        feed.poll(0, Duration::ZERO).await.map(|c| rows(&c)),
    );
    t.outcome("prune again", feed.prune(SHORT).await);
    t.outcome(
        "snapshot keeps every key",
        feed.snapshot().await.map(|snap| snap.keys.len()),
    );

    // Pruning is part of its transaction.
    ctl.sleep(SHORT * 2).await;
    let tx = s.begin().await?;
    t.outcome(
        "prune inside a transaction",
        tx.key_feed().prune(SHORT).await,
    );
    t.outcome(
        "feed inside the transaction",
        tx.key_feed()
            .poll(0, Duration::ZERO)
            .await
            .map(|c| rows(&c)),
    );
    tx.rollback().await;
    t.outcome(
        "feed after the rollback",
        feed.poll(0, Duration::ZERO).await.map(|c| rows(&c)),
    );
    t.outcome("prune everything", feed.prune(Duration::ZERO).await);
    t.outcome(
        "snapshot of an empty feed",
        feed.snapshot()
            .await
            .map(|snap| (snap.max_seq, snap.keys.len())),
    );
    Ok(())
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    park(ctl, t).await?;
    prune(ctl, t).await
}
