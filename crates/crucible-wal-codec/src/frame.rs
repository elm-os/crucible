//! Frame header (contract section 4.3) and the offset arithmetic of frames inside a segment.

use crucible_records::{Lsn, Timestamp};

use crate::crc::crc32c;

/// Frame magic, ASCII `CWF1`.
pub const FRAME_MAGIC: [u8; 4] = *b"CWF1";

/// Size of an encoded frame header.
pub const FRAME_HEADER_LEN: usize = 36;

/// The only frame header version.
pub const FRAME_HEADER_VERSION: u8 = 1;

/// Largest stored payload, and largest uncompressed payload, of one frame (64 MiB).
pub const MAX_PAYLOAD: u32 = 67_108_864;

const FLAG_COMPRESSED: u16 = 1;

/// Record type code of a frame (contract section 4.4). Code 0 is invalid and codes above 8 are unknown.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum RecordType {
    /// One accepted OTLP export request.
    IngestRequest = 1,
    /// Reserved; decoders accept the code and skip the frame.
    NativeBatch = 2,
    /// Facts written through the API.
    FactBatch = 3,
    /// A deletion request.
    Tombstone = 4,
    /// The durable boundaries of a batch.
    BatchSeal = 5,
    /// The low watermark.
    Checkpoint = 6,
    /// Pairs of a batch that were moved to quarantine.
    Quarantine = 7,
    /// Padding.
    Noop = 8,
}

impl RecordType {
    /// Every record type in code order.
    pub const ALL: [Self; 8] = [
        Self::IngestRequest,
        Self::NativeBatch,
        Self::FactBatch,
        Self::Tombstone,
        Self::BatchSeal,
        Self::Checkpoint,
        Self::Quarantine,
        Self::Noop,
    ];

    /// The stored code.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The record type with this code.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::IngestRequest),
            2 => Some(Self::NativeBatch),
            3 => Some(Self::FactBatch),
            4 => Some(Self::Tombstone),
            5 => Some(Self::BatchSeal),
            6 => Some(Self::Checkpoint),
            7 => Some(Self::Quarantine),
            8 => Some(Self::Noop),
            _ => None,
        }
    }
}

/// Why a frame header was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum FrameHeaderError {
    /// The stored header checksum does not match bytes 0 to 31. Every other variant implies a valid checksum.
    #[error("frame header checksum mismatch")]
    HeaderCrcMismatch,
    /// The magic is not `CWF1`.
    #[error("bad frame magic")]
    BadMagic,
    /// The header version is not 1.
    #[error("unsupported frame header version {0}")]
    UnsupportedVersion(u8),
    /// The record type code is 0 or above 8.
    #[error("unknown record type {0}")]
    UnknownRecordType(u8),
    /// A flag bit other than bit 0 is set.
    #[error("reserved frame flag set in {0:#06x}")]
    ReservedFlag(u16),
    /// The stored payload length exceeds [`MAX_PAYLOAD`].
    #[error("payload length {0} exceeds the maximum")]
    PayloadTooLarge(u32),
}

/// A decoded frame header. `header_crc32c` is computed on encode and checked on decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameHeader {
    /// Record type of the payload.
    pub record_type: RecordType,
    /// Flag bit 0: the stored payload is an uncompressed length followed by one LZ4 block.
    pub compressed: bool,
    /// Stored payload bytes; at most [`MAX_PAYLOAD`].
    pub payload_len: u32,
    /// Log sequence number of the frame.
    pub lsn: Lsn,
    /// Wall time read by the committer when it assigned the LSN.
    pub received_at: Timestamp,
    /// CRC32C of the stored payload bytes.
    pub payload_crc32c: u32,
}

impl FrameHeader {
    /// Encodes the header and its checksum. Fails only when `payload_len` exceeds [`MAX_PAYLOAD`].
    pub fn encode(&self) -> Result<[u8; FRAME_HEADER_LEN], FrameHeaderError> {
        if self.payload_len > MAX_PAYLOAD {
            return Err(FrameHeaderError::PayloadTooLarge(self.payload_len));
        }
        let flags = if self.compressed { FLAG_COMPRESSED } else { 0 };
        let mut out = [0u8; FRAME_HEADER_LEN];
        let (body, crc) = out.split_at_mut(32);
        body[0..4].copy_from_slice(&FRAME_MAGIC);
        body[4] = FRAME_HEADER_VERSION;
        body[5] = self.record_type.code();
        body[6..8].copy_from_slice(&flags.to_le_bytes());
        body[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        body[12..20].copy_from_slice(&self.lsn.get().to_le_bytes());
        body[20..28].copy_from_slice(&self.received_at.micros().to_le_bytes());
        body[28..32].copy_from_slice(&self.payload_crc32c.to_le_bytes());
        crc.copy_from_slice(&crc32c(body).to_le_bytes());
        Ok(out)
    }

    /// Decodes a header. The checksum is checked first, so every error other than
    /// [`FrameHeaderError::HeaderCrcMismatch`] describes a header whose checksum is valid.
    pub fn decode(bytes: &[u8; FRAME_HEADER_LEN]) -> Result<Self, FrameHeaderError> {
        let [
            m0,
            m1,
            m2,
            m3,
            version,
            code,
            f0,
            f1,
            p0,
            p1,
            p2,
            p3,
            l0,
            l1,
            l2,
            l3,
            l4,
            l5,
            l6,
            l7,
            t0,
            t1,
            t2,
            t3,
            t4,
            t5,
            t6,
            t7,
            c0,
            c1,
            c2,
            c3,
            h0,
            h1,
            h2,
            h3,
        ] = *bytes;
        let (body, _) = bytes.split_at(32);
        if crc32c(body) != u32::from_le_bytes([h0, h1, h2, h3]) {
            return Err(FrameHeaderError::HeaderCrcMismatch);
        }
        if [m0, m1, m2, m3] != FRAME_MAGIC {
            return Err(FrameHeaderError::BadMagic);
        }
        if version != FRAME_HEADER_VERSION {
            return Err(FrameHeaderError::UnsupportedVersion(version));
        }
        let Some(record_type) = RecordType::from_code(code) else {
            return Err(FrameHeaderError::UnknownRecordType(code));
        };
        let flags = u16::from_le_bytes([f0, f1]);
        if flags & !FLAG_COMPRESSED != 0 {
            return Err(FrameHeaderError::ReservedFlag(flags));
        }
        let payload_len = u32::from_le_bytes([p0, p1, p2, p3]);
        if payload_len > MAX_PAYLOAD {
            return Err(FrameHeaderError::PayloadTooLarge(payload_len));
        }
        Ok(Self {
            record_type,
            compressed: flags & FLAG_COMPRESSED != 0,
            payload_len,
            lsn: Lsn::new(u64::from_le_bytes([l0, l1, l2, l3, l4, l5, l6, l7])),
            received_at: Timestamp::from_micros(i64::from_le_bytes([
                t0, t1, t2, t3, t4, t5, t6, t7,
            ])),
            payload_crc32c: u32::from_le_bytes([c0, c1, c2, c3]),
        })
    }

    /// Whether `stored` is this frame's payload: the length and the CRC32C both match.
    pub fn payload_matches(&self, stored: &[u8]) -> bool {
        u32::try_from(stored.len()) == Ok(self.payload_len) && crc32c(stored) == self.payload_crc32c
    }
}

/// The stored payload length and payload checksum fields of a header whose checksum is valid, read without
/// interpreting the other fields. The scanner uses them to tell a torn frame from a corrupt one.
pub(crate) fn raw_payload_fields(bytes: &[u8; FRAME_HEADER_LEN]) -> (u32, u32) {
    let [.., c0, c1, c2, c3, _, _, _, _] = *bytes;
    let [_, _, _, _, _, _, _, _, p0, p1, p2, p3, ..] = *bytes;
    (
        u32::from_le_bytes([p0, p1, p2, p3]),
        u32::from_le_bytes([c0, c1, c2, c3]),
    )
}

/// Offset one past a frame that starts at `offset` and stores `payload_len` payload bytes, or `None` when the
/// frame would not fit in a segment of `segment_len` bytes, when `payload_len` exceeds [`MAX_PAYLOAD`], or
/// when the sum would overflow. A `Some` result is always above `offset` and at most `segment_len`.
pub const fn frame_end(offset: usize, payload_len: u32, segment_len: usize) -> Option<usize> {
    if payload_len > MAX_PAYLOAD {
        return None;
    }
    let Some(payload_start) = offset.checked_add(FRAME_HEADER_LEN) else {
        return None;
    };
    // A `u32` always fits `usize` on the 32-bit and 64-bit targets this crate builds for.
    let Some(end) = payload_start.checked_add(payload_len as usize) else {
        return None;
    };
    if end <= segment_len { Some(end) } else { None }
}
