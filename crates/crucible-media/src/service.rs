//! The upload, confirm and download flow of contract 14.2, over the control store and the object store.
//!
//! 1. [`Media::create`]: an upload of bytes the project already holds adds a use and needs no upload; any other
//!    gets a presigned PUT, valid for an hour, whose content type, length and checksum are signed headers.
//! 2. The client uploads straight to the store's public endpoint.
//! 3. [`Media::confirm`]: the store is asked for the object's length (and checksum, where it returns one), and
//!    the object is marked uploaded only when they match what was declared.
//! 4. [`Media::get`]: the object's metadata and a presigned GET, valid for 5 minutes, served inline only for
//!    the inline-safe types.

use std::fmt;
use std::sync::Arc;

use crucible_control::{
    ControlStore, MediaObject, MediaRef, MediaState, MediaUse, TxError, transaction,
};
use crucible_env::{Env, ObjectError, ObjectStore, Presigned};
use crucible_records::{MediaObjectId, ProjectScope, SpanId, TraceId};

use crate::rules::{GET_TTL, PUT_TTL, allowed, disposition, object_key};

/// How many times a create runs its transaction again after a conflict with another writer of the same bytes.
const CREATE_ATTEMPTS: usize = 4;

/// The deployment's media settings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MediaConfig {
    /// The key prefix, `object_store.prefix`.
    pub prefix: String,
    /// The cap on one object, `media.max_bytes`.
    pub max_bytes: u64,
}

/// An upload a client asks for (contract 14.2, step 1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NewUpload {
    /// The declared content type.
    pub content_type: String,
    /// The declared length in bytes.
    pub len: u64,
    /// The SHA-256 of the bytes.
    pub sha256: [u8; 32],
    /// The trace that uses the bytes.
    pub trace_id: TraceId,
    /// The span, when the use names one.
    pub span_id: Option<SpanId>,
    /// The payload field, such as `input`; empty when the use names none.
    pub field: String,
}

/// What a create answers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Created {
    /// The project already holds these bytes; the use was recorded and nothing needs uploading.
    Existing(MediaObject),
    /// Upload the bytes with this presigned PUT, then confirm.
    Upload(MediaObject, Presigned),
}

impl Created {
    /// The object either way.
    pub const fn object(&self) -> &MediaObject {
        match self {
            Self::Existing(object) | Self::Upload(object, _) => object,
        }
    }
}

/// Why a media call was refused.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum MediaError {
    /// The declared length is above `media.max_bytes`.
    #[error("the object is larger than the cap of {max} bytes")]
    TooLarge {
        /// The cap.
        max: u64,
    },
    /// The declared content type is not on the allowlist.
    #[error("the content type is not on the allowlist")]
    ContentType,
    /// No object of the project has this id.
    #[error("no media of this project has this id")]
    NotFound,
    /// The object has not been uploaded yet: the store has nothing under its key.
    #[error("the object has not been uploaded")]
    NotUploaded,
    /// The stored object's length or checksum differs from what was declared; it was deleted.
    #[error("the uploaded object does not match the declared length or SHA-256")]
    Mismatch,
    /// The control store failed.
    #[error(transparent)]
    Control(#[from] TxError),
    /// The object store failed.
    #[error(transparent)]
    Objects(#[from] ObjectError),
}

/// Media over the control store and the object store (contract 14).
#[derive(Clone)]
pub struct Media {
    env: Env,
    control: Arc<dyn ControlStore>,
    objects: Arc<dyn ObjectStore>,
    config: MediaConfig,
    /// How references are read out of payloads; without it, a span's references are its recorded uses.
    find: Option<FindRefs>,
}

impl fmt::Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Media")
            .field("objects", &self.objects)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Media {
    /// Media over these stores.
    pub fn new(
        env: &Env,
        control: Arc<dyn ControlStore>,
        objects: Arc<dyn ObjectStore>,
        config: MediaConfig,
    ) -> Self {
        Self {
            env: env.clone(),
            control,
            objects,
            config,
            find: None,
        }
    }

    /// The settings.
    pub const fn config(&self) -> &MediaConfig {
        &self.config
    }

    /// The key of an object.
    pub fn key(&self, object: &MediaObject) -> String {
        object_key(&self.config.prefix, object.project_id, object.id)
    }

    /// Checks a declared upload against the cap and the allowlist.
    pub fn check(&self, content_type: &str, len: u64) -> Result<(), MediaError> {
        if len > self.config.max_bytes {
            return Err(MediaError::TooLarge {
                max: self.config.max_bytes,
            });
        }
        if !allowed(content_type) {
            return Err(MediaError::ContentType);
        }
        Ok(())
    }

    /// Step 1 of contract 14.2. Bytes the project holds uploaded get a new use and no URL; bytes of a pending
    /// upload get the same id with a fresh URL; new bytes get a pending object. Every use is recorded once.
    pub async fn create(
        &self,
        scope: ProjectScope,
        upload: NewUpload,
    ) -> Result<Created, MediaError> {
        self.check(&upload.content_type, upload.len)?;
        let project_id = scope.project_id();
        let mut attempt = 0;
        let created = loop {
            attempt += 1;
            let now = self.env.clock.wall_now();
            let fresh = MediaObject {
                id: MediaObjectId::from_uuid(self.env.new_id()),
                project_id,
                sha256: upload.sha256,
                content_type: upload.content_type.clone(),
                len: upload.len,
                state: MediaState::Pending,
                created_at: now,
                uploaded_at: None,
            };
            let upload = upload.clone();
            let result = transaction(self.control.as_ref(), move |tx| {
                Box::pin(async move {
                    let media = tx.media();
                    let object = match media.object_by_hash(project_id, &upload.sha256).await? {
                        Some(stored) if stored.state == MediaState::Uploaded => stored,
                        Some(stored) => {
                            let rearmed = MediaObject {
                                content_type: fresh.content_type.clone(),
                                len: fresh.len,
                                created_at: fresh.created_at,
                                ..stored
                            };
                            media.rearm(&rearmed).await?;
                            rearmed
                        }
                        None => {
                            media.insert_object(&fresh).await?;
                            fresh
                        }
                    };
                    media
                        .add_ref(&MediaRef {
                            project_id,
                            media_id: object.id,
                            trace_id: upload.trace_id,
                            span_id: upload.span_id,
                            field: upload.field,
                            target_time: now,
                        })
                        .await?;
                    Ok(object)
                })
            })
            .await;
            match result {
                Ok(object) => break object,
                // Another request for the same bytes, or the collector, wrote first: look again.
                Err(
                    TxError::SerializationFailure
                    | TxError::UniqueViolation(_)
                    | TxError::ForeignKeyViolation(_),
                ) if attempt < CREATE_ATTEMPTS => {}
                Err(error) => return Err(error.into()),
            }
        };
        if created.state == MediaState::Uploaded {
            return Ok(Created::Existing(created));
        }
        let url = self.objects.presign_put(
            &self.key(&created),
            &created.content_type,
            &created.sha256,
            created.len,
            PUT_TTL,
        )?;
        Ok(Created::Upload(created, url))
    }

    /// Step 4 of contract 14.2: marks the object uploaded once the store holds it with the declared length,
    /// and the declared checksum where the store returns one. A mismatched object is deleted, so that the
    /// bytes can be uploaded again. Confirming an uploaded object answers it again.
    pub async fn confirm(
        &self,
        scope: ProjectScope,
        id: MediaObjectId,
    ) -> Result<MediaObject, MediaError> {
        let project_id = scope.project_id();
        let object = self
            .control
            .media()
            .object(project_id, id)
            .await?
            .ok_or(MediaError::NotFound)?;
        if object.state == MediaState::Uploaded {
            return Ok(object);
        }
        let key = self.key(&object);
        let Some(stored) = self.objects.head(&key).await? else {
            return Err(MediaError::NotUploaded);
        };
        if stored.len != object.len || stored.sha256.is_some_and(|sha| sha != object.sha256) {
            tracing::warn!(media = %id, project = %project_id, "an upload differs from what was declared; it is deleted");
            self.objects.delete(&key).await?;
            return Err(MediaError::Mismatch);
        }
        let at = self.env.clock.wall_now();
        if !self
            .control
            .media()
            .mark_uploaded(project_id, id, at)
            .await?
        {
            // Confirmed meanwhile, or collected as a stale upload.
            return match self.control.media().object(project_id, id).await? {
                Some(object) if object.state == MediaState::Uploaded => Ok(object),
                _ => Err(MediaError::NotFound),
            };
        }
        Ok(MediaObject {
            state: MediaState::Uploaded,
            uploaded_at: Some(at),
            ..object
        })
    }

    /// Step 5 of contract 14.2: the object and, once it is uploaded, a presigned GET.
    pub async fn get(
        &self,
        scope: ProjectScope,
        id: MediaObjectId,
    ) -> Result<(MediaObject, Option<Presigned>), MediaError> {
        let object = self
            .control
            .media()
            .object(scope.project_id(), id)
            .await?
            .ok_or(MediaError::NotFound)?;
        self.with_link(object)
    }

    /// The project's objects whose SHA-256 lies between `from` and `to`, at most two: how an id derived from a
    /// prefix of the hash, as other SDKs' media endpoints use, finds its object.
    pub async fn get_by_hash_prefix(
        &self,
        scope: ProjectScope,
        from: &[u8; 32],
        to: &[u8; 32],
    ) -> Result<Vec<MediaObject>, MediaError> {
        Ok(self
            .control
            .media()
            .objects_by_hash_range(scope.project_id(), from, to, 2)
            .await?)
    }

    /// The object with a presigned GET when it is uploaded.
    pub fn with_link(
        &self,
        object: MediaObject,
    ) -> Result<(MediaObject, Option<Presigned>), MediaError> {
        if object.state != MediaState::Uploaded {
            return Ok((object, None));
        }
        let link = self.objects.presign_get(
            &self.key(&object),
            disposition(&object.content_type),
            GET_TTL,
        )?;
        Ok((object, Some(link)))
    }

    /// The uses in a trace, of one span or of the whole trace, with their objects.
    pub async fn uses(
        &self,
        scope: ProjectScope,
        trace_id: TraceId,
        span_id: Option<SpanId>,
    ) -> Result<Vec<MediaUse>, MediaError> {
        Ok(self
            .control
            .media()
            .uses(scope.project_id(), trace_id, span_id)
            .await?)
    }
}

/// A media reference found in a payload field: where it is, the content type it names, and the SHA-256 values its
/// id can stand for, the least and the greatest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PayloadRef {
    /// The JSON pointer of the string that holds it within its field; empty for the whole field.
    pub pointer: String,
    /// The content type the reference names.
    pub content_type: String,
    /// The least and the greatest SHA-256 its id can stand for.
    pub hashes: ([u8; 32], [u8; 32]),
}

/// Reads the media references out of one payload field's bytes. The token formats belong to other products'
/// SDKs, so the reader is `crucible-compat`'s, which the binary passes in (contract 1.5).
pub type FindRefs = fn(&[u8]) -> Vec<PayloadRef>;

/// The most references [`Media::span_references`] answers with.
pub const MAX_SPAN_REFERENCES: usize = 64;

/// A medium one span references: where it appears, its content type, and its object when the project holds one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanReference {
    /// The payload field it appears in, such as `input`; empty for a use that names none.
    pub field: String,
    /// The JSON pointer within the field of the string that names it; `None` for a use recorded with its upload,
    /// which the payload does not name.
    pub pointer: Option<String>,
    /// The content type: the object's, or the one the reference names when the project holds no object for it.
    pub content_type: String,
    /// The object; `None` when the project holds none for the reference, for example because its upload never
    /// reached this server.
    pub object: Option<MediaObject>,
}

impl Media {
    /// The same media service, reading the references in payloads with `find`.
    #[must_use]
    pub fn with_payload_refs(mut self, find: FindRefs) -> Self {
        self.find = Some(find);
        self
    }

    /// The media one span references, at most [`MAX_SPAN_REFERENCES`]: those named in `fields`, its payload
    /// fields by name and body, in their order and the order they appear, then the uses recorded for the span that
    /// its payload does not name.
    pub async fn span_references(
        &self,
        scope: ProjectScope,
        trace_id: TraceId,
        span_id: SpanId,
        fields: &[(&str, &[u8])],
    ) -> Result<Vec<SpanReference>, MediaError> {
        let project_id = scope.project_id();
        let mut found: Vec<SpanReference> = Vec::new();
        let mut looked_up: Vec<(([u8; 32], [u8; 32]), Option<MediaObject>)> = Vec::new();
        if let Some(find) = self.find {
            'fields: for (field, body) in fields {
                for reference in find(body) {
                    if found.len() >= MAX_SPAN_REFERENCES {
                        break 'fields;
                    }
                    let held = looked_up.iter().find(|(hashes, _)| *hashes == reference.hashes);
                    let object = match held {
                        Some((_, object)) => object.clone(),
                        None => {
                            let (low, high) = &reference.hashes;
                            let object = self
                                .control
                                .media()
                                .objects_by_hash_range(project_id, low, high, 1)
                                .await?
                                .into_iter()
                                .next();
                            looked_up.push((reference.hashes, object.clone()));
                            object
                        }
                    };
                    found.push(SpanReference {
                        field: (*field).to_owned(),
                        pointer: Some(reference.pointer),
                        content_type: object
                            .as_ref()
                            .map_or(reference.content_type, |o| o.content_type.clone()),
                        object,
                    });
                }
            }
        }
        for recorded in self.uses(scope, trace_id, Some(span_id)).await? {
            if found.len() >= MAX_SPAN_REFERENCES {
                break;
            }
            let named = found.iter().any(|reference| {
                reference.field == recorded.reference.field
                    && reference
                        .object
                        .as_ref()
                        .is_some_and(|object| object.id == recorded.object.id)
            });
            if !named {
                found.push(SpanReference {
                    field: recorded.reference.field,
                    pointer: None,
                    content_type: recorded.object.content_type.clone(),
                    object: Some(recorded.object),
                });
            }
        }
        Ok(found)
    }
}
