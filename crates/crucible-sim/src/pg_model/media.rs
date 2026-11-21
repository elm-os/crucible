//! The media repository of the model (migration V8): `media_objects` and `media_refs` (contract 14.3).
//!
//! A use refers to its object through `media_refs_media_id_fkey`, which has no cascade: deleting an object row
//! that a committed use refers to is a foreign-key violation, also when this transaction's snapshot does not
//! show the use ([`super::restricts`]).

use async_trait::async_trait;
use crucible_control::{MediaObject, MediaRef, MediaRepo, MediaState, MediaUse, TxError};
use crucible_records::{MediaObjectId, ProjectId, SpanId, Timestamp, TraceId};

use super::{Cx, Halt, Key, Pool, Row, Runner, Step, Tbl, uuid_key};

/// The primary key of a use: object, trace, span (with its length, as it may be empty) and field.
fn ref_key(reference: &MediaRef) -> Key {
    let span = reference
        .span_id
        .map(|id| id.to_bytes().to_vec())
        .unwrap_or_default();
    let mut key = reference.media_id.to_bytes().to_vec();
    key.extend_from_slice(reference.trace_id.as_bytes());
    key.push(u8::try_from(span.len()).unwrap_or(u8::MAX));
    key.extend_from_slice(&span);
    key.extend_from_slice(reference.field.as_bytes());
    key
}

fn objects(cx: &Cx<'_>) -> Vec<MediaObject> {
    all!(cx, Tbl::MediaObjects, Row::MediaObject)
}

fn refs(cx: &Cx<'_>) -> Vec<(Key, MediaRef)> {
    cx.scan(Tbl::MediaRefs)
        .into_iter()
        .filter_map(|(key, row)| match row {
            Row::MediaRef(reference) => Some((key, reference)),
            _ => None,
        })
        .collect()
}

fn by_age(objects: &mut Vec<MediaObject>, limit: u32) {
    objects.sort_by_key(|object| (object.created_at, object.id));
    objects.truncate(limit as usize);
}

/// Deletes the uses `which` selects; returns how many.
fn delete_refs(cx: &mut Cx<'_>, which: impl Fn(&MediaRef) -> bool) -> Step<u64> {
    let mut deleted = 0;
    for (key, reference) in refs(cx) {
        if which(&reference) {
            cx.put(Tbl::MediaRefs, key, None)?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

#[async_trait]
impl MediaRepo for Runner {
    async fn insert_object(&self, object: &MediaObject) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::MediaObjects,
                uuid_key(object.id.to_bytes()),
                Row::MediaObject(object.clone()),
                "media_objects_pkey",
            )
        })
        .await
    }

    async fn object(
        &self,
        project_id: ProjectId,
        id: MediaObjectId,
    ) -> Result<Option<MediaObject>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::MediaObjects,
                &uuid_key(id.to_bytes()),
                Row::MediaObject
            )
            .filter(|object| object.project_id == project_id))
        })
        .await
    }

    async fn object_by_hash(
        &self,
        project_id: ProjectId,
        sha256: &[u8; 32],
    ) -> Result<Option<MediaObject>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(objects(cx)
                .into_iter()
                .find(|object| object.project_id == project_id && object.sha256 == *sha256))
        })
        .await
    }

    async fn objects_by_hash_range(
        &self,
        project_id: ProjectId,
        from: &[u8; 32],
        to: &[u8; 32],
        limit: u32,
    ) -> Result<Vec<MediaObject>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut found: Vec<MediaObject> = objects(cx)
                .into_iter()
                .filter(|object| {
                    object.project_id == project_id
                        && object.sha256 >= *from
                        && object.sha256 <= *to
                })
                .collect();
            found.sort_by_key(|object| object.sha256);
            found.truncate(limit as usize);
            Ok(found)
        })
        .await
    }

    async fn rearm(&self, object: &MediaObject) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(object.id.to_bytes());
            match one!(cx, Tbl::MediaObjects, &key, Row::MediaObject) {
                Some(stored) if stored.state == MediaState::Pending => {
                    let next = MediaObject {
                        content_type: object.content_type.clone(),
                        len: object.len,
                        created_at: object.created_at,
                        ..stored
                    };
                    cx.put(Tbl::MediaObjects, key, Some(Row::MediaObject(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn mark_uploaded(
        &self,
        project_id: ProjectId,
        id: MediaObjectId,
        at: Timestamp,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::MediaObjects, &key, Row::MediaObject) {
                Some(stored)
                    if stored.project_id == project_id && stored.state == MediaState::Pending =>
                {
                    let next = MediaObject {
                        state: MediaState::Uploaded,
                        uploaded_at: Some(at),
                        ..stored
                    };
                    cx.put(Tbl::MediaObjects, key, Some(Row::MediaObject(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn add_ref(&self, reference: &MediaRef) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = ref_key(reference);
            if cx.locked_by_other(Tbl::MediaRefs, &key) {
                return Err(Halt::Blocked);
            }
            // `ON CONFLICT DO NOTHING`: a conflicting row this transaction cannot see is a serialization failure
            // under repeatable read.
            if cx.latest(Tbl::MediaRefs, &key).is_some() {
                return if cx.get(Tbl::MediaRefs, &key).is_some() {
                    Ok(false)
                } else {
                    Err(TxError::SerializationFailure.into())
                };
            }
            cx.put(Tbl::MediaRefs, key, Some(Row::MediaRef(reference.clone())))?;
            Ok(true)
        })
        .await
    }

    async fn uses(
        &self,
        project_id: ProjectId,
        trace_id: TraceId,
        span_id: Option<SpanId>,
    ) -> Result<Vec<MediaUse>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut found: Vec<MediaUse> = refs(cx)
                .into_iter()
                .filter(|(_, r)| {
                    r.project_id == project_id
                        && r.trace_id == trace_id
                        && span_id.is_none_or(|span| r.span_id == Some(span))
                })
                .filter_map(|(_, reference)| {
                    let key = uuid_key(reference.media_id.to_bytes());
                    let object = one!(cx, Tbl::MediaObjects, &key, Row::MediaObject)?;
                    Some(MediaUse { reference, object })
                })
                .collect();
            found.sort_by(|a, b| {
                let key = |u: &MediaUse| {
                    (
                        u.reference.field.clone(),
                        u.reference.media_id,
                        u.reference
                            .span_id
                            .map(|id| id.to_bytes())
                            .unwrap_or_default(),
                    )
                };
                key(a).cmp(&key(b))
            });
            Ok(found)
        })
        .await
    }

    async fn delete_trace_refs(
        &self,
        project_id: ProjectId,
        trace_ids: &[TraceId],
        before: Timestamp,
    ) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            delete_refs(cx, |r| {
                r.project_id == project_id
                    && r.target_time < before
                    && trace_ids.contains(&r.trace_id)
            })
        })
        .await
    }

    async fn delete_refs_before(
        &self,
        project_id: ProjectId,
        cutoff: Timestamp,
    ) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            delete_refs(cx, |r| r.project_id == project_id && r.target_time < cutoff)
        })
        .await
    }

    async fn stale_pending(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<Vec<MediaObject>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut found: Vec<MediaObject> = objects(cx)
                .into_iter()
                .filter(|o| o.state == MediaState::Pending && o.created_at < before)
                .collect();
            by_age(&mut found, limit);
            Ok(found)
        })
        .await
    }

    async fn unreferenced(&self, limit: u32) -> Result<Vec<MediaObject>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let used: Vec<MediaObjectId> = refs(cx).into_iter().map(|(_, r)| r.media_id).collect();
            let mut found: Vec<MediaObject> = objects(cx)
                .into_iter()
                .filter(|o| o.state == MediaState::Uploaded && !used.contains(&o.id))
                .collect();
            by_age(&mut found, limit);
            Ok(found)
        })
        .await
    }

    async fn delete_pending(&self, id: MediaObjectId, before: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::MediaObjects, &key, Row::MediaObject) {
                Some(stored)
                    if stored.state == MediaState::Pending && stored.created_at < before =>
                {
                    delete_refs(cx, |r| r.media_id == id)?;
                    cx.put(Tbl::MediaObjects, key, None)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn delete_unreferenced(&self, id: MediaObjectId) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            let used = refs(cx).iter().any(|(_, r)| r.media_id == id);
            match one!(cx, Tbl::MediaObjects, &key, Row::MediaObject) {
                Some(stored) if stored.state == MediaState::Uploaded && !used => {
                    cx.put(Tbl::MediaObjects, key, None)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn existing(&self, ids: &[MediaObjectId]) -> Result<Vec<MediaObjectId>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut found: Vec<MediaObjectId> = ids
                .iter()
                .copied()
                .filter(|id| {
                    cx.get(Tbl::MediaObjects, &uuid_key(id.to_bytes()))
                        .is_some()
                })
                .collect();
            found.sort();
            found.dedup();
            Ok(found)
        })
        .await
    }

    async fn delete_project(&self, project_id: ProjectId) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            delete_refs(cx, |r| r.project_id == project_id)?;
            let mut deleted = 0;
            for object in objects(cx) {
                if object.project_id == project_id {
                    cx.put(Tbl::MediaObjects, uuid_key(object.id.to_bytes()), None)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await
    }
}
