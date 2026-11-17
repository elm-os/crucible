//! Segment header and the `EPOCH` and `CHECKPOINT` directory entries (contract sections 4.1 and 4.2).

use crucible_records::{Lsn, Timestamp, WalEpoch};

use crate::crc::crc32c;

/// Segment magic, ASCII `CWS1`.
pub const SEGMENT_MAGIC: [u8; 4] = *b"CWS1";

/// Size of an encoded segment header. Frames start at this offset.
pub const SEGMENT_HEADER_LEN: usize = 64;

/// The only segment format version.
pub const SEGMENT_FORMAT_VERSION: u16 = 1;

/// Size of the `EPOCH` entry: the epoch and its CRC32C.
pub const EPOCH_ENTRY_LEN: usize = 20;

/// Size of the `CHECKPOINT` hint: three integers and their CRC32C.
pub const CHECKPOINT_HINT_LEN: usize = 28;

/// Why a segment header was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum SegmentHeaderError {
    /// The stored checksum does not match bytes 0 to 59. Every other variant implies a valid checksum.
    #[error("segment header checksum mismatch")]
    CrcMismatch,
    /// The magic is not `CWS1`.
    #[error("bad segment magic")]
    BadMagic,
    /// The format version is not 1.
    #[error("unsupported segment format version {0}")]
    UnsupportedVersion(u16),
    /// A reserved byte is not zero.
    #[error("reserved segment header bytes are not zero")]
    ReservedNotZero,
}

/// The 64-byte header at the start of every segment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentHeader {
    /// Epoch of the WAL directory; must equal the `EPOCH` entry.
    pub wal_epoch: WalEpoch,
    /// Segment number; starts at 1 and increases by 1.
    pub segment_no: u64,
    /// LSN of the first frame written to this segment.
    pub first_lsn: Lsn,
    /// Creation time.
    pub created_at: Timestamp,
}

impl SegmentHeader {
    /// Encodes the header and its checksum.
    pub fn encode(&self) -> [u8; SEGMENT_HEADER_LEN] {
        let mut out = [0u8; SEGMENT_HEADER_LEN];
        let (body, crc) = out.split_at_mut(60);
        body[0..4].copy_from_slice(&SEGMENT_MAGIC);
        body[4..6].copy_from_slice(&SEGMENT_FORMAT_VERSION.to_le_bytes());
        body[8..24].copy_from_slice(&self.wal_epoch.to_bytes());
        body[24..32].copy_from_slice(&self.segment_no.to_le_bytes());
        body[32..40].copy_from_slice(&self.first_lsn.get().to_le_bytes());
        body[40..48].copy_from_slice(&self.created_at.micros().to_le_bytes());
        crc.copy_from_slice(&crc32c(body).to_le_bytes());
        out
    }

    /// Decodes a header; the checksum is checked first.
    pub fn decode(bytes: &[u8; SEGMENT_HEADER_LEN]) -> Result<Self, SegmentHeaderError> {
        let (body, crc) = bytes.split_at(60);
        if crc32c(body).to_le_bytes() != crc {
            return Err(SegmentHeaderError::CrcMismatch);
        }
        let (magic, rest) = split(body);
        if magic != SEGMENT_MAGIC {
            return Err(SegmentHeaderError::BadMagic);
        }
        let (version, rest) = split(rest);
        let version = u16::from_le_bytes(version);
        if version != SEGMENT_FORMAT_VERSION {
            return Err(SegmentHeaderError::UnsupportedVersion(version));
        }
        let (reserved_a, rest) = split::<2>(rest);
        let (wal_epoch, rest) = split(rest);
        let (segment_no, rest) = split(rest);
        let (first_lsn, rest) = split(rest);
        let (created_at, reserved_b) = split(rest);
        if reserved_a != [0; 2] || reserved_b.iter().any(|&b| b != 0) {
            return Err(SegmentHeaderError::ReservedNotZero);
        }
        Ok(Self {
            wal_epoch: WalEpoch::from_bytes(wal_epoch),
            segment_no: u64::from_le_bytes(segment_no),
            first_lsn: Lsn::new(u64::from_le_bytes(first_lsn)),
            created_at: Timestamp::from_micros(i64::from_le_bytes(created_at)),
        })
    }
}

/// Splits `N` bytes off the front. A slice shorter than `N` yields zeros and an empty rest; callers pass
/// slices of a fixed, sufficient length.
fn split<const N: usize>(bytes: &[u8]) -> ([u8; N], &[u8]) {
    match bytes.split_first_chunk::<N>() {
        Some((head, rest)) => (*head, rest),
        None => ([0; N], &[]),
    }
}

/// Why an `EPOCH` entry or a `CHECKPOINT` hint was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum EntryError {
    /// The entry does not have its fixed length.
    #[error("entry is {0} bytes long")]
    WrongLength(usize),
    /// The stored checksum does not match the entry's bytes.
    #[error("entry checksum mismatch")]
    CrcMismatch,
}

/// Encodes the `EPOCH` entry: the epoch (16 bytes) and the CRC32C of those bytes.
pub fn encode_epoch(wal_epoch: WalEpoch) -> [u8; EPOCH_ENTRY_LEN] {
    let mut out = [0u8; EPOCH_ENTRY_LEN];
    let (body, crc) = out.split_at_mut(16);
    body.copy_from_slice(&wal_epoch.to_bytes());
    crc.copy_from_slice(&crc32c(body).to_le_bytes());
    out
}

/// Decodes the whole contents of the `EPOCH` entry.
pub fn decode_epoch(bytes: &[u8]) -> Result<WalEpoch, EntryError> {
    let body = checked_body::<16>(bytes, EPOCH_ENTRY_LEN)?;
    Ok(WalEpoch::from_bytes(body))
}

/// The advisory `CHECKPOINT` hint: where the last checkpoint frame is. WAL records are authoritative.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckpointHint {
    /// `batch_seq` of the last checkpoint.
    pub batch_seq: u64,
    /// Watermark of the last checkpoint.
    pub last_lsn: Lsn,
    /// Segment that holds the checkpoint frame.
    pub segment_no: u64,
}

impl CheckpointHint {
    /// Encodes the hint: `batch_seq`, `last_lsn`, `segment_no` and the CRC32C of those 24 bytes.
    pub fn encode(&self) -> [u8; CHECKPOINT_HINT_LEN] {
        let mut out = [0u8; CHECKPOINT_HINT_LEN];
        let (body, crc) = out.split_at_mut(24);
        body[0..8].copy_from_slice(&self.batch_seq.to_le_bytes());
        body[8..16].copy_from_slice(&self.last_lsn.get().to_le_bytes());
        body[16..24].copy_from_slice(&self.segment_no.to_le_bytes());
        crc.copy_from_slice(&crc32c(body).to_le_bytes());
        out
    }

    /// Decodes the whole contents of the `CHECKPOINT` entry.
    pub fn decode(bytes: &[u8]) -> Result<Self, EntryError> {
        let body = checked_body::<24>(bytes, CHECKPOINT_HINT_LEN)?;
        let (batch_seq, rest) = split(&body);
        let (last_lsn, rest) = split(rest);
        let (segment_no, _) = split(rest);
        Ok(Self {
            batch_seq: u64::from_le_bytes(batch_seq),
            last_lsn: Lsn::new(u64::from_le_bytes(last_lsn)),
            segment_no: u64::from_le_bytes(segment_no),
        })
    }
}

/// The first `N` bytes of an entry of exactly `len` bytes whose last four bytes are the CRC32C of the rest.
fn checked_body<const N: usize>(bytes: &[u8], len: usize) -> Result<[u8; N], EntryError> {
    if bytes.len() != len {
        return Err(EntryError::WrongLength(bytes.len()));
    }
    match bytes.split_first_chunk::<N>() {
        Some((body, crc)) if crc32c(body).to_le_bytes() == crc => Ok(*body),
        _ => Err(EntryError::CrcMismatch),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn header() -> impl Strategy<Value = SegmentHeader> {
        (any::<[u8; 16]>(), any::<u64>(), any::<u64>(), any::<i64>()).prop_map(|(e, n, l, t)| {
            SegmentHeader {
                wal_epoch: WalEpoch::from_bytes(e),
                segment_no: n,
                first_lsn: Lsn::new(l),
                created_at: Timestamp::from_micros(t),
            }
        })
    }

    #[test]
    fn segment_header_layout() {
        let header = SegmentHeader {
            wal_epoch: WalEpoch::from_bytes([0xAB; 16]),
            segment_no: 2,
            first_lsn: Lsn::new(0x0102),
            created_at: Timestamp::from_micros(7),
        };
        let bytes = header.encode();
        assert_eq!(&bytes[0..4], b"CWS1");
        assert_eq!(&bytes[4..8], &[1, 0, 0, 0]);
        assert_eq!(&bytes[8..24], &[0xAB; 16]);
        assert_eq!(&bytes[24..32], &2u64.to_le_bytes());
        assert_eq!(&bytes[32..40], &[2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&bytes[40..48], &7i64.to_le_bytes());
        assert_eq!(&bytes[48..60], &[0; 12]);
        assert_eq!(
            &bytes[60..64],
            &::crc32c::crc32c(&bytes[..60]).to_le_bytes()
        );
    }

    fn reseal(bytes: &mut [u8; SEGMENT_HEADER_LEN]) {
        let crc = ::crc32c::crc32c(&bytes[..60]).to_le_bytes();
        bytes[60..].copy_from_slice(&crc);
    }

    #[test]
    fn segment_header_typed_errors() {
        let good = SegmentHeader {
            wal_epoch: WalEpoch::from_bytes([1; 16]),
            segment_no: 1,
            first_lsn: Lsn::FIRST,
            created_at: Timestamp::EPOCH,
        }
        .encode();
        for (index, value, expected) in [
            (0, b'X', SegmentHeaderError::BadMagic),
            (4, 2, SegmentHeaderError::UnsupportedVersion(2)),
            (6, 1, SegmentHeaderError::ReservedNotZero),
            (59, 1, SegmentHeaderError::ReservedNotZero),
        ] {
            let mut bytes = good;
            bytes[index] = value;
            assert_eq!(
                SegmentHeader::decode(&bytes),
                Err(SegmentHeaderError::CrcMismatch)
            );
            reseal(&mut bytes);
            assert_eq!(SegmentHeader::decode(&bytes), Err(expected));
        }
    }

    #[test]
    fn entries_reject_wrong_lengths() {
        assert_eq!(decode_epoch(&[0; 19]), Err(EntryError::WrongLength(19)));
        assert_eq!(decode_epoch(&[0; 21]), Err(EntryError::WrongLength(21)));
        assert_eq!(CheckpointHint::decode(&[]), Err(EntryError::WrongLength(0)));
        assert_eq!(
            CheckpointHint::decode(&[0; 28]),
            Err(EntryError::CrcMismatch)
        );
    }

    proptest! {
        #[test]
        fn segment_header_round_trips(header in header()) {
            prop_assert_eq!(SegmentHeader::decode(&header.encode()), Ok(header));
        }

        #[test]
        fn segment_header_bit_flips_are_detected(header in header(), bit in 0usize..SEGMENT_HEADER_LEN * 8) {
            let mut bytes = header.encode();
            bytes[bit / 8] ^= 1 << (bit % 8);
            prop_assert_eq!(SegmentHeader::decode(&bytes), Err(SegmentHeaderError::CrcMismatch));
        }

        #[test]
        fn segment_header_decode_never_panics(bytes in any::<[u8; SEGMENT_HEADER_LEN]>()) {
            let _ = SegmentHeader::decode(&bytes);
        }

        #[test]
        fn epoch_round_trips_and_detects_flips(epoch in any::<[u8; 16]>(), bit in 0usize..EPOCH_ENTRY_LEN * 8) {
            let epoch = WalEpoch::from_bytes(epoch);
            let mut bytes = encode_epoch(epoch);
            prop_assert_eq!(decode_epoch(&bytes), Ok(epoch));
            bytes[bit / 8] ^= 1 << (bit % 8);
            prop_assert_eq!(decode_epoch(&bytes), Err(EntryError::CrcMismatch));
        }

        #[test]
        fn checkpoint_hint_round_trips_and_detects_flips(
            batch_seq in any::<u64>(), last_lsn in any::<u64>(), segment_no in any::<u64>(),
            bit in 0usize..CHECKPOINT_HINT_LEN * 8,
        ) {
            let hint = CheckpointHint { batch_seq, last_lsn: Lsn::new(last_lsn), segment_no };
            let mut bytes = hint.encode();
            prop_assert_eq!(CheckpointHint::decode(&bytes), Ok(hint));
            bytes[bit / 8] ^= 1 << (bit % 8);
            prop_assert_eq!(CheckpointHint::decode(&bytes), Err(EntryError::CrcMismatch));
        }

        #[test]
        fn entries_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..40)) {
            let _ = decode_epoch(&bytes);
            let _ = CheckpointHint::decode(&bytes);
        }
    }
}
