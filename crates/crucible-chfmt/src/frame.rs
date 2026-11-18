//! ClickHouse compressed framing: 1 MiB chunks, each compressed into one checksummed frame.

use lz4_flex::block::CompressTable;

use crate::block::EncodeError;

/// Uncompressed bytes per frame; only the last frame of a stream may carry fewer.
pub const CHUNK_SIZE: usize = 1 << 20;

const CHECKSUM_LEN: usize = 16;
/// Method byte, compressed size and uncompressed size.
const HEADER_LEN: usize = 9;
/// Largest compressed or uncompressed frame size the decoder accepts.
const MAX_FRAME_LEN: usize = 1 << 30;
const METHOD_LZ4: u8 = 0x82;
const METHOD_ZSTD: u8 = 0x90;
/// ZSTD level for inserts: ClickHouse's default level for its ZSTD codec.
const ZSTD_LEVEL: i32 = 1;

/// Compression method of the frames of an encoded block (`clickhouse.insert_compression`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compression {
    /// LZ4 block format, method byte `0x82`.
    Lz4,
    /// ZSTD frame at level 1, method byte `0x90`.
    Zstd,
}

/// Why framed bytes could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The input ends inside a frame.
    #[error("input ends inside a frame")]
    Truncated,
    /// A frame declares a size outside the accepted range.
    #[error("frame declares compressed size {compressed} and uncompressed size {uncompressed}")]
    InvalidSize {
        /// Declared compressed size, including the 9 header bytes.
        compressed: usize,
        /// Declared uncompressed size.
        uncompressed: usize,
    },
    /// The stored CityHash128 does not match the frame.
    #[error("frame checksum mismatch")]
    ChecksumMismatch,
    /// The method byte is neither LZ4 (`0x82`) nor ZSTD (`0x90`).
    #[error("unknown compression method 0x{0:02x}")]
    UnknownMethod(u8),
    /// The codec rejected the payload.
    #[error("corrupt frame payload: {0}")]
    Corrupt(String),
    /// The payload decompressed to a size other than the declared one.
    #[error("frame decompressed to {actual} bytes, declared {declared}")]
    SizeMismatch {
        /// Declared uncompressed size.
        declared: usize,
        /// Bytes the payload decompressed to.
        actual: usize,
    },
}

/// CityHash128 v1.0.2 as stored in a frame: the first 64-bit word of the hash, then the second, little-endian.
fn checksum(frame: &[u8]) -> [u8; CHECKSUM_LEN] {
    // cityhash-rs returns the first word in the high half of the u128.
    let hash = cityhash_rs::cityhash_102_128(frame);
    let mut out = [0u8; CHECKSUM_LEN];
    out[..8].copy_from_slice(&((hash >> 64) as u64).to_le_bytes());
    out[8..].copy_from_slice(&(hash as u64).to_le_bytes());
    out
}

/// Destination of the Native encoder's bytes: a plain buffer, or a [`Framer`] that compresses as bytes arrive.
pub trait Sink {
    fn put(&mut self, bytes: &[u8]);

    fn put_varint(&mut self, mut value: u64) {
        let mut buf = [0u8; 10];
        let mut len = 0;
        while value >= 0x80 {
            buf[len] = (value as u8) | 0x80;
            value >>= 7;
            len += 1;
        }
        buf[len] = value as u8;
        self.put(&buf[..=len]);
    }

    fn put_string(&mut self, value: &str) {
        self.put_varint(value.len() as u64);
        self.put(value.as_bytes());
    }
}

impl Sink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// Splits a byte stream into [`CHUNK_SIZE`] chunks and compresses each into one frame. Frame boundaries depend only
/// on the stream, never on how it was split into `put` calls.
pub struct Framer {
    method: u8,
    codec: Codec,
    /// Bytes of the current, not yet full chunk.
    pending: Vec<u8>,
    /// Compressor output for one chunk, sized for the worst case.
    scratch: Vec<u8>,
    frames: Vec<u8>,
    uncompressed: usize,
    error: Option<String>,
}

enum Codec {
    Lz4(Box<CompressTable>),
    Zstd(Box<zstd::bulk::Compressor<'static>>),
}

impl std::fmt::Debug for Framer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Framer")
            .field("method", &self.method)
            .field("pending", &self.pending.len())
            .field("frames", &self.frames.len())
            .field("uncompressed", &self.uncompressed)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl Framer {
    pub fn new(compression: Compression) -> Result<Self, EncodeError> {
        let (method, codec, bound) = match compression {
            Compression::Lz4 => (
                METHOD_LZ4,
                Codec::Lz4(Box::new(CompressTable::large())),
                lz4_flex::block::get_maximum_output_size(CHUNK_SIZE),
            ),
            Compression::Zstd => {
                let compressor = zstd::bulk::Compressor::new(ZSTD_LEVEL)
                    .map_err(|e| EncodeError::Compression(e.to_string()))?;
                (
                    METHOD_ZSTD,
                    Codec::Zstd(Box::new(compressor)),
                    zstd::compress_bound(CHUNK_SIZE),
                )
            }
        };
        Ok(Self {
            method,
            codec,
            pending: Vec::with_capacity(CHUNK_SIZE),
            scratch: vec![0; bound],
            frames: Vec::new(),
            uncompressed: 0,
            error: None,
        })
    }

    /// Flushes the last chunk and returns the frames and the uncompressed length of the stream.
    pub fn finish(mut self) -> Result<(Vec<u8>, usize), EncodeError> {
        if !self.pending.is_empty() {
            let chunk = std::mem::take(&mut self.pending);
            self.frame(&chunk);
        }
        match self.error {
            Some(error) => Err(EncodeError::Compression(error)),
            None => Ok((self.frames, self.uncompressed)),
        }
    }

    fn frame(&mut self, chunk: &[u8]) {
        if self.error.is_some() {
            return;
        }
        let compressed = match &mut self.codec {
            Codec::Lz4(table) => {
                lz4_flex::block::compress_into_with_table(chunk, &mut self.scratch, table)
                    .map_err(|e| e.to_string())
            }
            Codec::Zstd(compressor) => compressor
                .compress_to_buffer(chunk, self.scratch.as_mut_slice())
                .map_err(|e| e.to_string()),
        };
        let payload = match compressed {
            Ok(len) => &self.scratch[..len],
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let start = self.frames.len();
        self.frames.extend_from_slice(&[0; CHECKSUM_LEN]);
        self.frames.push(self.method);
        self.frames
            .extend_from_slice(&((HEADER_LEN + payload.len()) as u32).to_le_bytes());
        self.frames
            .extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        self.frames.extend_from_slice(payload);
        let sum = checksum(&self.frames[start + CHECKSUM_LEN..]);
        self.frames[start..start + CHECKSUM_LEN].copy_from_slice(&sum);
    }
}

impl Sink for Framer {
    fn put(&mut self, mut bytes: &[u8]) {
        self.uncompressed += bytes.len();
        while !bytes.is_empty() {
            if self.pending.is_empty()
                && let Some((chunk, rest)) = bytes.split_at_checked(CHUNK_SIZE)
            {
                // A whole chunk is available: compress it in place instead of copying it.
                self.frame(chunk);
                bytes = rest;
                continue;
            }
            let take = (CHUNK_SIZE - self.pending.len()).min(bytes.len());
            let (head, rest) = bytes.split_at(take);
            self.pending.extend_from_slice(head);
            bytes = rest;
            if self.pending.len() == CHUNK_SIZE {
                let chunk = std::mem::take(&mut self.pending);
                self.frame(&chunk);
                self.pending = chunk;
                self.pending.clear();
            }
        }
    }
}

/// Decodes a sequence of LZ4 or ZSTD frames, such as a `compress=1` response body, verifying every checksum.
pub fn decompress(mut input: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    let mut zstd: Option<zstd::bulk::Decompressor<'static>> = None;
    while !input.is_empty() {
        let (head, _) = input
            .split_first_chunk::<{ CHECKSUM_LEN + HEADER_LEN }>()
            .ok_or(DecodeError::Truncated)?;
        let compressed = u32::from_le_bytes([head[17], head[18], head[19], head[20]]) as usize;
        let uncompressed = u32::from_le_bytes([head[21], head[22], head[23], head[24]]) as usize;
        if !(HEADER_LEN..=MAX_FRAME_LEN).contains(&compressed) || uncompressed > MAX_FRAME_LEN {
            return Err(DecodeError::InvalidSize {
                compressed,
                uncompressed,
            });
        }
        let frame = input
            .get(CHECKSUM_LEN..CHECKSUM_LEN + compressed)
            .ok_or(DecodeError::Truncated)?;
        if checksum(frame)[..] != head[..CHECKSUM_LEN] {
            return Err(DecodeError::ChecksumMismatch);
        }
        let method = head[CHECKSUM_LEN];
        if method != METHOD_LZ4 && method != METHOD_ZSTD {
            return Err(DecodeError::UnknownMethod(method));
        }
        let payload = &frame[HEADER_LEN..];
        let start = out.len();
        out.resize(start + uncompressed, 0);
        let dst = &mut out[start..];
        let written = if method == METHOD_LZ4 {
            lz4_flex::block::decompress_into(payload, dst)
                .map_err(|e| DecodeError::Corrupt(e.to_string()))?
        } else {
            let decompressor = match &mut zstd {
                Some(decompressor) => decompressor,
                None => zstd.insert(
                    zstd::bulk::Decompressor::new()
                        .map_err(|e| DecodeError::Corrupt(e.to_string()))?,
                ),
            };
            decompressor
                .decompress_to_buffer(payload, dst)
                .map_err(|e| DecodeError::Corrupt(e.to_string()))?
        };
        if written != uncompressed {
            return Err(DecodeError::SizeMismatch {
                declared: uncompressed,
                actual: written,
            });
        }
        input = &input[CHECKSUM_LEN + compressed..];
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The body ClickHouse 26.3 returned for
    /// `SELECT * FROM values('c UInt64', 0, 1) FORMAT Native` with `compress=1`.
    const CLICKHOUSE_FRAME: [u8; 49] = [
        0x0d, 0xac, 0x04, 0x03, 0xfe, 0xfe, 0x29, 0x05, 0x3b, 0xf4, 0x4c, 0xdc, 0xd8, 0x0d, 0x0c,
        0x7a, 0x82, 0x21, 0x00, 0x00, 0x00, 0x1b, 0x00, 0x00, 0x00, 0xc3, 0x01, 0x02, 0x01, 0x63,
        0x06, 0x55, 0x49, 0x6e, 0x74, 0x36, 0x34, 0x00, 0x01, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    fn frame_bytes(data: &[u8], compression: Compression) -> Vec<u8> {
        let mut framer = Framer::new(compression).unwrap();
        framer.put(data);
        let (frames, len) = framer.finish().unwrap();
        assert_eq!(len, data.len());
        frames
    }

    /// (method, compressed size, uncompressed size) of every frame.
    fn frame_headers(mut frames: &[u8]) -> Vec<(u8, usize, usize)> {
        let mut headers = Vec::new();
        while !frames.is_empty() {
            let size =
                |at: usize| u32::from_le_bytes(frames[at..at + 4].try_into().unwrap()) as usize;
            let compressed = size(17);
            headers.push((frames[16], compressed, size(21)));
            frames = &frames[CHECKSUM_LEN + compressed..];
        }
        headers
    }

    /// Bytes that compress somewhat but not trivially.
    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 7) ^ (i >> 9)) as u8).collect()
    }

    #[test]
    fn clickhouse_frame_checksum_layout_and_payload() {
        let frame = &CLICKHOUSE_FRAME;
        assert_eq!(checksum(&frame[16..])[..], frame[..16]);
        assert_eq!(frame_headers(frame), vec![(METHOD_LZ4, 33, 27)]);
        let mut expected = vec![1, 2, 1, b'c', 6];
        expected.extend_from_slice(b"UInt64");
        expected.extend_from_slice(&0u64.to_le_bytes());
        expected.extend_from_slice(&1u64.to_le_bytes());
        assert_eq!(decompress(frame).unwrap(), expected);
    }

    #[test]
    fn chunks_split_at_exactly_one_mebibyte() {
        for compression in [Compression::Lz4, Compression::Zstd] {
            for (len, chunks) in [
                (0, vec![]),
                (1, vec![1]),
                (CHUNK_SIZE - 1, vec![CHUNK_SIZE - 1]),
                (CHUNK_SIZE, vec![CHUNK_SIZE]),
                (CHUNK_SIZE + 1, vec![CHUNK_SIZE, 1]),
                (2 * CHUNK_SIZE - 1, vec![CHUNK_SIZE, CHUNK_SIZE - 1]),
                (2 * CHUNK_SIZE, vec![CHUNK_SIZE, CHUNK_SIZE]),
                (2 * CHUNK_SIZE + 1, vec![CHUNK_SIZE, CHUNK_SIZE, 1]),
            ] {
                let data = sample(len);
                let frames = frame_bytes(&data, compression);
                let headers = frame_headers(&frames);
                let sizes: Vec<usize> = headers.iter().map(|&(_, _, u)| u).collect();
                assert_eq!(sizes, chunks, "{compression:?} {len}");
                assert_eq!(decompress(&frames).unwrap(), data, "{compression:?} {len}");
            }
        }
    }

    #[test]
    fn method_bytes() {
        let lz4 = frame_bytes(b"abc", Compression::Lz4);
        let zstd = frame_bytes(b"abc", Compression::Zstd);
        assert_eq!(lz4[16], 0x82);
        assert_eq!(zstd[16], 0x90);
    }

    #[test]
    fn decoder_rejects_bad_headers() {
        let good = frame_bytes(&sample(1000), Compression::Lz4);
        let with = |at: usize, bytes: &[u8]| {
            let mut frame = good.clone();
            frame[at..at + bytes.len()].copy_from_slice(bytes);
            let sum = checksum(&frame[16..]);
            frame[..16].copy_from_slice(&sum);
            frame
        };
        assert_eq!(
            decompress(&with(16, &[0x02])),
            Err(DecodeError::UnknownMethod(0x02))
        );
        assert_eq!(
            decompress(&with(17, &8u32.to_le_bytes())),
            Err(DecodeError::InvalidSize {
                compressed: 8,
                uncompressed: 1000
            })
        );
        assert_eq!(
            decompress(&with(21, &(MAX_FRAME_LEN as u32 + 1).to_le_bytes())),
            Err(DecodeError::InvalidSize {
                compressed: good.len() - 16,
                uncompressed: (1 << 30) + 1
            })
        );
        assert!(matches!(
            decompress(&with(21, &999u32.to_le_bytes())),
            Err(DecodeError::Corrupt(_) | DecodeError::SizeMismatch { .. })
        ));
        assert_eq!(decompress(&good[..20]), Err(DecodeError::Truncated));
        assert_eq!(decompress(&[]), Ok(Vec::new()));
    }

    fn compression() -> impl Strategy<Value = Compression> {
        prop_oneof![Just(Compression::Lz4), Just(Compression::Zstd)]
    }

    /// Mostly small inputs, sometimes long enough for two or three frames.
    fn data() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            4 => proptest::collection::vec(any::<u8>(), 0..4096),
            1 => (CHUNK_SIZE - 8..2 * CHUNK_SIZE + 8, any::<u8>()).prop_map(|(len, salt)| {
                (0..len).map(|i| (i as u8).wrapping_mul(salt) ^ (i >> 12) as u8).collect()
            }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn framing_round_trips(data in data(), compression in compression()) {
            let frames = frame_bytes(&data, compression);
            prop_assert_eq!(decompress(&frames).unwrap(), data);
        }

        #[test]
        fn framing_ignores_how_the_stream_is_split(
            data in data(),
            cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
            compression in compression(),
        ) {
            let mut points: Vec<usize> = cuts.iter().map(|cut| cut.index(data.len() + 1)).collect();
            points.sort_unstable();
            let mut framer = Framer::new(compression).unwrap();
            let mut at = 0;
            for point in points {
                framer.put(&data[at..point]);
                at = point;
            }
            framer.put(&data[at..]);
            let (split, len) = framer.finish().unwrap();
            prop_assert_eq!(len, data.len());
            prop_assert_eq!(split, frame_bytes(&data, compression));
        }

        #[test]
        fn truncated_or_flipped_frames_are_rejected(
            data in proptest::collection::vec(any::<u8>(), 1..2048),
            compression in compression(),
            at in any::<prop::sample::Index>(),
            bit in 0..8u8,
        ) {
            let frames = frame_bytes(&data, compression);
            let cut = at.index(frames.len());
            prop_assert!(decompress(&frames[..cut]).is_err() || cut == 0);
            let mut flipped = frames.clone();
            flipped[cut] ^= 1 << bit;
            prop_assert!(decompress(&flipped).is_err());
        }
    }
}
