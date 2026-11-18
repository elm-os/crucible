//! API keys and the key feed (contract 11.8): every change writes a feed row in its own statement, sequence
//! numbers become visible out of order, and the overlap window finds the late ones.

use std::time::Duration;

use anyhow::{Result, ensure};
use crucible_control::{KeyChange, KeyChangeKind, KeyScope};

use super::{Ctl, Transcript, api_key, at, project};

/// Longer than any transaction of this script.
const WIDE: Duration = Duration::from_secs(3_600);

/// A feed row without its sequence number and time.
fn row(change: &KeyChange) -> (String, KeyChangeKind, u64, u64) {
    (
        change.key.note.clone(),
        change.change,
        change.key_version,
        change.key.version,
    )
}

fn rows(changes: &[KeyChange]) -> Vec<(String, KeyChangeKind, u64, u64)> {
    changes.iter().map(row).collect()
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let web = project(20);
    s.projects().insert(&web).await?;
    let keys = s.api_keys();
    let feed = s.key_feed();

    t.outcome(
        "empty snapshot",
        feed.snapshot()
            .await
            .map(|snap| (snap.max_seq, snap.keys.len())),
    );
    let (mut one, mut two) = (api_key(1, web.id), api_key(2, web.id));
    t.outcome("key insert", keys.insert(&one).await);
    t.outcome("key insert 2", keys.insert(&two).await);
    t.outcome("key same id", keys.insert(&one).await);
    let mut clash = api_key(3, web.id);
    clash.public_key = one.public_key.clone();
    t.outcome("key same public key", keys.insert(&clash).await);
    t.outcome(
        "key without project",
        keys.insert(&api_key(4, project(98).id)).await,
    );

    one.scopes = vec![KeyScope::Ingest];
    one.note = "key 1, ingest only".to_owned();
    t.outcome(
        "key change at 1",
        keys.update_if_version(&one, 1, KeyChangeKind::Changed)
            .await,
    );
    t.outcome(
        "key change at 1 again",
        keys.update_if_version(&one, 1, KeyChangeKind::Changed)
            .await,
    );
    two.expires_at = Some(at(86_400));
    t.outcome(
        "key expiry edit",
        keys.update_if_version(&two, 1, KeyChangeKind::ExpiryEdited)
            .await,
    );
    two.revoked_at = Some(at(50));
    t.outcome(
        "key revoke",
        keys.update_if_version(&two, 2, KeyChangeKind::Revoked)
            .await,
    );
    // A use is not a change: no version, no feed row.
    t.outcome("key last used", keys.set_last_used(one.id, at(55)).await);
    t.outcome(
        "key last used, unknown key",
        keys.set_last_used(api_key(9, web.id).id, at(55)).await,
    );
    t.outcome("key get", keys.get(two.id).await);
    t.outcome(
        "keys of the project",
        keys.list(web.id).await.map(|keys| {
            keys.into_iter()
                .map(|k| (k.note, k.version, k.last_used_at))
                .collect::<Vec<_>>()
        }),
    );

    let all = feed.poll(0, Duration::ZERO).await?;
    ensure!(
        all.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "a poll is ordered by sequence number"
    );
    ensure!(
        all.len() == 5,
        "five changes wrote five feed rows, found {}",
        all.len()
    );
    ensure!(
        all.iter()
            .all(|change| change.key.version >= change.key_version),
        "a feed row carries the key as it is now"
    );
    t.note("feed from the start", rows(&all));
    let last = all.last().map_or(0, |change| change.seq);
    t.outcome(
        "feed after the last row, no overlap",
        feed.poll(last, Duration::ZERO).await.map(|c| c.len()),
    );
    t.outcome(
        "feed after the last row, wide overlap",
        feed.poll(last, WIDE).await.map(|c| rows(&c)),
    );
    let snapshot = feed.snapshot().await?;
    ensure!(
        snapshot.max_seq == last,
        "the snapshot names the last feed row"
    );
    t.note(
        "snapshot",
        snapshot
            .keys
            .iter()
            .map(|k| (k.note.clone(), k.version, k.revoked_at))
            .collect::<Vec<_>>(),
    );

    // Out of order: a transaction draws its sequence number first and commits last.
    let slow = s.begin().await?;
    slow.api_keys().insert(&api_key(5, web.id)).await?;
    keys.insert(&api_key(6, web.id)).await?;
    let seen = feed.poll(last, Duration::ZERO).await?;
    t.note("poll while the older number is uncommitted", rows(&seen));
    let cursor = seen.last().map_or(last, |change| change.seq);
    slow.commit().await?;
    let narrow = feed.poll(cursor, Duration::ZERO).await?;
    t.note("poll after the cursor, no overlap", rows(&narrow));
    let wide = feed.poll(cursor, WIDE).await?;
    let late: Vec<_> = wide.iter().filter(|change| change.seq < cursor).collect();
    ensure!(
        narrow.is_empty() && late.iter().any(|change| change.key.note == "key 5"),
        "only the overlap window finds a row that committed under the cursor"
    );
    t.note(
        "rows under the cursor found by the overlap",
        late.iter()
            .filter(|c| c.seq > last)
            .map(|c| row(c))
            .collect::<Vec<_>>(),
    );

    // A rolled-back change leaves a gap and no row.
    let undone = s.begin().await?;
    undone.api_keys().insert(&api_key(7, web.id)).await?;
    undone.rollback().await;
    keys.insert(&api_key(8, web.id)).await?;
    let end = feed.poll(0, Duration::ZERO).await?;
    ensure!(
        end.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "sequence numbers stay unique and ordered"
    );
    let newest = end.last().map_or(0, |change| change.seq);
    t.note("the rollback left a gap", newest > cursor.max(last) + 1);
    t.note("feed at the end", rows(&end));
    Ok(())
}
