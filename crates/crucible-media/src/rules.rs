//! The rules of contract 14.1 and 14.3 as data: the content types an upload may declare, the ones served
//! inline, the caps and lifetimes, and the object key layout.

use std::time::Duration;

use crucible_env::Disposition;
use crucible_records::{MediaObjectId, ProjectId, Timestamp};

/// The default cap on one object (`media.max_bytes`): 64 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 64 << 20;
/// How long a presigned upload is valid.
pub const PUT_TTL: Duration = Duration::from_secs(3_600);
/// How long a presigned download is valid.
pub const GET_TTL: Duration = Duration::from_secs(300);
/// How long an upload may stay pending before it is removed.
pub const PENDING_TTL: Duration = Duration::from_secs(24 * 3_600);
/// How old a stored object without a row must be before the orphan scan deletes it.
pub const ORPHAN_AGE: Duration = Duration::from_secs(48 * 3_600);

/// The content types an upload may declare (contract 14.1), lowercase without parameters.
pub const CONTENT_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/webp",
    "image/gif",
    "image/svg+xml",
    "image/tiff",
    "image/bmp",
    "image/avif",
    "image/heic",
    "audio/mpeg",
    "audio/mp3",
    "audio/wav",
    "audio/ogg",
    "audio/oga",
    "audio/aac",
    "audio/mp4",
    "audio/flac",
    "audio/opus",
    "audio/webm",
    "video/mp4",
    "video/webm",
    "video/ogg",
    "video/mpeg",
    "video/quicktime",
    "video/x-msvideo",
    "video/x-matroska",
    "text/plain",
    "text/html",
    "text/css",
    "text/csv",
    "text/markdown",
    "text/x-python",
    "text/x-typescript",
    "application/javascript",
    "application/x-yaml",
    "application/pdf",
    "application/msword",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "application/zip",
    "application/json",
    "application/xml",
    "application/octet-stream",
    "application/rtf",
    "application/x-ndjson",
    "application/vnd.apache.parquet",
    "application/gzip",
    "application/x-tar",
    "application/x-7z-compressed",
];

/// The types a browser shows without running anything from the file: common raster images, audio and video
/// (contract 14.1). SVG, HTML, XML, scripts and documents are not among them.
pub const INLINE_SAFE: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/webp",
    "image/gif",
    "image/avif",
    "image/bmp",
    "audio/mpeg",
    "audio/mp3",
    "audio/wav",
    "audio/ogg",
    "audio/aac",
    "audio/mp4",
    "audio/flac",
    "audio/opus",
    "audio/webm",
    "video/mp4",
    "video/webm",
    "video/ogg",
];

/// Whether an upload may declare `content_type`. It must be listed exactly: the type is a signed header of
/// the upload, so the server never rewrites it.
pub fn allowed(content_type: &str) -> bool {
    CONTENT_TYPES.contains(&content_type)
}

/// How a download of `content_type` is served: inline only for the inline-safe types, as an attachment
/// otherwise.
pub fn disposition(content_type: &str) -> Disposition {
    if INLINE_SAFE.contains(&content_type) {
        Disposition::Inline
    } else {
        Disposition::Attachment
    }
}

/// The key of an object (contract 14.3): `<prefix>/<project_id>/<yyyy>/<mm>/<media_id>`, the month being that
/// of the media id's own time, so the key never changes for an id.
pub fn object_key(prefix: &str, project_id: ProjectId, id: MediaObjectId) -> String {
    let millis = id.uuid().v7_parts().map_or(0, |(millis, _)| millis);
    let date =
        Timestamp::from_micros(i64::try_from(millis.saturating_mul(1_000)).unwrap_or(0)).date();
    let lead = if prefix.is_empty() { "" } else { "/" };
    format!(
        "{prefix}{lead}{project_id}/{:04}/{:02}/{id}",
        date.year(),
        date.month()
    )
}

/// Where every key of the deployment starts: what the orphan scan lists.
pub fn key_root(prefix: &str) -> String {
    if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    }
}

/// The media id a key of this layout names; `None` for any other key, which the orphan scan leaves alone.
pub fn media_id_of(prefix: &str, key: &str) -> Option<(ProjectId, MediaObjectId)> {
    let rest = key.strip_prefix(&key_root(prefix))?;
    let mut parts = rest.split('/');
    let project_id = ProjectId::parse(parts.next()?).ok()?;
    let (year, month) = (parts.next()?, parts.next()?);
    let id = MediaObjectId::parse(parts.next()?).ok()?;
    if parts.next().is_some() || object_key(prefix, project_id, id) != key {
        return None;
    }
    let digits =
        |text: &str, len: usize| text.len() == len && text.bytes().all(|b| b.is_ascii_digit());
    (digits(year, 4) && digits(month, 2)).then_some((project_id, id))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn scriptable_and_document_types_are_never_inline() {
        for content_type in [
            "image/svg+xml",
            "text/html",
            "application/xml",
            "application/javascript",
            "application/pdf",
            "text/plain",
            "application/octet-stream",
        ] {
            assert!(allowed(content_type), "{content_type}");
            assert_eq!(disposition(content_type), Disposition::Attachment);
        }
        assert_eq!(disposition("image/png"), Disposition::Inline);
        assert_eq!(disposition("video/mp4"), Disposition::Inline);
    }

    #[test]
    fn the_inline_safe_types_are_allowed_raster_images_audio_and_video() {
        for content_type in INLINE_SAFE {
            assert!(allowed(content_type), "{content_type}");
            let (kind, sub) = content_type.split_once('/').unwrap();
            assert!(
                ["image", "audio", "video"].contains(&kind),
                "{content_type}"
            );
            assert!(
                !sub.contains("svg") && !sub.contains("xml"),
                "{content_type}"
            );
        }
    }

    #[test]
    fn keys_follow_the_layout_and_name_their_media_id() {
        let project = ProjectId::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b").unwrap();
        // 2026-11-19T00:00:00Z.
        let id =
            MediaObjectId::from_uuid(crucible_records::Uuid::from_v7_parts(1_795_046_400_000, 7));
        let key = object_key("media", project, id);
        assert_eq!(key, format!("media/{project}/2026/11/{id}"));
        assert_eq!(media_id_of("media", &key), Some((project, id)));
        let bare = object_key("", project, id);
        assert_eq!(bare, format!("{project}/2026/11/{id}"));
        assert_eq!(media_id_of("", &bare), Some((project, id)));
        for other in [
            format!("other/{project}/2026/11/{id}"),
            format!("media/{project}/2026/12/{id}"),
            format!("media/{project}/2026/11/{id}/x"),
            format!("media/{project}/2026/11/not-an-id"),
            "media/readme.txt".to_owned(),
        ] {
            assert_eq!(media_id_of("media", &other), None, "{other}");
        }
    }

    proptest! {
        /// The inline-safe rule: a type is served inline exactly when it is on the inline-safe list, and every
        /// other text, allowed or not, is an attachment.
        #[test]
        fn only_listed_types_are_inline(content_type in "[a-z+./-]{0,40}") {
            let inline = disposition(&content_type) == Disposition::Inline;
            prop_assert_eq!(inline, INLINE_SAFE.contains(&content_type.as_str()));
            if inline {
                prop_assert!(allowed(&content_type));
            }
        }

        /// Every key the layout makes names its own media id back, whatever the prefix and the id.
        #[test]
        fn keys_round_trip(prefix in "[a-z0-9-]{0,12}", millis in 0u64..4_102_444_800_000, rand: u128, project: [u8; 16]) {
            let project = ProjectId::from_bytes(project);
            let id = MediaObjectId::from_uuid(crucible_records::Uuid::from_v7_parts(millis, rand));
            let key = object_key(&prefix, project, id);
            prop_assert!(key.starts_with(&key_root(&prefix)));
            prop_assert_eq!(media_id_of(&prefix, &key), Some((project, id)));
        }
    }
}
