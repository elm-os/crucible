//! Fuzzes the frame iterator and, through it, the decompressor and the payload decoders.
//!
//! Random bytes almost never carry a valid CRC32C, so the first input byte selects how much of each frame
//! is repaired before the scan: bit 0 recomputes both checksums, bit 1 also clamps each declared payload
//! length to the bytes that remain (so frames chain), bit 2 also writes the magic and the header version.
//! With no bit set the bytes are scanned as they are.
#![no_main]

use crucible_wal_codec::{
    FRAME_HEADER_LEN, FRAME_MAGIC, FrameIter, MAX_PAYLOAD, Record, Scan, SegmentEnd, scan_frame,
};
use libfuzzer_sys::fuzz_target;

fn repair(segment: &mut [u8], mode: u8) {
    let mut offset = 0;
    while let Some(frame) = segment
        .get_mut(offset..)
        .filter(|rest| rest.len() >= FRAME_HEADER_LEN)
    {
        let (header, rest) = frame.split_at_mut(FRAME_HEADER_LEN);
        let declared = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        let payload_len = declared.min(rest.len());
        if mode & 2 != 0 {
            header[8..12].copy_from_slice(&(payload_len as u32).to_le_bytes());
        }
        if mode & 4 != 0 {
            header[0..4].copy_from_slice(&FRAME_MAGIC);
            header[4] = 1;
        }
        header[28..32].copy_from_slice(&crc32c::crc32c(&rest[..payload_len]).to_le_bytes());
        let crc = crc32c::crc32c(&header[..32]);
        header[32..36].copy_from_slice(&crc.to_le_bytes());
        offset += FRAME_HEADER_LEN + payload_len;
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let mut segment = rest.to_vec();
    if mode & 1 != 0 {
        repair(&mut segment, mode);
    }

    let mut iter = FrameIter::new(&segment, 0);
    let mut expected_offset = 0;
    let mut steps = 0;
    while let Some(item) = iter.next() {
        steps += 1;
        assert!(steps <= segment.len() + 1, "the iterator must terminate");
        match item {
            Ok(frame) => {
                assert_eq!(frame.offset(), expected_offset);
                assert!(frame.end_offset() >= frame.offset() + FRAME_HEADER_LEN);
                assert!(frame.end_offset() <= segment.len());
                assert_eq!(iter.offset(), frame.end_offset());
                assert_eq!(iter.last_lsn(), Some(frame.lsn()));
                assert!(frame.header().payload_len <= MAX_PAYLOAD);
                assert!(frame.header().payload_matches(frame.stored_payload()));
                assert_eq!(scan_frame(&segment, frame.offset()), Scan::Frame(frame));
                expected_offset = frame.end_offset();
                if let Ok(payload) = frame.payload() {
                    if let Ok(record) = Record::decode(frame.record_type(), &payload) {
                        assert_eq!(record.record_type(), frame.record_type());
                        let encoded = record.encode().expect("a decoded record encodes");
                        assert_eq!(encoded, payload.as_ref());
                    }
                }
            }
            Err(corruption) => {
                assert_eq!(corruption.offset, expected_offset);
                let end = SegmentEnd::Corrupt {
                    offset: corruption.offset,
                    error: corruption.error,
                };
                assert_eq!(iter.end(), Some(end));
            }
        }
    }
    let end = iter
        .end()
        .expect("an ended iterator knows how the data ends");
    assert_eq!(end.offset(), expected_offset);
    assert_eq!(iter.offset(), expected_offset);
    assert!(iter.next().is_none());
    assert_eq!(scan_frame(&segment, expected_offset), Scan::End(end));
});
