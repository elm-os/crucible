//! The production [`ObjectStore`]: an S3-compatible bucket, signed with `rusty-s3` and reached through `Net`.
//!
//! Every request is presigned. Presigned URLs for clients are signed for the public endpoint, whose host the
//! signature covers; the calls this node makes itself go to the internal endpoint with a URL that lives
//! [`INTERNAL_TTL`]. Signatures take their time from the `Env` clock: `rusty_s3::S3Action::sign` reads the
//! ambient clock and is never called.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use crucible_env::{
    ByteStream, Disposition, Env, ObjectError, ObjectMeta, ObjectPage, ObjectStore, Presigned,
    TlsRoots,
};
use crucible_records::Timestamp;
use futures::StreamExt as _;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED};
use http::{HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt as _;
use rusty_s3::actions::{CreateMultipartUpload, ListObjectsV2, ObjectIdentifier};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use url::Url;

use crate::http::{Answer, error_code, send, unavailable};

/// How long the URL of a request this node sends itself is valid.
pub const INTERNAL_TTL: Duration = Duration::from_secs(300);
/// The most keys one listing page returns.
pub const LIST_PAGE: usize = 1_000;
/// The header that asks a HEAD for the stored checksum.
const CHECKSUM_MODE: &str = "x-amz-checksum-mode";
/// The checksum header of an upload and of a HEAD answer.
const CHECKSUM: &str = "x-amz-checksum-sha256";

/// Where the bucket is and how to sign for it (`[object_store]`, contract 14.1).
#[derive(Clone, PartialEq, Eq)]
pub struct S3Config {
    /// The endpoint this node reaches, for example `http://versitygw:7070`.
    pub endpoint: String,
    /// The endpoint clients reach, which presigned URLs name; the internal one when `None`.
    pub public_endpoint: Option<String>,
    /// The region the signatures name.
    pub region: String,
    /// The bucket.
    pub bucket: String,
    /// The access key id.
    pub access_key: String,
    /// The secret access key.
    pub secret_key: String,
    /// Virtual-host-style URLs (`bucket.host`) instead of path-style ones (`host/bucket`).
    pub virtual_host: bool,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("public_endpoint", &self.public_endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("virtual_host", &self.virtual_host)
            .finish_non_exhaustive()
    }
}

/// An S3-compatible bucket behind [`ObjectStore`].
#[derive(Clone)]
pub struct S3Store {
    env: Env,
    internal: Bucket,
    public: Bucket,
    credentials: Credentials,
    roots: TlsRoots,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("internal", &self.internal.base_url().as_str())
            .field("public", &self.public.base_url().as_str())
            .finish_non_exhaustive()
    }
}

fn bucket(endpoint: &str, config: &S3Config, setting: &str) -> Result<Bucket, ObjectError> {
    let url = Url::parse(endpoint).map_err(|error| {
        ObjectError::Invalid(format!(
            "{setting} must be an http or https URL such as http://localhost:7070: {error}"
        ))
    })?;
    let style = if config.virtual_host {
        UrlStyle::VirtualHost
    } else {
        UrlStyle::Path
    };
    Bucket::new(url, style, config.bucket.clone(), config.region.clone()).map_err(|error| {
        ObjectError::Invalid(format!(
            "{setting} and object_store.bucket do not make a bucket URL ({error:?}); use an http or https URL \
             and a bucket name of lowercase letters, digits, `.` and `-`"
        ))
    })
}

/// The `Env` clock's time as the signer takes it.
fn signing_time(now: Timestamp) -> jiff::Timestamp {
    jiff::Timestamp::from_microsecond(now.micros()).unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// A time an S3 answer names: RFC 3339 in listings, the HTTP date format in `Last-Modified`.
fn parse_time(text: &str) -> Option<Timestamp> {
    let parsed = text.parse::<jiff::Timestamp>().ok().or_else(|| {
        jiff::fmt::rfc2822::parse(text)
            .ok()
            .map(|zoned| zoned.timestamp())
    })?;
    Some(Timestamp::from_micros(parsed.as_microsecond()))
}

fn header(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), ObjectError> {
    let invalid = |error: &dyn fmt::Display| ObjectError::Invalid(error.to_string());
    Ok((
        HeaderName::from_bytes(name.as_bytes()).map_err(|e| invalid(&e))?,
        HeaderValue::from_str(value).map_err(|e| invalid(&e))?,
    ))
}

impl S3Store {
    /// The store for `config`, sending through `env.net` and verifying TLS peers against `roots`. Refuses a
    /// configuration whose endpoints or bucket name do not make URLs, with the setting to fix.
    pub fn new(env: &Env, config: &S3Config, roots: TlsRoots) -> Result<Self, ObjectError> {
        let internal = bucket(&config.endpoint, config, "object_store.endpoint")?;
        let public = match &config.public_endpoint {
            Some(endpoint) => bucket(endpoint, config, "object_store.public_endpoint")?,
            None => internal.clone(),
        };
        Ok(Self {
            env: env.clone(),
            internal,
            public,
            credentials: Credentials::new(config.access_key.clone(), config.secret_key.clone()),
            roots,
        })
    }

    fn now(&self) -> Timestamp {
        self.env.clock.wall_now()
    }

    /// The origin of the public endpoint, as a Content-Security-Policy source: where browsers load media from.
    pub fn public_origin(&self) -> String {
        self.public.base_url().origin().ascii_serialization()
    }

    fn presigned<'a>(
        &self,
        action: &impl S3Action<'a>,
        ttl: Duration,
        headers: Vec<(String, String)>,
    ) -> Presigned {
        let now = self.now();
        let url = action.sign_with_time(ttl, &signing_time(now));
        let micros = i64::try_from(ttl.as_micros()).unwrap_or(i64::MAX);
        Presigned {
            url: url.to_string(),
            headers,
            expires_at: now.saturating_add_micros(micros),
        }
    }

    async fn call<'a>(
        &self,
        method: Method,
        action: &impl S3Action<'a>,
        headers: &[(HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<Answer, ObjectError> {
        let url = action.sign_with_time(INTERNAL_TTL, &signing_time(self.now()));
        send(&self.env, &self.roots, method, &url, headers, body).await
    }

    /// Checks that the bucket answers with these credentials: what `crucible serve` runs at startup to say
    /// early what is wrong.
    pub async fn check(&self) -> Result<(), ObjectError> {
        let action = self.internal.head_bucket(Some(&self.credentials));
        let answer = self.call(Method::HEAD, &action, &[], Bytes::new()).await?;
        if answer.status.is_success() {
            Ok(())
        } else {
            Err(answer.failure(&self.env).await)
        }
    }
}

#[async_trait::async_trait]
impl ObjectStore for S3Store {
    fn presign_put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &[u8; 32],
        len: u64,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError> {
        let checksum = STANDARD.encode(sha256);
        let headers = vec![
            (CONTENT_TYPE.as_str().to_owned(), content_type.to_owned()),
            (CONTENT_LENGTH.as_str().to_owned(), len.to_string()),
            (CHECKSUM.to_owned(), checksum),
        ];
        let mut action = self.public.put_object(Some(&self.credentials), key);
        for (name, value) in &headers {
            action.headers_mut().insert(name.clone(), value.clone());
        }
        Ok(self.presigned(&action, ttl, headers))
    }

    fn presign_get(
        &self,
        key: &str,
        disposition: Disposition,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError> {
        let mut action = self.public.get_object(Some(&self.credentials), key);
        action
            .query_mut()
            .insert("response-content-disposition", disposition.header());
        Ok(self.presigned(&action, ttl, Vec::new()))
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, ObjectError> {
        let mut action = self.internal.head_object(Some(&self.credentials), key);
        action.headers_mut().insert(CHECKSUM_MODE, "ENABLED");
        let mode = header(CHECKSUM_MODE, "ENABLED")?;
        let answer = self
            .call(Method::HEAD, &action, &[mode], Bytes::new())
            .await?;
        if answer.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !answer.status.is_success() {
            return Err(answer.failure(&self.env).await);
        }
        let text = |name: &str| {
            answer
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let len = text(CONTENT_LENGTH.as_str())
            .and_then(|len| len.parse().ok())
            .ok_or_else(|| unavailable("a HEAD answer has no Content-Length"))?;
        let sha256 = text(CHECKSUM)
            .and_then(|value| STANDARD.decode(value).ok())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok());
        let created_at = text(LAST_MODIFIED.as_str())
            .and_then(|value| parse_time(&value))
            .unwrap_or(Timestamp::EPOCH);
        Ok(Some(ObjectMeta {
            key: key.to_owned(),
            len,
            sha256,
            created_at,
        }))
    }

    async fn put_small(
        &self,
        key: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<(), ObjectError> {
        let mut action = self.internal.put_object(Some(&self.credentials), key);
        action
            .headers_mut()
            .insert(CONTENT_TYPE.as_str(), content_type.to_owned());
        let typed = header(CONTENT_TYPE.as_str(), content_type)?;
        let answer = self.call(Method::PUT, &action, &[typed], bytes).await?;
        if answer.status.is_success() {
            Ok(())
        } else {
            Err(answer.failure(&self.env).await)
        }
    }

    async fn start_upload(&self, key: &str, content_type: &str) -> Result<String, ObjectError> {
        let mut action = self
            .internal
            .create_multipart_upload(Some(&self.credentials), key);
        action
            .headers_mut()
            .insert(CONTENT_TYPE.as_str(), content_type.to_owned());
        let typed = header(CONTENT_TYPE.as_str(), content_type)?;
        let answer = self
            .call(Method::POST, &action, &[typed], Bytes::new())
            .await?;
        if !answer.status.is_success() {
            return Err(answer.failure(&self.env).await);
        }
        let body = answer.bytes(&self.env).await?;
        let started = CreateMultipartUpload::parse_response(&String::from_utf8_lossy(&body))
            .map_err(|error| unavailable(format!("unreadable answer to an upload start: {error}")))?;
        Ok(started.upload_id().to_owned())
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u16,
        bytes: Bytes,
    ) -> Result<String, ObjectError> {
        let action = self
            .internal
            .upload_part(Some(&self.credentials), key, number, upload_id);
        let answer = self.call(Method::PUT, &action, &[], bytes).await?;
        if !answer.status.is_success() {
            return Err(answer.failure(&self.env).await);
        }
        answer
            .headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| unavailable("the answer to an uploaded part has no ETag"))
    }

    async fn finish_upload(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> Result<(), ObjectError> {
        let action = self.internal.complete_multipart_upload(
            Some(&self.credentials),
            key,
            upload_id,
            etags.iter().map(String::as_str),
        );
        let url = action.sign_with_time(INTERNAL_TTL, &signing_time(self.now()));
        let body = action.body();
        let headers = [header("content-type", "application/xml")?];
        let answer = send(
            &self.env,
            &self.roots,
            Method::POST,
            &url,
            &headers,
            Bytes::from(body),
        )
        .await?;
        if !answer.status.is_success() {
            return Err(answer.failure(&self.env).await);
        }
        // The store may answer 200 and still fail, with an error document as the body.
        let body = answer.bytes(&self.env).await?;
        match error_code(&body) {
            Some(code) => Err(unavailable(format!(
                "the store did not finish the upload: {code}"
            ))),
            None => Ok(()),
        }
    }

    async fn abort_upload(&self, key: &str, upload_id: &str) -> Result<(), ObjectError> {
        let action = self
            .internal
            .abort_multipart_upload(Some(&self.credentials), key, upload_id);
        let answer = self
            .call(Method::DELETE, &action, &[], Bytes::new())
            .await?;
        if answer.status.is_success() || answer.status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(answer.failure(&self.env).await)
        }
    }

    async fn get_stream(&self, key: &str) -> Result<ByteStream, ObjectError> {
        let action = self.internal.get_object(Some(&self.credentials), key);
        let answer = self.call(Method::GET, &action, &[], Bytes::new()).await?;
        if !answer.status.is_success() {
            return Err(answer.failure(&self.env).await);
        }
        Ok(answer
            .body
            .into_data_stream()
            .map(|chunk| {
                chunk.map_err(|error| unavailable(format!("reading the object failed: {error}")))
            })
            .boxed())
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        let action = self.internal.delete_object(Some(&self.credentials), key);
        let answer = self
            .call(Method::DELETE, &action, &[], Bytes::new())
            .await?;
        if answer.status.is_success() || answer.status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(answer.failure(&self.env).await)
        }
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), ObjectError> {
        if keys.is_empty() {
            return Ok(());
        }
        let objects: Vec<ObjectIdentifier> = keys
            .iter()
            .map(|key| ObjectIdentifier::new(key.clone()))
            .collect();
        let mut action = self
            .internal
            .delete_objects(Some(&self.credentials), objects.iter());
        action.set_quiet(true);
        let url = action.sign_with_time(INTERNAL_TTL, &signing_time(self.now()));
        let (body, md5) = action.body_with_md5();
        let headers = [
            header("content-md5", &md5)?,
            header("content-type", "application/xml")?,
        ];
        let answer = send(
            &self.env,
            &self.roots,
            Method::POST,
            &url,
            &headers,
            Bytes::from(body),
        )
        .await?;
        if !answer.status.is_success() {
            return Err(answer.failure(&self.env).await);
        }
        let body = answer.bytes(&self.env).await?;
        let text = String::from_utf8_lossy(&body);
        match rusty_s3::actions::DeleteObjectsResponse::parse(&text) {
            Ok(result) => match result.errors.iter().find(|error| error.code != "NoSuchKey") {
                Some(error) => Err(unavailable(format!(
                    "the store did not delete {}: {}",
                    error.key, error.code
                ))),
                None => Ok(()),
            },
            // A quiet answer may be an empty document.
            Err(_) if !text.contains("<Error>") => Ok(()),
            Err(error) => Err(unavailable(format!(
                "unreadable batch delete answer: {error}"
            ))),
        }
    }

    async fn list(&self, prefix: &str, after: Option<&str>) -> Result<ObjectPage, ObjectError> {
        let mut action = self.internal.list_objects_v2(Some(&self.credentials));
        action.with_prefix(prefix);
        if let Some(after) = after {
            action.with_start_after(after);
        }
        action.with_max_keys(LIST_PAGE);
        let answer = self.call(Method::GET, &action, &[], Bytes::new()).await?;
        if !answer.status.is_success() {
            return Err(answer.failure(&self.env).await);
        }
        let body = answer.bytes(&self.env).await?;
        let parsed = ListObjectsV2::parse_response(&String::from_utf8_lossy(&body))
            .map_err(|error| unavailable(format!("unreadable listing: {error}")))?;
        let objects: Vec<ObjectMeta> = parsed
            .contents
            .into_iter()
            .map(|content| ObjectMeta {
                created_at: content
                    .last_modified
                    .as_deref()
                    .and_then(parse_time)
                    .unwrap_or(Timestamp::EPOCH),
                len: content.size.unwrap_or(0),
                sha256: None,
                key: content.key,
            })
            .collect();
        let next = match parsed.next_continuation_token {
            Some(_) => objects.last().map(|object| object.key.clone()),
            None => None,
        };
        Ok(ObjectPage { objects, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_in_both_s3_forms() {
        let listing = parse_time("2026-11-19T08:30:00.000Z").unwrap();
        assert_eq!(listing.to_string(), "2026-11-19T08:30:00.000000Z");
        let header = parse_time("Thu, 19 Nov 2026 08:30:00 GMT").unwrap();
        assert_eq!(header, listing);
        assert_eq!(parse_time("yesterday"), None);
    }

    #[test]
    fn signing_time_is_the_clock_time() {
        let now = Timestamp::from_micros(1_800_000_000_123_456);
        assert_eq!(
            signing_time(now).as_microsecond(),
            1_800_000_000_123_456_i64
        );
    }
}
