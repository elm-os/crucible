//! The media repository over Postgres (migration V8): `media_objects` and `media_refs` (contract 14.3).
//!
//! A use without a span stores an empty `span_id`, and one without a field an empty `field`, so that the
//! primary key of `media_refs` makes recording the same use twice a no-op.

use async_trait::async_trait;
use crucible_control::{MediaObject, MediaRef, MediaRepo, MediaState, MediaUse, TxError};
use crucible_records::{MediaObjectId, ProjectId, SpanId, Timestamp, TraceId};

use crate::sql::{Cols, P, int};
use crate::store::{Handle, every, first};

const OBJECT: &str =
    "o.id, o.project_id, o.sha256, o.content_type, o.len, o.state, o.created_at, o.uploaded_at";

fn object(c: &mut Cols<'_>) -> Result<MediaObject, TxError> {
    Ok(MediaObject {
        id: MediaObjectId::from_uuid(c.uuid()?),
        project_id: ProjectId::from_uuid(c.uuid()?),
        sha256: c.array()?,
        content_type: c.text()?,
        len: c.u64()?,
        state: c.parsed(MediaState::parse)?,
        created_at: c.ts()?,
        uploaded_at: c.opt_ts()?,
    })
}

fn span_bytes(span_id: Option<SpanId>) -> Vec<u8> {
    span_id.map(|id| id.to_bytes().to_vec()).unwrap_or_default()
}

fn usage(c: &mut Cols<'_>) -> Result<MediaUse, TxError> {
    let object = object(c)?;
    let trace_id = TraceId::from_bytes(c.array()?);
    let span = c.bytes()?;
    let span_id = match span.len() {
        0 => None,
        _ => Some(SpanId::from_bytes(
            <[u8; 8]>::try_from(span.as_slice()).map_err(|_| {
                TxError::Invalid(format!("a stored span id has {} bytes", span.len()))
            })?,
        )),
    };
    Ok(MediaUse {
        reference: MediaRef {
            project_id: object.project_id,
            media_id: object.id,
            trace_id,
            span_id,
            field: c.text()?,
            target_time: c.ts()?,
        },
        object,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[async_trait]
impl MediaRepo for Handle {
    async fn insert_object(&self, object: &MediaObject) -> Result<(), TxError> {
        self.write(
            "INSERT INTO media_objects (id, project_id, sha256, content_type, len, state, created_at, uploaded_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING 1",
            &[
                P::Uuid(object.id.uuid()),
                P::Uuid(object.project_id.uuid()),
                P::Bytes(&object.sha256),
                P::Text(&object.content_type),
                int(object.len)?,
                P::Text(object.state.as_str()),
                P::Ts(object.created_at),
                P::OptTs(object.uploaded_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn object(
        &self,
        project_id: ProjectId,
        id: MediaObjectId,
    ) -> Result<Option<MediaObject>, TxError> {
        let rows = self
            .read(
                &format!(
                    "SELECT {OBJECT} FROM media_objects o WHERE o.project_id = $1 AND o.id = $2"
                ),
                &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid())],
            )
            .await?;
        first(&rows, object)
    }

    async fn object_by_hash(
        &self,
        project_id: ProjectId,
        sha256: &[u8; 32],
    ) -> Result<Option<MediaObject>, TxError> {
        let rows = self
            .read(
                &format!(
                    "SELECT {OBJECT} FROM media_objects o WHERE o.project_id = $1 AND o.sha256 = $2"
                ),
                &[P::Uuid(project_id.uuid()), P::Bytes(sha256)],
            )
            .await?;
        first(&rows, object)
    }

    async fn objects_by_hash_range(
        &self,
        project_id: ProjectId,
        from: &[u8; 32],
        to: &[u8; 32],
        limit: u32,
    ) -> Result<Vec<MediaObject>, TxError> {
        let rows = self
            .read(
                &format!(
                    "SELECT {OBJECT} FROM media_objects o WHERE o.project_id = $1 AND o.sha256 >= $2 \
                     AND o.sha256 <= $3 ORDER BY o.sha256 LIMIT $4"
                ),
                &[
                    P::Uuid(project_id.uuid()),
                    P::Bytes(from),
                    P::Bytes(to),
                    P::Int(i64::from(limit)),
                ],
            )
            .await?;
        every(&rows, object)
    }

    async fn rearm(&self, object: &MediaObject) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE media_objects SET content_type = $2, len = $3, created_at = $4 \
                 WHERE id = $1 AND state = 'pending' RETURNING 1",
                &[
                    P::Uuid(object.id.uuid()),
                    P::Text(&object.content_type),
                    int(object.len)?,
                    P::Ts(object.created_at),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn mark_uploaded(
        &self,
        project_id: ProjectId,
        id: MediaObjectId,
        at: Timestamp,
    ) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE media_objects SET state = 'uploaded', uploaded_at = $3 \
                 WHERE project_id = $1 AND id = $2 AND state = 'pending' RETURNING 1",
                &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid()), P::Ts(at)],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn add_ref(&self, reference: &MediaRef) -> Result<bool, TxError> {
        let span = span_bytes(reference.span_id);
        let added = self
            .write(
                "INSERT INTO media_refs (project_id, media_id, trace_id, span_id, field, target_time) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT ON CONSTRAINT media_refs_pkey DO NOTHING \
                 RETURNING 1",
                &[
                    P::Uuid(reference.project_id.uuid()),
                    P::Uuid(reference.media_id.uuid()),
                    P::Bytes(reference.trace_id.as_bytes()),
                    P::Bytes(&span),
                    P::Text(&reference.field),
                    P::Ts(reference.target_time),
                ],
            )
            .await?;
        Ok(added == 1)
    }

    async fn uses(
        &self,
        project_id: ProjectId,
        trace_id: TraceId,
        span_id: Option<SpanId>,
    ) -> Result<Vec<MediaUse>, TxError> {
        let span = span_bytes(span_id);
        let rows = self
            .read(
                &format!(
                    "SELECT {OBJECT}, r.trace_id, r.span_id, r.field, r.target_time \
                     FROM media_refs r JOIN media_objects o ON o.id = r.media_id \
                     WHERE r.project_id = $1 AND r.trace_id = $2 AND ($3 = false OR r.span_id = $4) \
                     ORDER BY r.field, r.media_id, r.span_id"
                ),
                &[
                    P::Uuid(project_id.uuid()),
                    P::Bytes(trace_id.as_bytes()),
                    P::Bool(span_id.is_some()),
                    P::Bytes(&span),
                ],
            )
            .await?;
        every(&rows, usage)
    }

    async fn delete_trace_refs(
        &self,
        project_id: ProjectId,
        trace_ids: &[TraceId],
        before: Timestamp,
    ) -> Result<u64, TxError> {
        if trace_ids.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = trace_ids.iter().map(|id| hex(id.as_bytes())).collect();
        self.write(
            "DELETE FROM media_refs WHERE project_id = $1 AND target_time < $3 \
             AND trace_id IN (SELECT decode(t, 'hex') FROM unnest($2::text[]) AS t) RETURNING 1",
            &[
                P::Uuid(project_id.uuid()),
                P::TextArray(texts.iter().map(String::as_str).collect()),
                P::Ts(before),
            ],
        )
        .await
    }

    async fn delete_refs_before(
        &self,
        project_id: ProjectId,
        cutoff: Timestamp,
    ) -> Result<u64, TxError> {
        self.write(
            "DELETE FROM media_refs WHERE project_id = $1 AND target_time < $2 RETURNING 1",
            &[P::Uuid(project_id.uuid()), P::Ts(cutoff)],
        )
        .await
    }

    async fn stale_pending(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<Vec<MediaObject>, TxError> {
        let rows = self
            .read(
                &format!(
                    "SELECT {OBJECT} FROM media_objects o WHERE o.state = 'pending' AND o.created_at < $1 \
                     ORDER BY o.created_at, o.id LIMIT $2"
                ),
                &[P::Ts(before), P::Int(i64::from(limit))],
            )
            .await?;
        every(&rows, object)
    }

    async fn unreferenced(&self, limit: u32) -> Result<Vec<MediaObject>, TxError> {
        let rows = self
            .read(
                &format!(
                    "SELECT {OBJECT} FROM media_objects o WHERE o.state = 'uploaded' \
                     AND NOT EXISTS (SELECT 1 FROM media_refs r WHERE r.media_id = o.id) \
                     ORDER BY o.created_at, o.id LIMIT $1"
                ),
                &[P::Int(i64::from(limit))],
            )
            .await?;
        every(&rows, object)
    }

    async fn delete_pending(&self, id: MediaObjectId, before: Timestamp) -> Result<bool, TxError> {
        let rows = self
            .query_main(
                true,
                "WITH gone AS (DELETE FROM media_objects WHERE id = $1 AND state = 'pending' \
                 AND created_at < $2 RETURNING id), \
                 uses AS (DELETE FROM media_refs WHERE media_id IN (SELECT id FROM gone)) \
                 SELECT count(*) FROM gone",
                &[P::Uuid(id.uuid()), P::Ts(before)],
            )
            .await?;
        Ok(first(&rows, |c| c.u64())?.unwrap_or(0) == 1)
    }

    async fn delete_unreferenced(&self, id: MediaObjectId) -> Result<bool, TxError> {
        let changed = self
            .write(
                "DELETE FROM media_objects o WHERE o.id = $1 AND o.state = 'uploaded' \
                 AND NOT EXISTS (SELECT 1 FROM media_refs r WHERE r.media_id = o.id) RETURNING 1",
                &[P::Uuid(id.uuid())],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn existing(&self, ids: &[MediaObjectId]) -> Result<Vec<MediaObjectId>, TxError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let rows = self
            .read(
                "SELECT id FROM media_objects WHERE id = ANY($1::uuid[]) ORDER BY id",
                &[P::TextArray(texts.iter().map(String::as_str).collect())],
            )
            .await?;
        every(&rows, |c| Ok(MediaObjectId::from_uuid(c.uuid()?)))
    }
}
