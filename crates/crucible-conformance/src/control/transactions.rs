//! Transactions: commit and rollback visibility, snapshot reads, the serialization failure of a write-write
//! race, constraint errors inside a transaction, and the generic wrapper.

use anyhow::{Result, ensure};
use crucible_control::{OrgMembership, OrgRole, TxError, transaction};

use super::{Ctl, Transcript, project, user};

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let (ada, bob) = (user(10)?, user(11)?);
    let web = project(20);
    s.projects().insert(&web).await?;

    // Nothing is visible outside before the commit; everything is after it.
    let tx = s.begin().await?;
    tx.users().insert(&ada).await?;
    tx.memberships()
        .put_org(OrgMembership {
            user_id: ada.id,
            role: OrgRole::Owner,
        })
        .await?;
    t.outcome(
        "inside sees its write",
        tx.users().get(ada.id).await.map(|u| u.is_some()),
    );
    t.outcome(
        "outside before commit",
        s.users().get(ada.id).await.map(|u| u.is_some()),
    );
    t.outcome("commit", tx.commit().await);
    t.note(
        "outside after commit",
        (
            s.users().get(ada.id).await?.is_some(),
            s.memberships().org_role(ada.id).await?,
        ),
    );

    // A rollback and a dropped transaction leave nothing.
    let tx = s.begin().await?;
    tx.users().insert(&bob).await?;
    tx.rollback().await;
    let tx = s.begin().await?;
    tx.users().insert(&bob).await?;
    drop(tx);
    t.outcome(
        "after rollback and drop",
        s.users().get(bob.id).await.map(|u| u.is_some()),
    );

    // A transaction reads the snapshot of its first statement, and loses a write-write race.
    let tx = s.begin().await?;
    let seen = tx.projects().get(web.id).await?;
    let mut renamed = web.clone();
    renamed.name = "Renamed outside".to_owned();
    t.outcome(
        "concurrent update outside",
        s.projects().update_if_version(&renamed, 1).await,
    );
    let again = tx.projects().get(web.id).await?;
    ensure!(seen == again, "a transaction keeps reading its snapshot");
    t.note("snapshot read", again.map(|p| (p.name, p.version)));
    let mut inside = web.clone();
    inside.name = "Renamed inside".to_owned();
    let lost = tx.projects().update_if_version(&inside, 1).await;
    ensure!(
        lost == Err(TxError::SerializationFailure),
        "writing a row that changed since the snapshot is a serialization failure, got {lost:?}"
    );
    t.outcome("write after concurrent update", lost);
    t.outcome(
        "statement after the failure",
        tx.users().get(ada.id).await.map(|u| u.is_some()),
    );
    t.outcome("commit after the failure", tx.commit().await);
    t.outcome(
        "the outside update stands",
        s.projects()
            .get(web.id)
            .await
            .map(|p| p.map(|p| (p.name, p.version))),
    );

    // A constraint error fails the transaction; nothing of it is applied.
    let tx = s.begin().await?;
    tx.users().insert(&bob).await?;
    let mut clash = user(12)?;
    clash.email = ada.email.clone();
    t.outcome("unique violation inside", tx.users().insert(&clash).await);
    t.outcome("commit after the violation", tx.commit().await);
    t.outcome(
        "nothing of it applied",
        s.users().get(bob.id).await.map(|u| u.is_some()),
    );

    // A row inserted and committed outside after the snapshot still collides on its key.
    let tx = s.begin().await?;
    t.outcome("snapshot taken", tx.users().list().await.map(|u| u.len()));
    s.users().insert(&bob).await?;
    t.outcome(
        "inside cannot see it",
        tx.users().get(bob.id).await.map(|u| u.is_some()),
    );
    t.outcome(
        "inside insert of the same id",
        tx.users().insert(&bob).await,
    );
    tx.rollback().await;

    // The wrapper commits on Ok and rolls back on Err.
    let carol = user(13)?;
    let done = transaction(s, |tx| {
        let carol = carol.clone();
        Box::pin(async move {
            tx.users().insert(&carol).await?;
            tx.users().list().await.map(|users| users.len())
        })
    })
    .await;
    t.outcome("wrapper commit", done);
    let dave = user(14)?;
    let refused: Result<(), TxError> = transaction(s, |tx| {
        let dave = dave.clone();
        Box::pin(async move {
            tx.users().insert(&dave).await?;
            Err(TxError::Fenced)
        })
    })
    .await;
    t.outcome("wrapper rollback", refused);
    t.outcome(
        "users at the end",
        s.users()
            .list()
            .await
            .map(|users| users.into_iter().map(|u| u.id).collect::<Vec<_>>()),
    );
    Ok(())
}
