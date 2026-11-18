//! Fuzzes cursor decoding with arbitrary bytes.
//!
//! The first byte selects the layer. Even: the payload codec. Whatever decodes must re-encode to the same
//! bytes, and sealing it and opening it again with its own binding must give it back, or `Expired` once its
//! expiry has passed. Odd: the text form. Opening arbitrary text must not panic, and text that opens must be
//! exactly what sealing the cursor gives.
#![no_main]

use crucible_cursor::{
    Binding, Cursor, CursorError, FilterHash, KeyRing, Kid, Kind, Route, SigningKey, Sort,
};
use crucible_records::{ProjectId, TimeWindow, Timestamp};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };
    let kid = Kid::new("k1").expect("a valid key id");
    let ring = KeyRing::new(SigningKey::new(kid, std::array::from_fn(|i| i as u8)));
    let now = Timestamp::from_micros(1_794_907_800_000_000);
    if selector % 2 == 0 {
        if let Ok(cursor) = Cursor::decode(input) {
            assert_eq!(cursor.encode().as_bytes(), input);
            let opened = ring.open(&ring.seal(&cursor), &cursor.binding(), now);
            if now >= cursor.expires_at {
                assert_eq!(opened, Err(CursorError::Expired));
            } else {
                assert_eq!(opened, Ok(cursor));
            }
        }
    } else if let Ok(text) = std::str::from_utf8(input) {
        let binding = Binding {
            project_id: ProjectId::from_bytes([1; 16]),
            route: Route::TraceList,
            kind: Kind::Page,
            filter: FilterHash::of(b""),
            sort: Sort::NewestFirst,
            window: TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE).ok(),
        };
        if let Ok(cursor) = ring.open(text, &binding, now) {
            assert_eq!(ring.seal(&cursor), text);
        }
    }
});
