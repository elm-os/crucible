//! Media: `projects/{project_id}/media...` (contracts 12.6 and 14).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::wire::SafeU64;

/// The body of `POST media`: the bytes a client is about to upload and where they are used.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MediaCreate {
    /// The content type, from the allowlist (for example `image/png`); it is a signed header of the upload.
    pub content_type: String,
    /// The length in bytes, at most `features.media.max_bytes`.
    pub len: SafeU64,
    /// The SHA-256 of the bytes, 64 lowercase hexadecimal characters.
    pub sha256: String,
    /// The trace that uses the bytes, 32 hexadecimal characters.
    pub trace_id: String,
    /// The span that uses them, 16 hexadecimal characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub span_id: Option<String>,
    /// The payload field they appear in: one of the payload route's `field` values (`input`, `output`,
    /// `system_instructions`, `metadata`, `attributes`, `resource_attributes`, `events`, `links`,
    /// `tool_definitions`, `tool_calls`, `mcp_args`, `mcp_result`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub field: Option<String>,
}

/// A header a presigned request must carry exactly as given.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SignedHeader {
    /// The lowercase header name.
    pub name: String,
    /// The value.
    pub value: String,
}

/// A presigned upload: `PUT` the bytes to `url` with exactly these headers before `expires_at`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MediaUploadUrl {
    /// The URL, on the object store's public endpoint.
    pub url: String,
    /// The signed headers: the content type, the length and the checksum.
    pub headers: Vec<SignedHeader>,
    /// When the URL stops working (one hour after it was made).
    #[schema(format = DateTime)]
    pub expires_at: String,
}

/// The answer of `POST media`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MediaUpload {
    /// The media id; confirm the upload with `PATCH media/{media_id}`.
    pub media_id: String,
    /// The presigned upload; absent when the project already holds these bytes, so there is nothing to upload
    /// or confirm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub upload: Option<MediaUploadUrl>,
}

/// Where a media object is (contract 14.3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum MediaState {
    /// Waiting for its upload to be confirmed.
    Pending,
    /// Stored and confirmed.
    Uploaded,
}

/// How a browser is asked to treat a download.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum MediaDisposition {
    /// Shown in the page: common images, audio and video.
    Inline,
    /// Saved as a file: every other type.
    Attachment,
}

/// A media object.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Media {
    /// The media id.
    pub id: String,
    /// The content type.
    pub content_type: String,
    /// The length in bytes.
    pub len: SafeU64,
    /// The SHA-256 of the bytes, lowercase hex.
    pub sha256: String,
    /// Pending or uploaded.
    pub state: MediaState,
    /// How a download is served.
    pub disposition: MediaDisposition,
    /// When the upload was asked for.
    #[schema(format = DateTime)]
    pub created_at: String,
    /// When the upload was confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub uploaded_at: Option<String>,
    /// A presigned download on the object store's public endpoint, once uploaded. It lives 5 minutes; read
    /// the media again for a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub url: Option<String>,
    /// When `url` stops working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub url_expires_at: Option<String>,
    /// Whether `len` was cut to 2^53 - 1.
    pub saturated: bool,
}

/// The query of `GET media`: the uses in one trace, or in one span of it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct MediaListParams {
    /// The trace, 32 hexadecimal characters.
    pub trace_id: String,
    /// The span, 16 hexadecimal characters; without it, every use in the trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(nullable = false)]
    #[ts(optional)]
    pub span_id: Option<String>,
}

/// One use of a media object: where it appears.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MediaUse {
    /// The span; absent when the use names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub span_id: Option<String>,
    /// The payload field, as `POST media` named it; absent when the use names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub field: Option<String>,
    /// The object, without a download URL: read `GET media/{media_id}` for one.
    pub media: Media,
}

/// The answer of `GET media`: the uses in a trace or a span, by field.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MediaList {
    /// The uses.
    pub data: Vec<MediaUse>,
}

/// The query of `GET spans/{trace_id}/{span_id}/media`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct SpanMediaParams {
    /// The span's start time as the API returned it (RFC 3339): it locates the span's payload.
    #[param(format = DateTime)]
    pub start_time: String,
}

/// One medium a span references: where it appears, and the media when the project holds it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SpanMedia {
    /// The payload field it appears in: `input`, `output` or `metadata` for a reference in the payload, or the
    /// field its upload named; absent when the upload named none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub field: Option<String>,
    /// The JSON pointer, within the field, of the string that holds the reference: empty for a field that is the
    /// reference itself or is not JSON, and absent for media uploaded for the span that its payload does not name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub pointer: Option<String>,
    /// The content type: the media's, or the one the reference names when the project holds no media for it.
    pub content_type: String,
    /// The media, without a download URL: read `GET media/{media_id}` for one. Absent when the project holds no
    /// media for the reference, for example because its upload never reached this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub media: Option<Media>,
}

/// The answer of `GET spans/{trace_id}/{span_id}/media`: the media the span references, at most 64.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SpanMediaList {
    /// The references, those in the payload first, in field order and the order they appear.
    pub data: Vec<SpanMedia>,
}
