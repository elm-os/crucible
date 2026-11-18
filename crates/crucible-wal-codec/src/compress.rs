//! The compression rule of contract section 4.3 and frame preparation (section 4.5).

use std::borrow::Cow;

use crucible_records::{Lsn, Timestamp};

use crate::crc::crc32c;
use crate::frame::{FrameHeader, MAX_PAYLOAD, RecordType};

/// `wal.compression`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Compression {
    /// Store every payload as it is.
    None,
    /// Store a payload as an LZ4 block when that is smaller.
    #[default]
    Lz4,
}

/// Why a payload could not be stored or loaded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum CompressionError {
    /// The payload to store, or the stored payload, exceeds [`MAX_PAYLOAD`].
    #[error("payload of {0} bytes exceeds the maximum")]
    PayloadTooLarge(usize),
    /// A compressed payload is shorter than its 4-byte length prefix.
    #[error("compressed payload has no length prefix")]
    MissingLength,
    /// The recorded uncompressed length exceeds [`MAX_PAYLOAD`].
    #[error("uncompressed length {0} exceeds the maximum")]
    UncompressedTooLarge(u32),
    /// The LZ4 block is malformed or does not expand to exactly the recorded length.
    #[error("malformed LZ4 block")]
    Malformed,
}

/// The largest expansion of one LZ4 block: a block cannot expand to more than 255 times its size.
const LZ4_MAX_RATIO: usize = 255;

fn check_len(len: usize) -> Result<u32, CompressionError> {
    match u32::try_from(len) {
        Ok(len) if len <= MAX_PAYLOAD => Ok(len),
        _ => Err(CompressionError::PayloadTooLarge(len)),
    }
}

/// A payload in its stored form, ready for a frame: handlers build this off the committer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PreparedFrame {
    record_type: RecordType,
    compressed: bool,
    stored: Vec<u8>,
    payload_len: u32,
    payload_crc32c: u32,
}

impl PreparedFrame {
    /// Applies the compression rule to `payload` and computes the payload checksum.
    ///
    /// With [`Compression::Lz4`] the stored form is a `u32` uncompressed length followed by one LZ4 block,
    /// unless that is not smaller than the payload, in which case the payload is stored as it is.
    pub fn new(
        record_type: RecordType,
        payload: Vec<u8>,
        compression: Compression,
    ) -> Result<Self, CompressionError> {
        let raw_len = check_len(payload.len())?;
        let mut compressed = false;
        let mut stored = payload;
        if compression == Compression::Lz4 {
            let mut out = vec![0u8; 4 + lz4_flex::block::get_maximum_output_size(stored.len())];
            let (prefix, block) = out.split_at_mut(4);
            prefix.copy_from_slice(&raw_len.to_le_bytes());
            if let Ok(block_len) = lz4_flex::block::compress_into(&stored, block)
                && 4 + block_len < stored.len()
            {
                out.truncate(4 + block_len);
                stored = out;
                compressed = true;
            }
        }
        Ok(Self {
            record_type,
            compressed,
            payload_len: check_len(stored.len())?,
            payload_crc32c: crc32c(&stored),
            stored,
        })
    }

    /// The header of this frame once the committer has assigned its LSN and receive time.
    pub fn header(&self, lsn: Lsn, received_at: Timestamp) -> FrameHeader {
        FrameHeader {
            record_type: self.record_type,
            compressed: self.compressed,
            payload_len: self.payload_len,
            lsn,
            received_at,
            payload_crc32c: self.payload_crc32c,
        }
    }

    /// The stored payload bytes, which follow the header on disk.
    pub fn stored_payload(&self) -> &[u8] {
        &self.stored
    }

    /// Whether the stored payload is compressed.
    pub fn is_compressed(&self) -> bool {
        self.compressed
    }

    /// Bytes the frame occupies in a segment: the header and the stored payload.
    pub fn encoded_len(&self) -> usize {
        // At most 36 + 64 MiB, which cannot overflow.
        crate::frame::FRAME_HEADER_LEN + self.stored.len()
    }

    /// Appends the encoded header and the stored payload to `out`.
    pub fn write_to(&self, lsn: Lsn, received_at: Timestamp, out: &mut Vec<u8>) {
        // `payload_len` was checked against the cap at construction, so the header always encodes.
        if let Ok(header) = self.header(lsn, received_at).encode() {
            out.extend_from_slice(&header);
            out.extend_from_slice(&self.stored);
        }
    }
}

/// The payload of a frame from its stored bytes: borrowed when stored as it is, decompressed otherwise.
pub fn load_payload(compressed: bool, stored: &[u8]) -> Result<Cow<'_, [u8]>, CompressionError> {
    check_len(stored.len())?;
    if !compressed {
        return Ok(Cow::Borrowed(stored));
    }
    let Some((prefix, block)) = stored.split_first_chunk::<4>() else {
        return Err(CompressionError::MissingLength);
    };
    let recorded = u32::from_le_bytes(*prefix);
    if recorded > MAX_PAYLOAD {
        return Err(CompressionError::UncompressedTooLarge(recorded));
    }
    let len = recorded as usize;
    // Refuse impossible lengths before allocating the output buffer.
    if len > block.len().saturating_mul(LZ4_MAX_RATIO) {
        return Err(CompressionError::Malformed);
    }
    match lz4_flex::block::decompress(block, len) {
        Ok(payload) if payload.len() == len => Ok(Cow::Owned(payload)),
        _ => Err(CompressionError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn compressible_payload_is_stored_compressed() {
        let payload = vec![7u8; 4096];
        let frame =
            PreparedFrame::new(RecordType::Noop, payload.clone(), Compression::Lz4).unwrap();
        assert!(frame.is_compressed());
        assert!(frame.stored_payload().len() < payload.len());
        assert_eq!(&frame.stored_payload()[..4], &4096u32.to_le_bytes());
        assert_eq!(load_payload(true, frame.stored_payload()).unwrap(), payload);
        let plain =
            PreparedFrame::new(RecordType::Noop, payload.clone(), Compression::None).unwrap();
        assert!(!plain.is_compressed());
        assert_eq!(plain.stored_payload(), payload);
    }

    #[test]
    fn incompressible_payload_is_stored_as_is() {
        for payload in [vec![], vec![1], (0..=255u8).collect::<Vec<_>>()] {
            let frame =
                PreparedFrame::new(RecordType::Noop, payload.clone(), Compression::Lz4).unwrap();
            assert!(!frame.is_compressed());
            assert_eq!(frame.stored_payload(), payload);
        }
    }

    #[test]
    fn load_checks_both_sizes() {
        assert_eq!(
            load_payload(true, &[1, 2, 3]),
            Err(CompressionError::MissingLength)
        );
        let mut stored = (MAX_PAYLOAD + 1).to_le_bytes().to_vec();
        stored.extend_from_slice(&[0; 16]);
        assert_eq!(
            load_payload(true, &stored),
            Err(CompressionError::UncompressedTooLarge(MAX_PAYLOAD + 1))
        );
        // A length that the block cannot expand to is refused without allocating it.
        let mut stored = MAX_PAYLOAD.to_le_bytes().to_vec();
        stored.extend_from_slice(&[0; 16]);
        assert_eq!(
            load_payload(true, &stored),
            Err(CompressionError::Malformed)
        );
        // The recorded length must match the block exactly, in both directions.
        let frame = PreparedFrame::new(RecordType::Noop, vec![9; 1000], Compression::Lz4).unwrap();
        for wrong in [999u32, 1001] {
            let mut stored = frame.stored_payload().to_vec();
            stored[..4].copy_from_slice(&wrong.to_le_bytes());
            assert_eq!(
                load_payload(true, &stored),
                Err(CompressionError::Malformed)
            );
        }
    }

    #[test]
    fn oversized_payloads_are_refused() {
        let big = vec![0u8; MAX_PAYLOAD as usize + 1];
        assert_eq!(
            load_payload(false, &big),
            Err(CompressionError::PayloadTooLarge(big.len()))
        );
        assert_eq!(
            PreparedFrame::new(RecordType::Noop, big, Compression::None),
            Err(CompressionError::PayloadTooLarge(MAX_PAYLOAD as usize + 1))
        );
    }

    proptest! {
        #[test]
        fn stored_form_round_trips_and_never_grows(
            payload in prop_oneof![
                proptest::collection::vec(any::<u8>(), 0..2048),
                proptest::collection::vec(0u8..4, 0..2048),
            ],
            lz4 in any::<bool>(),
        ) {
            let compression = if lz4 { Compression::Lz4 } else { Compression::None };
            let frame = PreparedFrame::new(RecordType::Noop, payload.clone(), compression).unwrap();
            prop_assert!(frame.stored_payload().len() <= payload.len());
            prop_assert_eq!(frame.is_compressed(), frame.stored_payload().len() < payload.len());
            let loaded = load_payload(frame.is_compressed(), frame.stored_payload()).unwrap();
            prop_assert_eq!(loaded.as_ref(), payload.as_slice());
        }

        #[test]
        fn load_never_panics(stored in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = load_payload(true, &stored);
            let _ = load_payload(false, &stored);
        }
    }
}
