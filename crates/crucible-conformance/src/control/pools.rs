//! The two pools (contract 2.7): when transactions hold every main connection, calls on the main pool fail as
//! unavailable after the wait, and the reserved pool (the key feed, leases, session lookups, the database
//! clock) still answers.

use anyhow::{Result, ensure};
use crucible_control::{LeaseName, TxError};

use super::{Ctl, MAIN_POOL, Transcript};

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let mut open = Vec::new();
    for _ in 0..MAIN_POOL {
        open.push(s.begin().await?);
    }
    let refused = s.begin().await.map(|_| ());
    ensure!(
        matches!(refused, Err(TxError::Unavailable(_))),
        "a transaction beyond the pool fails as unavailable, got {refused:?}"
    );
    t.outcome("begin beyond the main pool", refused);
    t.outcome(
        "a main-pool statement",
        s.users().list().await.map(|users| users.len()),
    );
    t.outcome(
        "a session lookup",
        s.sessions().get(&[1; 32]).await.map(|s| s.is_some()),
    );
    t.outcome(
        "a lease read",
        s.leases()
            .get(LeaseName::Lifecycle)
            .await
            .map(|l| l.is_some()),
    );
    t.outcome(
        "a feed poll",
        s.key_feed()
            .poll(0, std::time::Duration::ZERO)
            .await
            .map(|c| c.len()),
    );
    t.outcome("the database clock", s.db_now().await.map(|_| ()));
    for tx in open {
        tx.rollback().await;
    }
    t.outcome(
        "begin after the pool drained",
        match s.begin().await {
            Ok(tx) => {
                tx.rollback().await;
                Ok(())
            }
            Err(error) => Err(error),
        },
    );
    Ok(())
}
