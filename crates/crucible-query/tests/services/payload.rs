//! Payload byte ranges, and the bodies the media of one span are found in: from the store, from the WAL through the
//! recent index for a span the store does not serve yet, `not_found` only for a whole and strong miss, and payload
//! retention (contract 8.6, 8.7, 8.9 and 14.2).

use crucible_env::QueryError;
use crucible_plan::{Caller, PayloadField};
use crucible_query::{
    PayloadBytes, PayloadQuery, QueryServiceError, ReadContext, RetentionSettings, SpanBodies,
};
use crucible_recent::SpanEntry;
use crucible_recent::test_support::span_record;
use crucible_records::{TableId, Timestamp};

use crate::world::{DAY, NOW, World, ctx, run, scoped, span, span_payload_row};

const INPUT: &str = "{\"messages\":[{\"role\":\"user\",\"content\":\"h\u{e9}llo\"}]}";

fn query(entry: &SpanEntry, range: Option<(u64, u64)>) -> PayloadQuery {
    PayloadQuery {
        trace_id: entry.summary.trace_id,
        span_id: entry.summary.span_id,
        start_time: entry.summary.start_time,
        field: PayloadField::Input,
        range,
    }
}

fn read(world: &World, query: PayloadQuery) -> Result<PayloadBytes, QueryServiceError> {
    run(world.service.payload(&ctx(1), query))
}

/// A node whose store holds one span with `INPUT`.
fn shipped() -> (World, SpanEntry) {
    let mut world = World::new(100);
    let entry = span(1, 10, 0, NOW.saturating_add_micros(-1_500));
    let lsn = world.ack(vec![entry.clone()]);
    world.store.insert(
        TableId::SpanPayload,
        span_payload_row(&entry, lsn, NOW, INPUT, &[]),
    );
    world.settle();
    (world, entry)
}

#[test]
fn ranges_come_from_the_store_with_the_field_length() {
    let (world, entry) = shipped();
    let total = INPUT.len() as u64;
    let whole = read(&world, query(&entry, None)).unwrap();
    assert_eq!(whole.bytes, INPUT.as_bytes());
    assert_eq!(whole.total, total);
    assert!(!whole.meta.partial);
    let part = read(&world, query(&entry, Some((2, 8)))).unwrap();
    assert_eq!(part.bytes, &INPUT.as_bytes()[2..10]);
    assert_eq!(part.total, total);
    // A range that cuts a character returns the bytes as they are.
    let at = INPUT.find('\u{e9}').unwrap() as u64;
    let cut = read(&world, query(&entry, Some((at, 1)))).unwrap();
    assert_eq!(cut.bytes, [0xc3]);
    // A range past the end is empty, with the length.
    let past = read(&world, query(&entry, Some((total + 5, 10)))).unwrap();
    assert!(past.bytes.is_empty());
    assert_eq!(past.total, total);
    assert_eq!(
        read(&world, query(&entry, Some((0, 0)))),
        Err(QueryServiceError::Validation("range"))
    );
}

#[test]
fn a_span_the_store_does_not_serve_yet_is_read_from_its_frame() {
    let mut world = World::new(100);
    let entry = span(1, 10, 0, NOW);
    let lsn = world.ack(vec![entry.clone()]);
    let mut record = span_record(&entry, lsn, NOW);
    record.payload.input = INPUT.as_bytes().to_vec().into();
    world.wal.put(entry.locator, record);
    let part = read(&world, query(&entry, Some((0, 4)))).unwrap();
    assert_eq!(part.bytes, &INPUT.as_bytes()[..4]);
    assert_eq!(part.total, INPUT.len() as u64);
    // Another start time names another span.
    let mut other = query(&entry, None);
    other.start_time = NOW.saturating_add_micros(1);
    assert_eq!(read(&world, other), Err(QueryServiceError::NotFound));
    // With the store down the frame still answers.
    world.store.set_unavailable(true);
    let down = read(&world, query(&entry, None)).unwrap();
    assert_eq!(down.bytes, INPUT.as_bytes());
    assert!(down.meta.partial);
}

fn bodies(
    world: &World,
    entry: &SpanEntry,
    start_time: Timestamp,
) -> Result<SpanBodies, QueryServiceError> {
    let span = &entry.summary;
    run(world
        .service
        .span_bodies(&ctx(1), span.trace_id, span.span_id, start_time))
}

#[test]
fn the_bodies_of_a_span_the_store_does_not_serve_yet_come_from_its_frame() {
    let mut world = World::new(100);
    let entry = span(1, 10, 0, NOW);
    let lsn = world.ack(vec![entry.clone()]);
    let mut record = span_record(&entry, lsn, NOW);
    record.payload.input = INPUT.as_bytes().to_vec().into();
    record.payload.output = b"{\"image\":\"media-1\"}".to_vec().into();
    record.payload.metadata_keys = vec!["attachment".into()];
    record.payload.metadata_values = vec!["media-2".into()];
    world.wal.put(entry.locator, record);
    let expected = SpanBodies {
        input: INPUT.as_bytes().to_vec(),
        output: b"{\"image\":\"media-1\"}".to_vec(),
        metadata: vec![("attachment".to_owned(), "media-2".to_owned())],
    };
    assert_eq!(bodies(&world, &entry, NOW), Ok(expected.clone()));
    // Another start time names another span.
    assert_eq!(
        bodies(&world, &entry, NOW.saturating_add_micros(1)),
        Err(QueryServiceError::NotFound)
    );
    // With the store down the frame still answers.
    world.store.set_unavailable(true);
    assert_eq!(bodies(&world, &entry, NOW), Ok(expected));
}

#[test]
fn a_miss_is_not_found_only_when_whole_and_strong() {
    let (world, entry) = shipped();
    let mut absent = query(&entry, None);
    absent.span_id = crate::world::span_id(99);
    assert_eq!(read(&world, absent), Err(QueryServiceError::NotFound));
    // A frame acknowledged but not applied yet may hold it.
    let mut lagging = World::new(100);
    lagging.ack_only(vec![entry.clone()]);
    assert!(matches!(
        read(&lagging, query(&entry, None)),
        Err(QueryServiceError::Store(QueryError::Unavailable(_)))
    ));
    world.store.set_unavailable(true);
    assert!(matches!(
        read(&world, query(&entry, None)),
        Err(QueryServiceError::Store(QueryError::Unavailable(_)))
    ));
}

#[test]
fn expired_payloads_are_not_found() {
    let mut world = World::new(100);
    let entry = span(1, 10, 0, NOW.saturating_add_micros(-3 * DAY));
    let lsn = world.ack(vec![entry.clone()]);
    world.store.insert(
        TableId::SpanPayload,
        span_payload_row(&entry, lsn, NOW, INPUT, &[]),
    );
    world.settle();
    assert!(read(&world, query(&entry, None)).is_ok());
    let retention = RetentionSettings {
        retention_days: Some(30),
        payload_retention_days: Some(2),
        mcp_content_retention_days: None,
    };
    let short = ReadContext::new(scoped(1), retention, Caller::Session);
    assert_eq!(
        run(world.service.payload(&short, query(&entry, None))),
        Err(QueryServiceError::NotFound)
    );
}
