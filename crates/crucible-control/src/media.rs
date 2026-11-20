//! Media in the control plane (contracts 11.1 and 14.3): the stored objects, one per project and SHA-256,
//! and their references, one row per use.
//!
//! A reference holds its object: the foreign key from `media_refs` to `media_objects` has no cascade, so an
//! object row can be deleted only once nothing refers to it, and a reference can only be added to an object row
//! that still exists. An object that is no longer needed loses its row (its `deleted` state of contract 14.3 is
//! never stored), and a later upload of the same bytes gets a new row, a new id and so a new object key.

use std::fmt;

use async_trait::async_trait;
use crucible_records::{MediaObjectId, ProjectId, SpanId, Timestamp, TraceId};
use serde::{Deserialize, Serialize};

use crate::error::TxError;

text_enum! {
    /// Where a media object stands (contract 14.3).
    MediaState {
        /// A presigned upload was handed out and has not been confirmed.
        Pending => "pending",
        /// The upload was confirmed against the store.
        Uploaded => "uploaded",
    }
}

/// One stored object (`media_objects`): unique per project and SHA-256.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MediaObject {
    /// The media id; the object key derives from it.
    pub id: MediaObjectId,
    /// The project.
    pub project_id: ProjectId,
    /// The SHA-256 of the bytes.
    pub sha256: [u8; 32],
    /// The declared content type.
    pub content_type: String,
    /// The declared length in bytes.
    pub len: u64,
    /// Pending or uploaded.
    pub state: MediaState,
    /// When the upload was last asked for: a new request for the bytes of a pending object moves it.
    pub created_at: Timestamp,
    /// When the upload was confirmed.
    pub uploaded_at: Option<Timestamp>,
}

/// One use of an object (`media_refs`): a payload field of a span, or of a trace when there is no span.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MediaRef {
    /// The project.
    pub project_id: ProjectId,
    /// The object.
    pub media_id: MediaObjectId,
    /// The trace.
    pub trace_id: TraceId,
    /// The span, when the use names one.
    pub span_id: Option<SpanId>,
    /// The payload field, such as `input`; empty when the use names none.
    pub field: String,
    /// The time retention measures the use by (`payload_retention_days`): when it was recorded.
    pub target_time: Timestamp,
}

/// A use with its object.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MediaUse {
    /// The use.
    pub reference: MediaRef,
    /// The object it refers to.
    pub object: MediaObject,
}

/// The media repository (contract 14).
#[async_trait]
pub trait MediaRepo: Send + Sync {
    /// Creates an object row. Another row of the project with the same SHA-256 is
    /// `UniqueViolation("media_objects_project_id_sha256_key")`.
    async fn insert_object(&self, object: &MediaObject) -> Result<(), TxError>;
    /// The object with this id, if it belongs to the project.
    async fn object(
        &self,
        project_id: ProjectId,
        id: MediaObjectId,
    ) -> Result<Option<MediaObject>, TxError>;
    /// The project's object with this SHA-256.
    async fn object_by_hash(
        &self,
        project_id: ProjectId,
        sha256: &[u8; 32],
    ) -> Result<Option<MediaObject>, TxError>;
    /// The project's objects whose SHA-256 lies between `from` and `to`, both included, in hash order, at most
    /// `limit`: the objects a prefix of a hash names.
    async fn objects_by_hash_range(
        &self,
        project_id: ProjectId,
        from: &[u8; 32],
        to: &[u8; 32],
        limit: u32,
    ) -> Result<Vec<MediaObject>, TxError>;
    /// Asks again for the upload of a pending object: writes its content type, length and creation time.
    /// `false` when the object is not pending.
    async fn rearm(&self, object: &MediaObject) -> Result<bool, TxError>;
    /// Marks a pending object of the project uploaded at `at`. `false` when there is no such pending object.
    async fn mark_uploaded(
        &self,
        project_id: ProjectId,
        id: MediaObjectId,
        at: Timestamp,
    ) -> Result<bool, TxError>;
    /// Records a use. `false` when the same use (object, trace, span, field) is already recorded. A use of an
    /// object row that is gone is `ForeignKeyViolation("media_refs_media_id_fkey")`.
    async fn add_ref(&self, reference: &MediaRef) -> Result<bool, TxError>;
    /// The uses of the trace's objects with their objects, ordered by field and media id: those of one span,
    /// or every use in the trace when `span_id` is `None`.
    async fn uses(
        &self,
        project_id: ProjectId,
        trace_id: TraceId,
        span_id: Option<SpanId>,
    ) -> Result<Vec<MediaUse>, TxError>;
    /// Deletes the uses in these traces recorded before `before`; returns how many.
    async fn delete_trace_refs(
        &self,
        project_id: ProjectId,
        trace_ids: &[TraceId],
        before: Timestamp,
    ) -> Result<u64, TxError>;
    /// Deletes the project's uses whose target time is before `cutoff`; returns how many.
    async fn delete_refs_before(
        &self,
        project_id: ProjectId,
        cutoff: Timestamp,
    ) -> Result<u64, TxError>;
    /// The pending objects created before `before`, oldest first, at most `limit`, in every project.
    async fn stale_pending(
        &self,
        before: Timestamp,
        limit: u32,
    ) -> Result<Vec<MediaObject>, TxError>;
    /// The uploaded objects that no use refers to, oldest first, at most `limit`, in every project.
    async fn unreferenced(&self, limit: u32) -> Result<Vec<MediaObject>, TxError>;
    /// Deletes a pending object created before `before`, with its uses, in one statement. `false` when the
    /// object is not such an object any more. A use recorded by a transaction this one cannot see makes it
    /// fail instead.
    async fn delete_pending(&self, id: MediaObjectId, before: Timestamp) -> Result<bool, TxError>;
    /// Deletes an uploaded object that no use refers to. `false` when it is gone, not uploaded or referred to.
    /// A use recorded by a transaction this one cannot see makes it fail instead.
    async fn delete_unreferenced(&self, id: MediaObjectId) -> Result<bool, TxError>;
    /// Which of these ids have an object row, in any project, ascending.
    async fn existing(&self, ids: &[MediaObjectId]) -> Result<Vec<MediaObjectId>, TxError>;
}
