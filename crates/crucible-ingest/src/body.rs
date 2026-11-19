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

/// Inflates a gzip body of one or more members to at most [`MAX_BODY_BYTES`]. `reserve(bytes)` grows the
/// in-flight reservation to cover an output of `bytes` before the output holds them, so the reservation follows
/// the bytes held; its refusal ends the inflate.
///
/// The last member's trailer states its inflated size modulo 2^32 (RFC 1952). That is the client's claim, so it
/// is only a hint, clamped to the cap and to deflate's largest expansion of the body: when the reservation can
/// hold it, the output is allocated once at that size and the decoder writes straight into it. A body whose
/// output outgrows the hint (several members, or a trailer that understates) continues as it would without
/// one, and a trailer that lies still fails the decoder's check, so neither the output nor any refusal depends
/// on the hint.
pub(crate) fn inflate(
    compressed: &[u8],
    mut reserve: impl FnMut(u64) -> Result<(), Reject>,
) -> Result<Vec<u8>, Reject> {
    let mut decoder = MultiGzDecoder::new(compressed);
    let mut out = Vec::new();
    let hint = trailer_size(compressed)
        .min(MAX_BODY_BYTES)
        .min(compressed.len().saturating_mul(MAX_INFLATE_RATIO));
    if hint > 0 && reserve(hint as u64).is_ok() {
        out.resize(hint, 0);
        let mut filled = 0;
        while let Some(spare) = out.get_mut(filled..).filter(|spare| !spare.is_empty()) {
            match decoder.read(spare).map_err(malformed)? {
                0 => {
                    out.truncate(filled);
                    return Ok(out);
                }
                n => filled += n,
            }
        }
        // The hint is used up: one byte more tells the end from a body that outgrows it.
        let mut next = [0; 1];
        if decoder.read(&mut next).map_err(malformed)? == 0 {
            return Ok(out);
        }
        let len = out.len() + 1;
        if len > MAX_BODY_BYTES {
            return Err(too_large("the inflated request body", MAX_BODY_BYTES));
        }
        reserve(len as u64)?;
        out.push(next[0]);
    }
    let mut chunk = vec![0; INFLATE_CHUNK];
    loop {
        let n = decoder.read(&mut chunk).map_err(malformed)?;
        if n == 0 {
            return Ok(out);
        }
        let len = out.len() + n;
        if len > MAX_BODY_BYTES {
            return Err(too_large("the inflated request body", MAX_BODY_BYTES));
        }
        reserve(len as u64)?;
        out.extend_from_slice(chunk.get(..n).unwrap_or_default());
    }
}

fn malformed(e: std::io::Error) -> Reject {
    Reject::Malformed(format!("the gzip body does not inflate: {e}"))
}

/// Deflate's largest expansion: a stream of maximal back-references inflates to at most about 1,032 times its
/// size, so no honest body inflates beyond that.
const MAX_INFLATE_RATIO: usize = 1_032;

/// The inflated size, modulo 2^32, that the trailer of a gzip body's last member states: its last four bytes,
/// little-endian. 0 for a body too short to hold a member.
fn trailer_size(compressed: &[u8]) -> usize {
    match compressed.len().checked_sub(4).map(|at| compressed.split_at(at).1) {
        Some(&[a, b, c, d]) if compressed.len() >= 18 => u32::from_le_bytes([a, b, c, d]) as usize,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {

    use std::io::Write as _;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use proptest::prelude::*;

    use super::*;

    fn gzip(body: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    /// Text-like bytes that compress about as an OTLP body of word payloads does.
    fn words(len: usize, seed: u64) -> Vec<u8> {
        const WORDS: [&str; 8] = [
            "agent ", "tool ", "model ", "prompt ", "token ", "query ", "result ", "the ",
        ];
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(WORDS[(state % 8) as usize].as_bytes());
        }
        out.truncate(len);
        out
    }

    fn unlimited(_: u64) -> Result<(), Reject> {
        Ok(())
    }

    /// The work count of SPD-15's inflate item: an honest single-member body is inflated into one allocation of
    /// exactly its size, which its trailer states, instead of a buffer that doubles from 64 KiB.
    #[test]
    fn an_honest_body_is_inflated_into_one_allocation_of_its_size() {
        for len in [1_000, 102_400, 1_048_576 + 12_345] {
            let body = words(len, 7);
            let out = inflate(&gzip(&body), unlimited).unwrap();
            assert_eq!(out, body);
            assert_eq!(out.capacity(), out.len(), "a {len}-byte body");
        }
    }

    /// The inflate before SPD-15: 64 KiB reads copied into a buffer that doubles. The timing run compares it.
    fn doubling_inflate(compressed: &[u8]) -> Vec<u8> {
        let mut decoder = MultiGzDecoder::new(compressed);
        let (mut out, mut chunk) = (Vec::new(), vec![0; INFLATE_CHUNK]);
        loop {
            let n = decoder.read(&mut chunk).unwrap();
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&chunk[..n]);
        }
    }

    /// A timing run of the inflate of the 10 KiB gzip lane's 1 MiB bodies and the 1 KiB lane's 100 KiB bodies on
    /// the production clock, against the inflate before SPD-15, alternating in rounds; medians of the rounds. Run
    /// with `cargo test --release -p crucible-ingest --lib inflate_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing run; see the comment"]
    fn inflate_throughput() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let env = crucible_env_prod::build(
            runtime.handle().clone(),
            &crucible_env_prod::ProdConfig::default(),
        )
        .unwrap()
        .env;
        for (len, bodies) in [(1_048_576, 40), (102_400, 400)] {
            let compressed = gzip(&words(len, 11));
            let (mut before, mut after) = (Vec::new(), Vec::new());
            for _ in 0..21 {
                let started = env.clock.now();
                for _ in 0..bodies {
                    std::hint::black_box(doubling_inflate(std::hint::black_box(&compressed)));
                }
                before.push(env.clock.now().saturating_duration_since(started));
                let started = env.clock.now();
                for _ in 0..bodies {
                    std::hint::black_box(
                        inflate(std::hint::black_box(&compressed), unlimited).unwrap(),
                    );
                }
                after.push(env.clock.now().saturating_duration_since(started));
            }
            before.sort_unstable();
            after.sort_unstable();
            let per_body = |rounds: &[std::time::Duration]| {
                rounds[rounds.len() / 2].as_secs_f64() / f64::from(bodies) * 1e6
            };
            let (old, new) = (per_body(&before), per_body(&after));
            println!(
                "inflate {len}-byte bodies ({} compressed): {old:.1} us per body before, {new:.1} us after \
                 ({:+.1}%)",
                compressed.len(),
                (new / old - 1.0) * 100.0
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Whatever a body's members and trailers, the output and every refusal are those of a plain read of
        /// the members: an over-the-budget output is refused the same way, and a corrupt trailer still fails.
        #[test]
        fn outputs_and_refusals_follow_the_body_not_its_trailer(
            len in 0usize..300_000,
            split in 0usize..300_000,
            members in 1usize..3,
            trailer in proptest::option::of(any::<u32>()),
            budget in prop_oneof![Just(u64::MAX), 0u64..400_000],
            seed: u64,
        ) {
            let body = words(len, seed);
            let at = split.min(len);
            let mut compressed = if members == 1 {
                gzip(&body)
            } else {
                [gzip(&body[..at]), gzip(&body[at..])].concat()
            };
            if let Some(size) = trailer {
                let end = compressed.len();
                compressed[end - 4..].copy_from_slice(&size.to_le_bytes());
            }
            let refusal = Reject::TooManyRequests { retry_after_s: 1, detail: "budget" };
            let reserve = |bytes: u64| if bytes <= budget { Ok(()) } else { Err(refusal.clone()) };
            // A plain read keeps what inflated before an error: past the budget, the reservation refuses first.
            let mut reference = Vec::new();
            let read = MultiGzDecoder::new(compressed.as_slice()).read_to_end(&mut reference);
            let expected = if reference.len() as u64 > budget {
                Err("budget")
            } else if read.is_err() {
                Err("malformed")
            } else {
                Ok(reference)
            };
            let got = match inflate(&compressed, reserve) {
                Ok(out) => Ok(out),
                Err(Reject::Malformed(_)) => Err("malformed"),
                Err(other) => {
                    prop_assert_eq!(&other, &refusal);
                    Err("budget")
                }
            };
            prop_assert_eq!(got, expected);
        }
    }
}
