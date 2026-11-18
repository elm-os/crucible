//! The request body: its media type and content coding, and reading and inflating it under the caps of
//! contract 4.3 and 4.8 while the in-flight reservation follows the bytes held.

use std::io::Read as _;

use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use crucible_otlp::MAX_BODY_BYTES;
use flate2::read::MultiGzDecoder;
use http_body_util::BodyExt as _;

use crate::admission::Held;
use crate::reply::Reject;

/// The largest plain body: the canonical body cap. A JSON body is held to it too, before transcoding.
pub(crate) const MAX_PLAIN_BYTES: usize = MAX_BODY_BYTES;

/// The largest gzip body as received: the canonical body cap plus deflate's worst-case growth on data that
/// does not compress (5 bytes per stored block of up to 65,535 bytes, plus the gzip header and trailer), so
/// that a body at the cap still fits once compressed.
pub(crate) const MAX_GZIP_BYTES: usize = MAX_BODY_BYTES + MAX_BODY_BYTES / 8_192 + 1_024;

/// Bytes read from the decoder per step.
const INFLATE_CHUNK: usize = 64 * 1024;

/// The encoding of an OTLP body, and of the response to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// `application/x-protobuf`.
    Protobuf,
    /// `application/json`, transcoded to protobuf before the append.
    Json,
}

impl Format {
    /// The format named by `Content-Type`, ignoring case and parameters; `None` for any other media type.
    pub(crate) fn of(headers: &HeaderMap) -> Option<Self> {
        let value = headers.get(CONTENT_TYPE)?.to_str().ok()?;
        let media = value.split(';').next().unwrap_or_default().trim();
        if media.eq_ignore_ascii_case("application/x-protobuf") {
            Some(Self::Protobuf)
        } else if media.eq_ignore_ascii_case("application/json") {
            Some(Self::Json)
        } else {
            None
        }
    }

    /// The `Content-Type` of a response in this format.
    pub(crate) const fn content_type(self) -> &'static str {
        match self {
            Self::Protobuf => "application/x-protobuf",
            Self::Json => "application/json",
        }
    }
}

/// Whether the body is gzip-compressed. No `Content-Encoding`, or `identity`, means a plain body; any other
/// coding is refused.
pub(crate) fn is_gzip(headers: &HeaderMap) -> Result<bool, Reject> {
    let Some(value) = headers.get(CONTENT_ENCODING) else {
        return Ok(false);
    };
    let coding = value.to_str().map(str::trim);
    match coding {
        Ok(c) if c.eq_ignore_ascii_case("gzip") || c.eq_ignore_ascii_case("x-gzip") => Ok(true),
        Ok(c) if c.is_empty() || c.eq_ignore_ascii_case("identity") => Ok(false),
        _ => Err(Reject::UnsupportedMediaType(
            "the content coding is not supported; send gzip or an uncompressed body".to_owned(),
        )),
    }
}

/// The declared body length, when `Content-Length` is present.
pub(crate) fn declared_len(headers: &HeaderMap) -> Result<Option<u64>, Reject> {
    let Some(value) = headers.get(CONTENT_LENGTH) else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .map(Some)
        .ok_or_else(|| Reject::Malformed("Content-Length is not a number".to_owned()))
}

pub(crate) fn too_large(what: &str, cap: usize) -> Reject {
    Reject::TooLarge(format!("{what} is above the cap of {cap} bytes"))
}

/// Reads the whole body, at most `cap` bytes, growing the reservation as data arrives.
pub(crate) async fn read(
    mut body: Body,
    cap: usize,
    declared: Option<u64>,
    held: &mut Held,
) -> Result<Vec<u8>, Reject> {
    let expected = declared.and_then(|len| usize::try_from(len).ok());
    let mut out = Vec::with_capacity(expected.unwrap_or(0).min(cap));
    while let Some(frame) = body.frame().await {
        let frame = frame
            .map_err(|e| Reject::Malformed(format!("reading the request body failed: {e}")))?;
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let len = out.len().saturating_add(data.len());
        if len > cap {
            return Err(too_large("the request body", cap));
        }
        held.grow_to(len as u64)?;
        out.extend_from_slice(&data);
    }
    held.set(out.len() as u64)?;
    Ok(out)
}

/// Inflates a gzip body of one or more members to at most [`MAX_BODY_BYTES`], growing the reservation as the
/// output grows on top of the `base` bytes held for other buffers.
pub(crate) fn inflate(compressed: &[u8], base: u64, held: &mut Held) -> Result<Vec<u8>, Reject> {
    let mut decoder = MultiGzDecoder::new(compressed);
    let mut out = Vec::new();
    let mut chunk = vec![0; INFLATE_CHUNK];
    loop {
        let n = decoder
            .read(&mut chunk)
            .map_err(|e| Reject::Malformed(format!("the gzip body does not inflate: {e}")))?;
        if n == 0 {
            return Ok(out);
        }
        let len = out.len() + n;
        if len > MAX_BODY_BYTES {
            return Err(too_large("the inflated request body", MAX_BODY_BYTES));
        }
        held.grow_to(base + len as u64)?;
        out.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
}
