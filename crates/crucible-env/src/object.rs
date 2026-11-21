//! `ObjectStore` (contract 2.10): optional S3-compatible object storage, for media and export files.
//!
//! Keys are full object keys; the layout under a deployment's prefix belongs to the caller. Presigned
//! requests are signed for the store's public endpoint, which clients reach directly; every other call goes
//! to its internal endpoint. Production is the S3 adapter of `crucible-media`; the simulation models lost
//! uploads, eventually consistent listings, throttling and objects that differ from what was signed.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_records::Timestamp;
use futures::stream::BoxStream;

/// How a presigned download asks the browser to treat the object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Disposition {
    /// Shown in the page.
    Inline,
    /// Saved as a file.
    Attachment,
}

impl Disposition {
    /// The `Content-Disposition` value.
    pub const fn header(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Attachment => "attachment",
        }
    }
}

/// A presigned request: the URL, the headers the client must send with it exactly as given, and when the
/// signature expires.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Presigned {
    /// The URL, on the public endpoint.
    pub url: String,
    /// The signed headers, by lowercase name. A request without one of them, or with another value, is
    /// refused by the store.
    pub headers: Vec<(String, String)>,
    /// When the URL stops working.
    pub expires_at: Timestamp,
}

/// What a store reports about one object.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ObjectMeta {
    /// The key.
    pub key: String,
    /// The length in bytes.
    pub len: u64,
    /// The SHA-256 the store recorded at upload, when it returns one.
    pub sha256: Option<[u8; 32]>,
    /// When the store wrote the object.
    pub created_at: Timestamp,
}

/// One page of a listing, in key order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ObjectPage {
    /// The objects.
    pub objects: Vec<ObjectMeta>,
    /// Where the next page starts (pass it as `after`); `None` on the last page.
    pub next: Option<String>,
}

/// Why a call to the store failed.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ObjectError {
    /// No object has this key.
    #[error("no object has this key")]
    NotFound,
    /// The store did not answer, answered with a server error, or asked to slow down. Retrying later can
    /// succeed.
    #[error("the object store is unavailable: {0}")]
    Unavailable(String),
    /// The store refused the request: its HTTP status and the store's error code. Usually the credentials,
    /// the bucket or the endpoint are wrong.
    #[error("the object store refused the request with {status} {code}")]
    Rejected {
        /// The HTTP status.
        status: u16,
        /// The store's error code, such as `AccessDenied` or `NoSuchBucket`.
        code: String,
    },
    /// The request could not be built, for example from a key the store cannot hold.
    #[error("invalid object store request: {0}")]
    Invalid(String),
}

/// The bytes of a stored object, in order.
pub type ByteStream = BoxStream<'static, Result<Bytes, ObjectError>>;

/// The smallest part of a multipart upload, except its last part, which may be shorter.
pub const MIN_PART_BYTES: usize = 5 << 20;

/// S3-compatible object storage behind presigned URLs (contract 2.10).
#[async_trait]
pub trait ObjectStore: fmt::Debug + Send + Sync {
    /// A presigned PUT of `len` bytes of `content_type` whose SHA-256 is `sha256`, valid for `ttl`. The content
    /// type, the length and the checksum are signed headers, so the store refuses any other body or type.
    fn presign_put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &[u8; 32],
        len: u64,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError>;

    /// A presigned GET valid for `ttl`, whose answer carries `disposition`.
    fn presign_get(
        &self,
        key: &str,
        disposition: Disposition,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError>;

    /// The object's length, checksum and write time; `None` when there is no such object.
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, ObjectError>;

    /// Stores `bytes` of `content_type` under `key` in one request, replacing what was there.
    async fn put_small(&self, key: &str, content_type: &str, bytes: Bytes)
    -> Result<(), ObjectError>;

    /// Starts a multipart upload of an object of `content_type` under `key`, and returns the store's id for
    /// it. Nothing appears under the key until the upload is finished.
    async fn start_upload(&self, key: &str, content_type: &str) -> Result<String, ObjectError>;

    /// Uploads part `number` (1 to 10,000) of an upload, and returns the part's `ETag`. Every part but the
    /// last must hold at least [`MIN_PART_BYTES`].
    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u16,
        bytes: Bytes,
    ) -> Result<String, ObjectError>;

    /// Joins the parts, given by their `ETag`s in part order from part 1, into the object under `key`, which
    /// then appears whole.
    async fn finish_upload(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> Result<(), ObjectError>;

    /// Abandons an upload and frees its parts. Abandoning an upload that is gone succeeds.
    async fn abort_upload(&self, key: &str, upload_id: &str) -> Result<(), ObjectError>;

    /// The object's bytes; [`ObjectError::NotFound`] when there is no such object.
    async fn get_stream(&self, key: &str) -> Result<ByteStream, ObjectError>;

    /// Deletes the object. Deleting a key that holds nothing succeeds.
    async fn delete(&self, key: &str) -> Result<(), ObjectError>;

    /// Deletes up to 1,000 objects in one call; keys that hold nothing are skipped.
    async fn delete_batch(&self, keys: &[String]) -> Result<(), ObjectError>;

    /// One page of the keys under `prefix` that sort after `after`, in key order. A listing may lag
    /// behind recent writes and deletes.
    async fn list(&self, prefix: &str, after: Option<&str>) -> Result<ObjectPage, ObjectError>;
}
