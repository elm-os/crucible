//! Optional media on S3-compatible object storage (contract 14).
//!
//! - [`S3Store`]: the production [`crucible_env::ObjectStore`], signed with `rusty-s3` and sent through `Net`,
//!   with an internal endpoint for this node's calls and a public one that presigned URLs name.
//! - [`Media`]: the flow of contract 14.2: a presigned PUT whose content type, length and checksum are signed
//!   headers, deduplicated by project and SHA-256; the confirm by HEAD; a presigned GET served inline only for
//!   the inline-safe types.
//! - [`gc::job`]: the `media-gc` leader job: retention by reference time, stale uploads, unused objects and
//!   the weekly orphan scan.
//! - [`rules`]: the allowlist, the inline-safe list, the caps, the lifetimes and the object key layout.
//! - [`Media::span_references`]: the media a span references, read out of its payload with the reader the binary
//!   passes in ([`FindRefs`]), and the uses recorded for it.
//!
//! Without an object store none of this is built, and every media route answers `media_not_supported`.

pub mod gc;
mod http;
pub mod rules;
mod s3;
mod service;

#[cfg(feature = "test-support")]
pub use http::fetch;
pub use s3::{INTERNAL_TTL, LIST_PAGE, S3Config, S3Store};
pub use service::{
    Created, FindRefs, HashRange, MAX_SPAN_REFERENCES, Media, MediaConfig, MediaError, NewUpload,
    PayloadRef, SpanReference,
};
