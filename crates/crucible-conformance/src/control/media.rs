//! Media (contracts 11.1 and 14.3): the rows of `media_objects` and `media_refs`. Every statement of
//! `MediaRepo`: inserts and their constraints, the reads by id, hash and hash range, re-arming, the confirm,
//! uses and their reads, the deletes by trace and by time, the collector's reads and deletes, and the foreign
//! key that keeps a used object's row: a delete that races a new use, in both orders, fails.

use anyhow::{Result, ensure};
use crucible_control::{MediaObject, MediaRef, MediaState, MediaUse, TxError};
use crucible_records::{MediaObjectId, ProjectId, SpanId, TraceId};

use super::{Ctl, Transcript, at, id, project};

/// An object as both stores must report it.
fn shape(object: &MediaObject) -> String {
    format!(
        "{} {} {} {} {} created {} uploaded {:?}",
        object.id,
        object.sha256[0],
        object.content_type,
        object.len,
        object.state,
        object.created_at,
        object.uploaded_at.map(|t| t.to_string()),
    )
}

fn shapes(result: Result<Vec<MediaObject>, TxError>) -> Result<Vec<String>, TxError> {
    result.map(|objects| objects.iter().map(shape).collect())
}

fn use_shape(usage: &MediaUse) -> String {
    format!(
        "{} trace {} span {:?} field {:?} at {}",
        usage.object.id,
        usage.reference.trace_id.as_bytes()[0],
        usage.reference.span_id.map(|id| id.to_string()),
        usage.reference.field,
        usage.reference.target_time,
    )
}

fn object(n: u64, project_id: ProjectId, hash: u8, state: MediaState) -> MediaObject {
    MediaObject {
        id: MediaObjectId::from_uuid(id(n)),
        project_id,
        sha256: [hash; 32],
        content_type: "image/png".to_owned(),
        len: 100 + n,
        state,
        created_at: at(i64::from(hash)),
        uploaded_at: (state == MediaState::Uploaded).then(|| at(1_000)),
    }
}

fn trace(n: u8) -> TraceId {
    TraceId::from_bytes([n; 16])
}

fn reference(
    object: &MediaObject,
    trace_n: u8,
    span: Option<u8>,
    field: &str,
    secs: i64,
) -> MediaRef {
    MediaRef {
        project_id: object.project_id,
        media_id: object.id,
        trace_id: trace(trace_n),
        span_id: span.map(|n| SpanId::from_bytes([n; 8])),
        field: field.to_owned(),
        target_time: at(secs),
    }
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let (one, two) = (project(401), project(402));
    s.projects().insert(&one).await?;
    s.projects().insert(&two).await?;
    let media = s.media();

    // Objects and their constraints.
    let a = object(1, one.id, 0x10, MediaState::Pending);
    let b = object(2, one.id, 0x20, MediaState::Uploaded);
    t.outcome("insert a pending object", media.insert_object(&a).await);
    t.outcome("insert an uploaded object", media.insert_object(&b).await);
    t.outcome(
        "insert the same bytes again in the project",
        media
            .insert_object(&object(3, one.id, 0x10, MediaState::Pending))
            .await,
    );
    t.outcome(
        "insert the same bytes in another project",
        media
            .insert_object(&object(4, two.id, 0x10, MediaState::Pending))
            .await,
    );
    t.outcome("insert a taken id", media.insert_object(&a).await);
    let missing = ProjectId::from_uuid(id(499));
    t.outcome(
        "insert for a missing project",
        media
            .insert_object(&object(5, missing, 0x50, MediaState::Pending))
            .await,
    );

    // Reads.
    t.outcome(
        "object",
        media
            .object(one.id, a.id)
            .await
            .map(|o| o.as_ref().map(shape)),
    );
    t.outcome(
        "object under another project",
        media.object(two.id, a.id).await.map(|o| o.is_some()),
    );
    t.outcome(
        "object by hash",
        media
            .object_by_hash(one.id, &[0x20; 32])
            .await
            .map(|o| o.as_ref().map(shape)),
    );
    let mut low = [0x10; 32];
    low[31] = 0;
    t.outcome(
        "objects by a hash range that holds one",
        shapes(
            media
                .objects_by_hash_range(one.id, &low, &[0x10; 32], 2)
                .await,
        ),
    );
    t.outcome(
        "objects by a hash range that holds both, at most one",
        shapes(
            media
                .objects_by_hash_range(one.id, &[0; 32], &[0xff; 32], 1)
                .await,
        ),
    );

    // Re-arming a pending upload; an uploaded object is not re-armed.
    let rearmed = MediaObject {
        content_type: "image/jpeg".to_owned(),
        len: 7,
        created_at: at(500),
        ..a.clone()
    };
    t.outcome("rearm a pending object", media.rearm(&rearmed).await);
    t.outcome(
        "rearm an uploaded object",
        media
            .rearm(&MediaObject {
                created_at: at(600),
                ..b.clone()
            })
            .await,
    );
    t.outcome(
        "the rearmed object",
        media
            .object(one.id, a.id)
            .await
            .map(|o| o.as_ref().map(shape)),
    );

    // Uses.
    t.outcome(
        "add a use",
        media
            .add_ref(&reference(&a, 1, Some(1), "input", 100))
            .await,
    );
    t.outcome(
        "add the same use again",
        media
            .add_ref(&reference(&a, 1, Some(1), "input", 200))
            .await,
    );
    t.outcome(
        "add a use without a span",
        media.add_ref(&reference(&a, 1, None, "", 100)).await,
    );
    t.outcome(
        "add a use of another object in the trace",
        media
            .add_ref(&reference(&b, 1, Some(2), "output", 300))
            .await,
    );
    t.outcome(
        "add a use in another trace",
        media
            .add_ref(&reference(&b, 2, Some(3), "metadata", 50))
            .await,
    );
    let gone = object(6, one.id, 0x60, MediaState::Uploaded);
    t.outcome(
        "add a use of an object that has no row",
        media
            .add_ref(&reference(&gone, 1, None, "input", 100))
            .await,
    );
    let uses = |trace_n: u8, span: Option<u8>| {
        let span = span.map(|n| SpanId::from_bytes([n; 8]));
        async move {
            media
                .uses(one.id, trace(trace_n), span)
                .await
                .map(|uses| uses.iter().map(use_shape).collect::<Vec<_>>())
        }
    };
    t.outcome("the uses of a span", uses(1, Some(1)).await);
    t.outcome("the uses of a trace", uses(1, None).await);
    t.outcome(
        "the uses of a trace under another project",
        media.uses(two.id, trace(1), None).await.map(|u| u.len()),
    );

    // The confirm.
    t.outcome(
        "mark uploaded under another project",
        media.mark_uploaded(two.id, a.id, at(900)).await,
    );
    t.outcome(
        "mark uploaded",
        media.mark_uploaded(one.id, a.id, at(900)).await,
    );
    t.outcome(
        "mark uploaded again",
        media.mark_uploaded(one.id, a.id, at(901)).await,
    );

    // The collector's reads, and the deletes that a use keeps from happening.
    t.outcome(
        "unreferenced while every object is used",
        shapes(media.unreferenced(10).await),
    );
    t.outcome(
        "delete an uploaded object that is used",
        media.delete_unreferenced(a.id).await,
    );
    t.outcome(
        "delete the uses of a trace before a time",
        media.delete_trace_refs(one.id, &[trace(1)], at(150)).await,
    );
    t.outcome("the uses of the trace left", uses(1, None).await);
    t.outcome(
        "delete the uses of no trace",
        media.delete_trace_refs(one.id, &[], at(10_000)).await,
    );
    t.outcome(
        "delete the uses before a cut-off",
        media.delete_refs_before(one.id, at(10_000)).await,
    );
    t.outcome(
        "unreferenced once the uses are gone",
        shapes(media.unreferenced(10).await),
    );
    t.outcome(
        "delete an unused uploaded object",
        media.delete_unreferenced(a.id).await,
    );
    t.outcome("delete it again", media.delete_unreferenced(a.id).await);
    t.outcome(
        "existing",
        media
            .existing(&[a.id, b.id, gone.id])
            .await
            .map(|ids| ids.iter().map(ToString::to_string).collect::<Vec<_>>()),
    );

    // Stale pending uploads go with their uses.
    let p = object(7, two.id, 0x70, MediaState::Pending);
    media.insert_object(&p).await?;
    media
        .add_ref(&reference(&p, 3, Some(4), "input", 700))
        .await?;
    t.outcome(
        "stale pending before a time",
        shapes(media.stale_pending(at(0x70 + 1), 10).await),
    );
    t.outcome(
        "stale pending before an earlier time",
        shapes(media.stale_pending(at(0x70), 10).await),
    );
    t.outcome(
        "delete a pending object that is not stale yet",
        media.delete_pending(p.id, at(0x70)).await,
    );
    t.outcome(
        "delete a stale pending object with its use",
        media.delete_pending(p.id, at(0x70 + 1)).await,
    );
    t.outcome(
        "its uses",
        media.uses(two.id, trace(3), None).await.map(|u| u.len()),
    );
    t.outcome(
        "delete an uploaded object as pending",
        media.delete_pending(b.id, at(10_000)).await,
    );

    // A use recorded while the object's row is deleted: whichever commits second fails.
    let c = object(8, two.id, 0x80, MediaState::Uploaded);
    media.insert_object(&c).await?;
    let tx = s.begin().await?;
    t.outcome(
        "a transaction reads the object",
        tx.media().object(two.id, c.id).await.map(|o| o.is_some()),
    );
    t.outcome(
        "the unused object is deleted outside",
        media.delete_unreferenced(c.id).await,
    );
    let late = tx
        .media()
        .add_ref(&reference(&c, 4, None, "input", 800))
        .await;
    ensure!(
        late.is_err(),
        "a use of a row deleted after the snapshot was recorded: {late:?}"
    );
    t.outcome("the transaction then adds a use", late);
    tx.rollback().await;

    let d = object(9, two.id, 0x90, MediaState::Uploaded);
    media.insert_object(&d).await?;
    let tx = s.begin().await?;
    t.outcome(
        "a transaction reads the object",
        tx.media().object(two.id, d.id).await.map(|o| o.is_some()),
    );
    t.outcome(
        "a use is recorded outside",
        media.add_ref(&reference(&d, 5, None, "output", 900)).await,
    );
    let late = tx.media().delete_unreferenced(d.id).await;
    ensure!(
        late != Ok(true),
        "an object whose new use the snapshot does not show was deleted"
    );
    t.outcome("the transaction then deletes it as unused", late);
    tx.rollback().await;
    t.outcome(
        "the used object stays",
        media.object(two.id, d.id).await.map(|o| o.is_some()),
    );

    let e = object(10, two.id, 0xa0, MediaState::Pending);
    media.insert_object(&e).await?;
    let tx = s.begin().await?;
    t.outcome(
        "a transaction reads the pending object",
        tx.media().object(two.id, e.id).await.map(|o| o.is_some()),
    );
    t.outcome(
        "a use is recorded outside",
        media.add_ref(&reference(&e, 6, None, "input", 950)).await,
    );
    let late = tx.media().delete_pending(e.id, at(10_000)).await;
    ensure!(
        late != Ok(true),
        "a pending object whose new use the snapshot does not show was deleted"
    );
    t.outcome("the transaction then deletes it as stale", late);
    tx.rollback().await;
    t.outcome(
        "the pending object stays with its use",
        media.uses(two.id, trace(6), None).await.map(|u| u.len()),
    );
    Ok(())
}
