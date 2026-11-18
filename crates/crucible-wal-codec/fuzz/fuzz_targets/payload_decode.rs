//! Fuzzes the payload decoders, the decompressor and the fixed-size entry decoders with arbitrary bytes.
//!
//! The first byte selects the decoder. Whatever a payload decoder accepts must re-encode to the same bytes.
#![no_main]

use crucible_wal_codec::{
    CheckpointHint, MAX_PAYLOAD, Record, RecordType, SegmentHeader, decode_epoch, load_payload,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };
    match selector % 12 {
        code @ 1..=8 => {
            let record_type = RecordType::from_code(code).expect("codes 1 to 8 are record types");
            if let Ok(record) = Record::decode(record_type, input) {
                assert_eq!(record.record_type(), record_type);
                assert_eq!(record.encode().expect("a decoded record encodes"), input);
            }
        }
        9 => {
            if let Ok(payload) = load_payload(true, input) {
                assert!(payload.len() <= MAX_PAYLOAD as usize);
                let recorded = u32::from_le_bytes(*input.first_chunk().expect("length prefix"));
                assert_eq!(payload.len(), recorded as usize);
            }
        }
        10 => {
            if let Some(bytes) = input.first_chunk() {
                if let Ok(header) = SegmentHeader::decode(bytes) {
                    assert_eq!(&header.encode(), bytes);
                }
            }
        }
        _ => {
            let _ = decode_epoch(input);
            if let Ok(hint) = CheckpointHint::decode(input) {
                assert_eq!(hint.encode().as_slice(), input);
            }
        }
    }
});
