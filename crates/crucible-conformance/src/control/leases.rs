//! Leases with fencing tokens (contract 11.7): acquisition, renewal, release, expiry on the database clock,
//! fenced writes, and an acquisition that waits for a fenced transaction.

use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use crucible_control::{Lease, LeaseName, TxError};
use crucible_records::Timestamp;

use super::{Ctl, Transcript};

const TTL: Duration = Duration::from_secs(15);
const SHORT: Duration = Duration::from_millis(400);
const NAME: LeaseName = LeaseName::Lifecycle;

/// A lease without its expiry, plus whether the expiry lies within the ttl of the database clock.
fn shape(lease: &Lease, now: Timestamp, ttl: Duration) -> (String, u64, bool) {
    let ttl = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
    let left = lease.expires_at.micros() - now.micros();
    (
        lease.holder.clone(),
        lease.fencing_token,
        left > 0 && left <= ttl,
    )
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let leases = s.leases();
    let now = s.db_now().await?;

    t.outcome("no lease yet", leases.get(NAME).await);
    let a = leases
        .acquire(NAME, "node-a", TTL)
        .await?
        .context("a free lease is acquired")?;
    t.note("a acquires", shape(&a, now, TTL + Duration::from_secs(5)));
    t.outcome(
        "b is refused while a holds",
        leases
            .acquire(NAME, "node-b", TTL)
            .await
            .map(|l| l.is_some()),
    );
    t.outcome(
        "a cannot acquire twice",
        leases
            .acquire(NAME, "node-a", TTL)
            .await
            .map(|l| l.is_some()),
    );
    t.outcome(
        "another lease is independent",
        leases
            .acquire(LeaseName::Housekeeping, "node-b", TTL)
            .await
            .map(|l| l.map(|l| l.fencing_token)),
    );
    let renewed = leases.renew(&a, TTL).await?.context("a renews its lease")?;
    ensure!(
        renewed.fencing_token == a.fencing_token && renewed.expires_at >= a.expires_at,
        "a renewal keeps the token and moves the expiry forward"
    );
    t.outcome("a writes its cursor", leases.set_cursor(&a, "a-1").await);
    t.outcome("cursor", leases.cursor(NAME).await);

    // Release keeps the row and the token; the next holder gets the next token.
    t.outcome("a releases", leases.release(&a).await);
    t.outcome(
        "a cannot renew after release",
        leases.renew(&a, TTL).await.map(|l| l.is_some()),
    );
    let b = leases
        .acquire(NAME, "node-b", SHORT)
        .await?
        .context("a released lease is acquired")?;
    ensure!(
        b.fencing_token == a.fencing_token + 1,
        "the token increases by one"
    );
    t.note("b acquires", (b.holder.clone(), b.fencing_token));
    t.outcome(
        "a's stale write is fenced",
        leases.set_cursor(&a, "a-2").await,
    );
    t.outcome("a's stale release", leases.release(&a).await);
    t.outcome("b writes its cursor", leases.set_cursor(&b, "b-1").await);

    // Expiry is the database clock: after the ttl another node takes over, and the old holder is fenced.
    ctl.sleep(SHORT * 2).await;
    t.outcome(
        "b cannot renew after expiry",
        leases.renew(&b, TTL).await.map(|l| l.is_some()),
    );
    let c = leases
        .acquire(NAME, "node-c", TTL)
        .await?
        .context("an expired lease is acquired")?;
    t.note("c acquires", (c.holder.clone(), c.fencing_token));
    t.outcome(
        "b's stale write is fenced",
        leases.set_cursor(&b, "b-2").await,
    );
    t.outcome("cursor", leases.cursor(NAME).await);

    // Fenced transactions.
    let tx = s.begin().await?;
    t.outcome("stale fence", tx.fence(&b).await);
    t.outcome("commit after a stale fence", tx.commit().await);
    let tx = s.begin().await?;
    t.outcome("live fence", tx.fence(&c).await);
    t.outcome("fenced write", tx.leases().set_cursor(&c, "c-1").await);
    t.outcome("fenced commit", tx.commit().await);
    t.outcome("cursor", leases.cursor(NAME).await);

    // A fenced transaction holds off a new acquisition until it ends, so its writes and the token check are
    // one atomic step: the lease is released underneath it, and the acquisition still waits.
    let c = leases.renew(&c, SHORT).await?.context("c renews")?;
    let tx = s.begin().await?;
    tx.fence(&c).await?;
    tx.leases().set_cursor(&c, "c-2").await?;
    ctl.sleep(SHORT * 2).await;
    let (acquired, committed) = futures::join!(
        async {
            let lease = leases.acquire(NAME, "node-d", TTL).await;
            (lease, s.leases().cursor(NAME).await)
        },
        async {
            ctl.sleep(Duration::from_millis(300)).await;
            tx.commit().await
        }
    );
    t.outcome("fenced commit under an expired lease", committed);
    let (d, cursor_seen) = acquired;
    let d = d?.context("d acquires once the fenced transaction ended")?;
    ensure!(
        cursor_seen.as_ref().ok().and_then(Option::as_deref) == Some("c-2"),
        "the acquisition returned only after the fenced write committed, cursor {cursor_seen:?}"
    );
    t.note(
        "d acquires after the fenced commit",
        (d.holder.clone(), d.fencing_token),
    );
    let stale: Result<(), TxError> = leases.set_cursor(&c, "c-3").await;
    t.outcome("c is fenced after d took over", stale);
    t.outcome(
        "the lease row",
        leases
            .get(NAME)
            .await
            .map(|l| l.map(|l| (l.holder, l.fencing_token))),
    );
    Ok(())
}
