//! Kani proofs of the cursor codec obligation (contract 4.9): decoding arbitrary input never panics, and the
//! codec round-trips.
//!
//! Bounds: the decoding proofs cover every input of up to one byte more than the longest payload; the
//! round-trip proof covers every valid cursor of every kind, ids of every length up to `MAX_ID_LEN` included.
//! Under Kani `MAX_ID_LEN` is 26 bytes instead of 512, which keeps the longest payload at 104 bytes: with the
//! real cap the solver needs more than 2 GiB. Every id length takes the same code path; ids up to the real cap
//! are covered by the property tests and the fuzz target. Comparing slices of those lengths loops at most 105
//! times, hence the unwind bound of 106; a longer loop fails the proof instead of being cut short. The MAC and
//! base64 layers are covered by property tests and the fuzz target, not here.

use crucible_records::{Lsn, ProjectId, SpanId, TimeWindow, Timestamp, TraceId, WalEpoch};

use crate::payload::{
    Cursor, CursorId, FilterHash, IdList, MAX_ID_LEN, MAX_PAYLOAD_LEN, Position, Sort,
};

fn any_time() -> Timestamp {
    Timestamp::from_micros(kani::any())
}

fn any_window() -> TimeWindow {
    let (from, to) = (any_time(), any_time());
    kani::assume(from < to);
    TimeWindow::new(from, to).unwrap()
}

fn any_id() -> CursorId {
    let bytes: [u8; MAX_ID_LEN] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= MAX_ID_LEN);
    CursorId::new(&bytes[..len]).unwrap()
}

fn any_position() -> Position {
    match kani::any::<u8>() % 5 {
        0 => Position::TracePage {
            window: any_window(),
            as_of: any_time(),
            last: (any_time(), kani::any(), TraceId::from_bytes(kani::any())),
        },
        1 => Position::TraceHead {
            window: any_window(),
            as_of: any_time(),
            wal_epoch: WalEpoch::from_bytes(kani::any()),
            lsn: Lsn::new(kani::any()),
        },
        2 => Position::ChildPage {
            trace_id: TraceId::from_bytes(kani::any()),
            parent: SpanId::from_bytes(kani::any()),
            last: (any_time(), SpanId::from_bytes(kani::any())),
        },
        3 => Position::SpanPage {
            window: any_window(),
            as_of: any_time(),
            last: (any_time(), SpanId::from_bytes(kani::any())),
        },
        _ => Position::IdPage {
            list: match kani::any::<u8>() % 3 {
                0 => IdList::Sessions,
                1 => IdList::Users,
                _ => IdList::UserSessions,
            },
            window: any_window(),
            last: (any_time(), any_id()),
        },
    }
}

fn any_cursor() -> Cursor {
    Cursor {
        project_id: ProjectId::from_bytes(kani::any()),
        filter: FilterHash::from_bytes(kani::any()),
        sort: if kani::any() {
            Sort::NewestFirst
        } else {
            Sort::OldestFirst
        },
        position: any_position(),
        issued_at: any_time(),
        expires_at: any_time(),
    }
}

/// Decoding any input of up to `MAX_PAYLOAD_LEN + 1` bytes never panics.
#[kani::proof]
#[kani::unwind(106)]
fn decode_arbitrary_input_never_panics() {
    let bytes: [u8; MAX_PAYLOAD_LEN + 1] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= bytes.len());
    let _ = Cursor::decode(&bytes[..len]);
}

/// Encode then decode is the identity for every valid cursor.
#[kani::proof]
#[kani::unwind(106)]
fn encode_then_decode_is_identity() {
    let cursor = any_cursor();
    assert_eq!(Cursor::decode(cursor.encode().as_bytes()), Ok(cursor));
}

/// Whatever decodes re-encodes to the same bytes, so a payload has exactly one encoding.
#[kani::proof]
#[kani::unwind(106)]
fn decode_then_encode_is_identity() {
    let bytes: [u8; MAX_PAYLOAD_LEN + 1] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= bytes.len());
    if let Ok(cursor) = Cursor::decode(&bytes[..len]) {
        assert_eq!(cursor.encode().as_bytes(), &bytes[..len]);
    }
}
