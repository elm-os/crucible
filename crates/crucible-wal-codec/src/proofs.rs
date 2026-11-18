//! Kani proofs of the frame codec obligations (contract section 4.9).
//!
//! Under Kani the checksum is the bitwise CRC32C model in `crc.rs` (the product routine uses tables and CPU
//! intrinsics). A property test checks that the model equals the product routine.
//!
//! Bounds: the header proofs cover every 36-byte input. The single-bit-flip obligation is split into four
//! proofs, described on each. The iterator proofs cover segments of at most 40 bytes.

use crucible_records::{Lsn, Timestamp};

use crate::crc::crc32c_update;
use crate::frame::{
    FRAME_HEADER_LEN, FrameHeader, FrameHeaderError, MAX_PAYLOAD, RecordType, frame_end,
};
use crate::iter::{FrameIter, Scan, scan_frame};

fn any_valid_header() -> FrameHeader {
    let code: u8 = kani::any();
    let record_type = RecordType::from_code(code);
    kani::assume(record_type.is_some());
    let payload_len: u32 = kani::any();
    kani::assume(payload_len <= MAX_PAYLOAD);
    FrameHeader {
        record_type: record_type.unwrap(),
        compressed: kani::any(),
        payload_len,
        lsn: Lsn::new(kani::any()),
        received_at: Timestamp::from_micros(kani::any()),
        payload_crc32c: kani::any(),
    }
}

/// Obligation 1: decoding an arbitrary 36-byte array never panics.
#[kani::proof]
#[kani::unwind(33)]
fn decode_arbitrary_header_never_panics() {
    let bytes: [u8; FRAME_HEADER_LEN] = kani::any();
    let _ = FrameHeader::decode(&bytes);
}

/// Obligation 2: encode then decode is the identity for every valid header.
#[kani::proof]
#[kani::unwind(33)]
fn encode_then_decode_is_identity() {
    let header = any_valid_header();
    let bytes = header.encode().unwrap();
    assert_eq!(FrameHeader::decode(&bytes), Ok(header));
}

/// Obligation 3, first half: a decoded `payload_len` never exceeds `MAX_PAYLOAD`.
#[kani::proof]
#[kani::unwind(33)]
fn decoded_payload_len_is_bounded() {
    let bytes: [u8; FRAME_HEADER_LEN] = kani::any();
    if let Ok(header) = FrameHeader::decode(&bytes) {
        assert!(header.payload_len <= MAX_PAYLOAD);
    }
}

/// Obligation 3, second half: the offset arithmetic never overflows, for every offset, payload length and
/// segment length, and a frame that fits ends above its start and inside the segment.
#[kani::proof]
fn frame_offset_arithmetic_never_overflows() {
    let offset: usize = kani::any();
    let payload_len: u32 = kani::any();
    let segment_len: usize = kani::any();
    if let Some(end) = frame_end(offset, payload_len, segment_len) {
        assert!(end > offset);
        assert!(end <= segment_len);
        assert!(end - offset == FRAME_HEADER_LEN + payload_len as usize);
        assert!(payload_len <= MAX_PAYLOAD);
    }
}

/// Obligation 4, wiring: for one fixed header, flipping any one of its 288 bits makes `decode` report a
/// checksum mismatch. This runs the real encoder and decoder, so it shows that the check covers bytes 0 to 31
/// and the stored checksum. The two proofs below extend the result from this header to every header.
#[kani::proof]
#[kani::unwind(33)]
fn single_bit_flip_fails_the_crc_check() {
    let header = FrameHeader {
        record_type: RecordType::IngestRequest,
        compressed: false,
        payload_len: 0,
        lsn: Lsn::FIRST,
        received_at: Timestamp::EPOCH,
        payload_crc32c: 0,
    };
    let mut bytes = header.encode().unwrap();
    let bit: usize = kani::any();
    kani::assume(bit < FRAME_HEADER_LEN * 8);
    bytes[bit / 8] ^= 1 << (bit % 8);
    assert_eq!(
        FrameHeader::decode(&bytes),
        Err(FrameHeaderError::HeaderCrcMismatch)
    );
}

/// Obligation 4, stored checksum: for every valid header, flipping any bit of the stored checksum (bytes 32
/// to 35) fails the check.
#[kani::proof]
#[kani::unwind(33)]
fn checksum_field_bit_flip_fails_the_crc_check() {
    let mut bytes = any_valid_header().encode().unwrap();
    let bit: usize = kani::any();
    kani::assume(bit >= 32 * 8 && bit < FRAME_HEADER_LEN * 8);
    bytes[bit / 8] ^= 1 << (bit % 8);
    assert_eq!(
        FrameHeader::decode(&bytes),
        Err(FrameHeaderError::HeaderCrcMismatch)
    );
}

/// Obligation 4, linearity: one byte step of the checksum model is linear over XOR, for every pair of states
/// and bytes. By induction over the bytes, the checksums of two equal-length inputs `m` and `m ^ e` differ by
/// exactly the zero-initialised state reached on `e`, whatever `m` is (the initial value and the final XOR
/// cancel).
#[kani::proof]
#[kani::unwind(9)]
fn crc_update_is_linear() {
    let (a, b): (u32, u32) = (kani::any(), kani::any());
    let (x, y): (u8, u8) = (kani::any(), kani::any());
    assert_eq!(
        crc32c_update(a ^ b, x ^ y),
        crc32c_update(a, x) ^ crc32c_update(b, y)
    );
}

/// Obligation 4, bounded model: for an input of 32 bytes (the checksummed part of a header) and each of its
/// 256 bit positions, the zero-initialised model state reached on the one-bit difference is not zero. With
/// linearity this means that a flipped bit changes the checksum of every 32-byte input, so no single-bit
/// flip in bytes 0 to 31 of any header passes the check.
#[kani::proof]
#[kani::unwind(33)]
fn single_bit_difference_never_cancels() {
    let bit: usize = kani::any();
    kani::assume(bit < 32 * 8);
    let mut difference = [0u8; 32];
    difference[bit / 8] = 1 << (bit % 8);
    let mut state = 0u32;
    for byte in difference {
        state = crc32c_update(state, byte);
    }
    assert!(state != 0);
}

/// Bound of the iterator proofs: a segment of at most 40 arbitrary bytes, which holds one frame header and
/// up to four payload bytes, scanned from an arbitrary offset.
const SEGMENT_BOUND: usize = FRAME_HEADER_LEN + 4;

/// Obligation 5 for the scan kernel: a scan yields a frame that ends above the scanned offset and inside the
/// segment, or it ends the data at the scanned offset.
#[kani::proof]
#[kani::unwind(33)]
fn scan_advances_or_ends() {
    let bytes: [u8; SEGMENT_BOUND] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= SEGMENT_BOUND);
    let segment = &bytes[..len];
    let offset: usize = kani::any();
    match scan_frame(segment, offset) {
        Scan::Frame(frame) => {
            assert!(frame.offset() == offset);
            assert!(frame.end_offset() > offset);
            assert!(frame.end_offset() <= len);
        }
        Scan::End(end) => assert!(end.offset() == if offset < len { offset } else { len }),
    }
}

/// Obligation 5 for the iterator: every call to `next` moves the offset forward, or ends the iteration for
/// good (an error is yielded at most once, on the call that ends it).
#[kani::proof]
#[kani::unwind(33)]
fn iterator_always_advances_or_errors() {
    let bytes: [u8; SEGMENT_BOUND] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= SEGMENT_BOUND);
    let mut iter = FrameIter::new(&bytes[..len], kani::any());
    let before = iter.offset();
    match iter.next() {
        Some(Ok(_)) => {
            assert!(iter.offset() > before);
            assert!(iter.offset() <= len);
        }
        Some(Err(_)) | None => {
            assert!(iter.offset() == before);
            assert!(iter.end().is_some());
            assert!(iter.next().is_none());
        }
    }
}
