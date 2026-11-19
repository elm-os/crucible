//! The cursor codec, the text form and the key ring (contracts 11.2 and 12.4).

// Test code throughout: lets the helpers use `unwrap` under the workspace lints.
#![cfg(test)]

use crucible_cursor::{
    Binding, CURSOR_TTL_MICROS, Cursor, CursorError, DecodeError, FilterHash, InvalidKid, KeyRing,
    KeyRingError, KeyState, Kid, Kind, MAX_PAYLOAD_LEN, MAX_TEXT_LEN, Position, Route, SigningKey,
    Sort,
};
use crucible_records::{Lsn, ProjectId, SpanId, TimeWindow, Timestamp, TraceId, WalEpoch};
use proptest::collection::vec;
use proptest::prelude::*;

fn kid(text: &str) -> Kid {
    Kid::new(text).unwrap()
}

fn key(id: &str, secret: [u8; 32]) -> SigningKey {
    SigningKey::new(kid(id), secret)
}

fn time() -> impl Strategy<Value = Timestamp> {
    (0..Timestamp::MAX_EXCLUSIVE.micros()).prop_map(Timestamp::from_micros)
}

fn any_time() -> impl Strategy<Value = Timestamp> {
    any::<i64>().prop_map(Timestamp::from_micros)
}

fn window() -> impl Strategy<Value = TimeWindow> {
    (any::<i64>(), any::<i64>())
        .prop_filter("from < to", |(from, to)| from < to)
        .prop_map(|(from, to)| {
            TimeWindow::new(Timestamp::from_micros(from), Timestamp::from_micros(to)).unwrap()
        })
}

fn position() -> impl Strategy<Value = Position> {
    prop_oneof![
        (
            window(),
            any_time(),
            any_time(),
            any::<u32>(),
            any::<[u8; 16]>()
        )
            .prop_map(|(window, as_of, start, hash, trace)| Position::TracePage {
                window,
                as_of,
                last: (start, hash, TraceId::from_bytes(trace)),
            }),
        (window(), any_time(), any::<[u8; 16]>(), any::<u64>()).prop_map(
            |(window, as_of, epoch, lsn)| Position::TraceHead {
                window,
                as_of,
                wal_epoch: WalEpoch::from_bytes(epoch),
                lsn: Lsn::new(lsn),
            }
        ),
        (
            any::<[u8; 16]>(),
            any::<[u8; 8]>(),
            any_time(),
            any::<[u8; 8]>()
        )
            .prop_map(|(trace, parent, start, span)| Position::ChildPage {
                trace_id: TraceId::from_bytes(trace),
                parent: SpanId::from_bytes(parent),
                last: (start, SpanId::from_bytes(span)),
            }),
        (
            window(),
            any_time(),
            any_time(),
            any::<u32>(),
            any::<[u8; 8]>()
        )
            .prop_map(|(window, as_of, minute, hash, span)| Position::SearchPage {
                window,
                as_of,
                last: (minute, hash, SpanId::from_bytes(span)),
            }),
        (window(), any_time(), any_time(), any::<[u8; 8]>()).prop_map(
            |(window, as_of, start, span)| Position::McpOperationPage {
                window,
                as_of,
                last: (start, SpanId::from_bytes(span)),
            }
        ),
    ]
}

fn sort() -> impl Strategy<Value = Sort> {
    prop_oneof![Just(Sort::NewestFirst), Just(Sort::OldestFirst)]
}

/// A cursor as the API issues it: a valid issue time and an expiry a day later.
fn cursor() -> impl Strategy<Value = Cursor> {
    (
        any::<[u8; 16]>(),
        any::<[u8; 16]>(),
        sort(),
        position(),
        time(),
    )
        .prop_map(|(project, filter, sort, position, issued_at)| {
            Cursor::new(
                ProjectId::from_bytes(project),
                FilterHash::from_bytes(filter),
                sort,
                position,
                issued_at,
            )
        })
}

/// Any value the codec can carry, times included.
fn any_cursor() -> impl Strategy<Value = Cursor> {
    (cursor(), any_time(), any_time()).prop_map(|(cursor, issued_at, expires_at)| Cursor {
        issued_at,
        expires_at,
        ..cursor
    })
}

/// A window other than `window`, which may be none.
fn other_window(window: Option<TimeWindow>) -> Option<TimeWindow> {
    let all = TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE).ok();
    if window == all {
        TimeWindow::new(Timestamp::EPOCH, Timestamp::from_micros(1)).ok()
    } else {
        all
    }
}

proptest! {
    #[test]
    fn the_codec_round_trips(cursor in any_cursor()) {
        let payload = cursor.encode();
        prop_assert!(payload.as_bytes().len() <= MAX_PAYLOAD_LEN);
        prop_assert_eq!(Cursor::decode(payload.as_bytes()), Ok(cursor));
    }

    #[test]
    fn arbitrary_payloads_never_panic_and_accepted_ones_re_encode(
        bytes in vec(any::<u8>(), 0..=MAX_PAYLOAD_LEN + 8),
    ) {
        if let Ok(cursor) = Cursor::decode(&bytes) {
            let payload = cursor.encode();
            prop_assert_eq!(payload.as_bytes(), &bytes[..]);
        }
    }

    #[test]
    fn arbitrary_text_never_panics_and_never_opens(text in ".{0,300}", secret: [u8; 32]) {
        let ring = KeyRing::new(key("a", secret));
        let binding = Cursor::new(
            ProjectId::from_bytes([1; 16]),
            FilterHash::of(b""),
            Sort::NewestFirst,
            Position::ChildPage {
                trace_id: TraceId::from_bytes([2; 16]),
                parent: SpanId::from_bytes([3; 8]),
                last: (Timestamp::EPOCH, SpanId::from_bytes([4; 8])),
            },
            Timestamp::EPOCH,
        )
        .binding();
        prop_assert!(ring.open(&text, &binding, Timestamp::EPOCH).is_err());
    }

    #[test]
    fn sealed_cursors_open_until_they_expire(
        cursor in cursor(),
        secret: [u8; 32],
        age in 0..CURSOR_TTL_MICROS,
    ) {
        let ring = KeyRing::new(key("a", secret));
        let text = ring.seal(&cursor);
        prop_assert!(text.len() <= MAX_TEXT_LEN, "{} characters", text.len());
        prop_assert!(text.starts_with("v1.a."));
        let binding = cursor.binding();
        let now = cursor.issued_at.saturating_add_micros(age);
        prop_assert_eq!(ring.open(&text, &binding, now), Ok(cursor));
        prop_assert_eq!(
            ring.open(&text, &binding, cursor.expires_at),
            Err(CursorError::Expired)
        );
        prop_assert_eq!(cursor.expires_at.micros() - cursor.issued_at.micros(), CURSOR_TTL_MICROS);
    }

    #[test]
    fn every_single_byte_change_is_refused(
        cursor in cursor(),
        secret: [u8; 32],
        replacement in 0u8..128,
    ) {
        let ring = KeyRing::new(key("a", secret));
        let text = ring.seal(&cursor);
        let (binding, now) = (cursor.binding(), cursor.issued_at);
        for at in 0..text.len() {
            let mut bytes = text.clone().into_bytes();
            bytes[at] = if bytes[at] == replacement { (replacement + 1) % 128 } else { replacement };
            let changed = String::from_utf8(bytes).unwrap();
            prop_assert!(ring.open(&changed, &binding, now).is_err(), "{} opened", changed);
        }
    }

    #[test]
    fn a_cursor_opens_only_for_its_own_binding(
        cursor in cursor(),
        secret: [u8; 32],
        project: [u8; 16],
        filter: [u8; 16],
    ) {
        let ring = KeyRing::new(key("a", secret));
        let text = ring.seal(&cursor);
        let (b, now) = (cursor.binding(), cursor.issued_at);
        let open = |binding: Binding| ring.open(&text, &binding, now);
        let project = ProjectId::from_bytes(project);
        if project != b.project_id {
            prop_assert_eq!(open(Binding { project_id: project, ..b }), Err(CursorError::WrongProject));
        }
        let route = match b.route {
            Route::TraceList => Route::SpanChildren,
            Route::SpanChildren => Route::Search,
            Route::Search => Route::McpOperations,
            Route::McpOperations => Route::TraceList,
        };
        prop_assert_eq!(open(Binding { route, ..b }), Err(CursorError::WrongRoute));
        let kind = match b.kind {
            Kind::Page => Kind::Head,
            Kind::Head => Kind::Page,
        };
        prop_assert_eq!(open(Binding { kind, ..b }), Err(CursorError::WrongKind));
        let filter = FilterHash::from_bytes(filter);
        if filter != b.filter {
            prop_assert_eq!(open(Binding { filter, ..b }), Err(CursorError::FiltersChanged));
        }
        let sort = match b.sort {
            Sort::NewestFirst => Sort::OldestFirst,
            Sort::OldestFirst => Sort::NewestFirst,
        };
        prop_assert_eq!(open(Binding { sort, ..b }), Err(CursorError::SortChanged));
        let window = other_window(b.window);
        prop_assert_ne!(window, b.window);
        prop_assert_eq!(open(Binding { window, ..b }), Err(CursorError::WindowChanged));
        // A ring with another key under the same id refuses it as tampered.
        let impostor = KeyRing::new(key("a", secret.map(|byte| !byte)));
        prop_assert_eq!(impostor.open(&text, &b, now), Err(CursorError::Tampered));
        prop_assert_eq!(open(b), Ok(cursor));
    }

    #[test]
    fn rotation_keeps_old_cursors_until_their_key_is_retired(
        cursor in cursor(),
        first: [u8; 32],
        second: [u8; 32],
        delay in 0..CURSOR_TTL_MICROS,
    ) {
        let mut ring = KeyRing::new(key("a", first));
        let old = ring.seal(&cursor);
        let binding = cursor.binding();
        // The rotation happens after the cursor was issued, while it is still live.
        let rotated_at = cursor.issued_at.saturating_add_micros(delay);
        ring.rotate(key("b", second)).unwrap();
        prop_assert_eq!(ring.active_kid(), &kid("b"));
        prop_assert_eq!(ring.state(&kid("a")), Some(KeyState::VerifyOnly));
        let new = ring.seal(&cursor);
        prop_assert!(new.starts_with("v1.b."), "{}", new);
        prop_assert_eq!(ring.open(&old, &binding, rotated_at), Ok(cursor));
        prop_assert_eq!(ring.open(&new, &binding, rotated_at), Ok(cursor));
        // A day after the rotation every cursor the old key signed has expired, so retiring it then refuses no
        // live cursor.
        let retire_at = rotated_at.saturating_add_micros(CURSOR_TTL_MICROS);
        prop_assert_eq!(ring.open(&old, &binding, retire_at), Err(CursorError::Expired));
        prop_assert!(ring.retire(&kid("a")));
        prop_assert_eq!(ring.state(&kid("a")), None);
        prop_assert_eq!(ring.open(&old, &binding, rotated_at), Err(CursorError::UnknownKid));
        prop_assert_eq!(ring.open(&new, &binding, rotated_at), Ok(cursor));
        // The active key is never retired, and an id is never reused while the ring holds it.
        prop_assert!(!ring.retire(&kid("b")));
        prop_assert_eq!(
            ring.rotate(key("b", first)).unwrap_err(),
            KeyRingError::DuplicateKid(kid("b"))
        );
    }
}

/// Key bytes 0 to 31 under the id `k1`.
fn golden_ring() -> KeyRing {
    KeyRing::new(key("k1", std::array::from_fn(|i| i as u8)))
}

const ISSUED: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);
const MINUTE: i64 = 60_000_000;

fn golden_cursors() -> [Cursor; 4] {
    let window = TimeWindow::new(
        ISSUED.saturating_add_micros(-24 * 60 * MINUTE),
        ISSUED.saturating_add_micros(60 * MINUTE),
    )
    .unwrap();
    let mut trace = [0xa5; 16];
    trace[8..].copy_from_slice(&7u64.to_be_bytes());
    let trace = TraceId::from_bytes(trace);
    let issue = |sort, position| {
        Cursor::new(
            ProjectId::from_bytes([1; 16]),
            FilterHash::of(b""),
            sort,
            position,
            ISSUED,
        )
    };
    [
        issue(
            Sort::NewestFirst,
            Position::TracePage {
                window,
                as_of: ISSUED,
                last: (ISSUED.saturating_add_micros(-MINUTE), 0xdead_beef, trace),
            },
        ),
        issue(
            Sort::NewestFirst,
            Position::TraceHead {
                window,
                as_of: ISSUED,
                wal_epoch: WalEpoch::from_bytes([0xe0; 16]),
                lsn: Lsn::new(42),
            },
        ),
        issue(
            Sort::OldestFirst,
            Position::ChildPage {
                trace_id: trace,
                parent: SpanId::from_bytes(10u64.to_be_bytes()),
                last: (
                    ISSUED.saturating_add_micros(-59_000_000),
                    SpanId::from_bytes(11u64.to_be_bytes()),
                ),
            },
        ),
        issue(
            Sort::NewestFirst,
            Position::SearchPage {
                window,
                as_of: ISSUED,
                last: (
                    ISSUED.saturating_add_micros(-MINUTE),
                    0xdead_beef,
                    SpanId::from_bytes(12u64.to_be_bytes()),
                ),
            },
        ),
    ]
}

/// The expected texts come from a separate encoder of the layout in the payload module's documentation
/// (Python's `struct`, `hmac` and `base64`), so the layout, the MAC and the encoding are each pinned.
#[test]
fn the_text_form_matches_a_separate_encoder_of_the_layout() {
    let ring = golden_ring();
    let expected = [
        "v1.k1.AQEBAQEBAQEBAQEBAQEBAQEBAQHjsMRCmPwcFJr79MiZb7kkAAZgdYnS1gAABmCJp6o2AAAGYGFr-3YAAAZgdmBmegAABmB1idLWAAAGYHWGP08A3q2-76WlpaWlpaWlAAAAAAAAAAc.6HzZu5Q6KvE6HcQo5nszEw",
        "v1.k1.AQIBAQEBAQEBAQEBAQEBAQEBAQHjsMRCmPwcFJr79MiZb7kkAAZgdYnS1gAABmCJp6o2AAAGYGFr-3YAAAZgdmBmegAABmB1idLWAODg4ODg4ODg4ODg4ODg4OAAAAAAAAAAKg.FdfBQUrJd_OltJZsDIsjFg",
        "v1.k1.AQECAgEBAQEBAQEBAQEBAQEBAQHjsMRCmPwcFJr79MiZb7kkAAZgdYnS1gAABmCJp6o2AKWlpaWlpaWlAAAAAAAAAAcAAAAAAAAACgAGYHWGTpFAAAAAAAAAAAs.Fd_lH4ykLaXgVRq4aiiLsQ",
        "v1.k1.AQEDAQEBAQEBAQEBAQEBAQEBAQHjsMRCmPwcFJr79MiZb7kkAAZgdYnS1gAABmCJp6o2AAAGYGFr-3YAAAZgdmBmegAABmB1idLWAAAGYHWGP08A3q2-7wAAAAAAAAAM.O-1AhOlK8y1UTtCXRFcUlA",
    ];
    for (cursor, expected) in golden_cursors().iter().zip(expected) {
        assert_eq!(ring.seal(cursor), expected);
        assert_eq!(ring.open(expected, &cursor.binding(), ISSUED), Ok(*cursor));
    }
    assert_eq!(
        FilterHash::of(b"").to_bytes(),
        [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24
        ]
    );
    assert_eq!(
        FilterHash::of(b"abc").to_bytes(),
        [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23
        ]
    );
}

/// Every substitution of one ASCII byte, at every position, and every truncation, of one cursor of each kind.
#[test]
fn no_single_byte_change_or_truncation_opens() {
    let ring = golden_ring();
    for cursor in golden_cursors() {
        let text = ring.seal(&cursor);
        let binding = cursor.binding();
        for at in 0..text.len() {
            for replacement in (0u8..128).filter(|b| *b != text.as_bytes()[at]) {
                let mut bytes = text.clone().into_bytes();
                bytes[at] = replacement;
                let changed = String::from_utf8(bytes).unwrap();
                assert!(ring.open(&changed, &binding, ISSUED).is_err(), "{changed}");
            }
            assert!(ring.open(&text[..at], &binding, ISSUED).is_err());
        }
    }
}

/// The MAC's 16 bytes take 22 base64 characters whose last one carries 4 unused bits. A last character that
/// differs only in those bits decodes to the same bytes under a lenient decoder; it must not open.
#[test]
fn a_mac_with_unused_bits_set_is_malformed() {
    let ring = golden_ring();
    let [cursor, ..] = golden_cursors();
    let text = ring.seal(&cursor);
    let (signed, tag) = text.rsplit_once('.').unwrap();
    assert_eq!(tag.len(), 22);
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let last = alphabet
        .iter()
        .position(|c| *c == *tag.as_bytes().last().unwrap())
        .unwrap();
    assert_eq!(
        last & 0b1111,
        0,
        "the unused bits of a canonical tag are zero"
    );
    for spare in 1..16 {
        let changed = format!(
            "{signed}.{}{}",
            &tag[..21],
            char::from(alphabet[last | spare])
        );
        assert_eq!(
            ring.open(&changed, &cursor.binding(), ISSUED),
            Err(CursorError::Malformed),
            "{changed}"
        );
    }
    // Padding is not base64url without padding.
    let padded = format!("{text}==");
    assert_eq!(
        ring.open(&padded, &cursor.binding(), ISSUED),
        Err(CursorError::Malformed)
    );
}

#[test]
fn text_that_is_not_a_v1_cursor_is_malformed() {
    let ring = golden_ring();
    let [cursor, ..] = golden_cursors();
    let (binding, text) = (cursor.binding(), ring.seal(&cursor));
    let open = |text: &str| ring.open(text, &binding, ISSUED);
    let long = format!("{text}{}", "A".repeat(MAX_TEXT_LEN));
    for bad in [
        "",
        "garbage",
        "v1",
        "v1.k1",
        "v1.k1.AAAA",
        "tmp1.p.AAAA",
        &format!("v2{}", &text[2..]),
        &format!("{text}.AAAA"),
        &format!("x{text}"),
        &long,
    ] {
        assert_eq!(open(bad), Err(CursorError::Malformed), "{bad:?}");
    }
    assert_eq!(open("v1.k.AAAA.BBBB"), Err(CursorError::UnknownKid));
    assert_eq!(open(&text), Ok(cursor));
}

#[test]
fn decoding_refuses_what_encoding_cannot_produce() {
    let [page, head, child, search] = golden_cursors().map(|c| c.encode().as_bytes().to_vec());
    assert_eq!(
        (page.len(), head.len(), child.len(), search.len()),
        (MAX_PAYLOAD_LEN, 100, 92, 96)
    );
    let with = |bytes: &[u8], at: usize, value: u8| {
        let mut bytes = bytes.to_vec();
        bytes[at] = value;
        Cursor::decode(&bytes)
    };
    assert_eq!(with(&page, 0, 2), Err(DecodeError::Version(2)));
    assert_eq!(with(&page, 0, 0), Err(DecodeError::Version(0)));
    for (at, value) in [(1, 0), (1, 3), (2, 0), (2, 5), (3, 0), (3, 3)] {
        assert_eq!(
            with(&page, at, value),
            Err(DecodeError::Value),
            "{at} {value}"
        );
    }
    // Head and span children, and head and search, are no kind of cursor.
    assert_eq!(with(&head, 2, 2), Err(DecodeError::Value));
    assert_eq!(with(&head, 2, 3), Err(DecodeError::Value));
    assert_eq!(with(&head, 2, 4), Err(DecodeError::Value));
    // A page of span children or of search read as a trace page runs out of bytes; the reverse has bytes left
    // over.
    assert_eq!(with(&child, 2, 1), Err(DecodeError::Length));
    assert_eq!(with(&page, 2, 2), Err(DecodeError::Length));
    assert_eq!(with(&search, 2, 1), Err(DecodeError::Length));
    assert_eq!(with(&page, 2, 3), Err(DecodeError::Length));
    // The window must be `from < to`: write `to = from`.
    let mut empty = page.clone();
    let from: [u8; 8] = page[52..60].try_into().unwrap();
    empty[60..68].copy_from_slice(&from);
    assert_eq!(Cursor::decode(&empty), Err(DecodeError::Value));
    for bytes in [&page, &head, &child, &search] {
        assert_eq!(
            Cursor::decode(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Length)
        );
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert_eq!(Cursor::decode(&longer), Err(DecodeError::Length));
    }
    assert_eq!(Cursor::decode(&[]), Err(DecodeError::Length));
}

#[test]
fn a_ring_has_exactly_one_active_key_and_unique_ids() {
    let (a, b) = (key("a", [1; 32]), key("b", [2; 32]));
    let ring = |keys: Vec<(SigningKey, KeyState)>| KeyRing::from_keys(keys).map(|_| ());
    assert_eq!(ring(Vec::new()), Err(KeyRingError::NoActiveKey));
    assert_eq!(
        ring(vec![(a.clone(), KeyState::VerifyOnly)]),
        Err(KeyRingError::NoActiveKey)
    );
    assert_eq!(
        ring(vec![
            (a.clone(), KeyState::Active),
            (b.clone(), KeyState::Active)
        ]),
        Err(KeyRingError::SeveralActiveKeys)
    );
    assert_eq!(
        ring(vec![
            (a.clone(), KeyState::Active),
            (key("a", [3; 32]), KeyState::VerifyOnly)
        ]),
        Err(KeyRingError::DuplicateKid(kid("a")))
    );
    // A verify-only key opens what it signed; the active key signs.
    let [cursor, ..] = golden_cursors();
    let old = KeyRing::new(a.clone()).seal(&cursor);
    let both = KeyRing::from_keys([(a, KeyState::VerifyOnly), (b, KeyState::Active)]).unwrap();
    assert_eq!(both.active_kid(), &kid("b"));
    assert_eq!(both.open(&old, &cursor.binding(), ISSUED), Ok(cursor));
    assert!(both.seal(&cursor).starts_with("v1.b."));
    // Debug never shows a secret.
    assert_eq!(
        format!("{both:?}"),
        "KeyRing { active: SigningKey { kid: Kid(\"b\"), .. }, verify_only: [SigningKey { kid: Kid(\"a\"), .. }] }"
    );
}

#[test]
fn key_ids_are_short_and_never_hold_the_separator() {
    let longest = "x".repeat(32);
    for ok in ["a", "2026-11", "k_1", "Z9", longest.as_str()] {
        assert_eq!(Kid::new(ok).map(|k| k.to_string()), Ok(ok.to_owned()));
    }
    let too_long = "x".repeat(33);
    for bad in [
        "",
        ".",
        "a.b",
        "a b",
        "\u{fc}",
        "a+b",
        "a/b",
        "a=",
        too_long.as_str(),
    ] {
        assert_eq!(Kid::new(bad), Err(InvalidKid), "{bad:?}");
    }
}

#[test]
fn every_error_is_cursor_invalid() {
    for error in [
        CursorError::Malformed,
        CursorError::UnknownKid,
        CursorError::Tampered,
        CursorError::Expired,
        CursorError::WrongProject,
        CursorError::WrongRoute,
        CursorError::WrongKind,
        CursorError::FiltersChanged,
        CursorError::SortChanged,
        CursorError::WindowChanged,
    ] {
        assert_eq!(error.problem_code(), "cursor_invalid");
    }
}
