//! The span list and the trace list under a list filter: the merge with the recent index, the store-only read
//! of filtered lists, the semi-joins into the spans of a trace, paging, and `meta` under an outage.

use std::sync::Arc;
use std::time::Duration;

use crucible_plan::{Filter, TextField, Value};
use crucible_query::{
    Consistency, PartialReason, QueryServiceError, SpanListCursor, SpanListPage, SpanListQuery,
    TraceListQuery,
};
use crucible_recent::SpanEntry;
use crucible_records::{Level, ObservationType, SpanId, TableId, TimeWindow, Timestamp, TraceId};

use crate::store::stored_row;
use crate::world::{DAY, NOW, World, ctx, project, run, span, span_id, trace};

const MS: i64 = 1_000;

fn at(ms: i64) -> Timestamp {
    NOW.saturating_add_micros(ms * MS)
}

fn window() -> TimeWindow {
    TimeWindow::new(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(DAY),
    )
    .unwrap()
}

fn spans(
    world: &World,
    filter: Filter,
    limit: u32,
    cursor: Option<SpanListCursor>,
) -> SpanListPage {
    let query = SpanListQuery {
        window: window(),
        filter,
        limit: Some(limit),
        cursor,
    };
    run(world.service.span_list(&ctx(1), query)).unwrap()
}

fn ids(page: &SpanListPage) -> Vec<SpanId> {
    page.data.iter().map(|span| span.span_id).collect()
}

fn typed(mut entry: SpanEntry, kind: ObservationType) -> SpanEntry {
    entry.summary.observation_type = kind;
    entry
}

/// The `span_payload` row of a span with one metadata entry.
fn payload(entry: &SpanEntry, metadata: &str) -> Vec<Value> {
    let s = &entry.summary;
    stored_row(
        TableId::SpanPayload,
        &[
            ("project_id", Value::Uuid(project(1).uuid())),
            ("start_minute", Value::Time(s.start_time.floor_minute())),
            (
                "trace_hash",
                Value::UInt(u64::from(s.trace_id.trace_hash())),
            ),
            ("span_id", Value::Bytes(s.span_id.as_bytes().to_vec())),
            ("trace_id", Value::Bytes(s.trace_id.as_bytes().to_vec())),
            ("start_time", Value::Time(s.start_time)),
            (
                "metadata_kv",
                Value::Array(vec![Value::String(metadata.to_owned())]),
            ),
        ],
    )
}

#[test]
fn the_span_list_merges_the_store_and_the_index_newest_first() {
    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, at(-30)), span(1, 11, 10, at(-29))]);
    world.settle();
    world.ack(vec![span(2, 20, 0, at(-20)), span(2, 21, 20, at(-19))]);

    let first = spans(&world, Filter::all(), 3, None);
    assert_eq!(ids(&first), [span_id(21), span_id(20), span_id(11)]);
    assert!(first.has_more);
    assert_eq!(first.meta.consistency, Consistency::Strong);
    assert!(!first.meta.partial);
    // A span acknowledged after the first page, older than its last row, is not on later pages: `as_of`
    // holds them still.
    world.clock.advance(Duration::from_millis(1));
    world.ack(vec![span(3, 30, 0, at(-35))]);
    let second = spans(&world, Filter::all(), 3, first.next_cursor);
    assert_eq!(ids(&second), [span_id(10)]);
    assert!(!second.has_more && second.next_cursor.is_none());
    // A first page sees it.
    let again = spans(&world, Filter::all(), 10, None);
    assert_eq!(again.data.len(), 5);
    assert_eq!(again.data[4].span_id, span_id(30));
}

#[test]
fn a_filtered_span_list_reads_the_store_only_and_says_when_that_trails() {
    let mut world = World::new(100);
    let mut failed = span(1, 11, 10, at(-29));
    failed.summary.level = Level::Error;
    world.ack(vec![span(1, 10, 0, at(-30)), failed]);
    world.settle();
    let mut pending = span(2, 21, 20, at(-19));
    pending.summary.level = Level::Error;
    world.ack(vec![span(2, 20, 0, at(-20)), pending]);

    let errors = Filter::Level(vec![Level::Error]);
    let listed = spans(&world, errors.clone(), 10, None);
    assert_eq!(ids(&listed), [span_id(11)]);
    assert_eq!(listed.meta.consistency, Consistency::Lagging);
    world.settle();
    let listed = spans(&world, errors, 10, None);
    assert_eq!(ids(&listed), [span_id(21), span_id(11)]);
    assert_eq!(listed.meta.consistency, Consistency::Strong);

    // Metadata conditions are semi-joined from the payload tier.
    let entry = span(1, 10, 0, at(-30));
    world
        .store
        .insert(TableId::SpanPayload, payload(&entry, "team=search"));
    let tagged = Filter::Metadata {
        key: "team".to_owned(),
        value: "search".to_owned(),
    };
    assert_eq!(ids(&spans(&world, tagged, 10, None)), [span_id(10)]);
    let by_user = Filter::Text(TextField::UserId, vec!["nobody".to_owned()]);
    assert!(spans(&world, by_user, 10, None).data.is_empty());
}

#[test]
fn an_outage_leaves_the_index_answer_partial_and_fails_filtered_lists() {
    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, at(-30))]);
    world.settle();
    world.ack(vec![span(2, 20, 0, at(-20))]);
    world.store.set_unavailable(true);
    let page = spans(&world, Filter::all(), 10, None);
    assert_eq!(ids(&page), [span_id(20)]);
    assert!(page.meta.partial);
    assert_eq!(
        page.meta.partial_reason,
        Some(PartialReason::StoreUnavailable)
    );
    let query = SpanListQuery {
        window: window(),
        filter: Filter::Level(vec![Level::Error]),
        limit: None,
        cursor: None,
    };
    let failed = run(world.service.span_list(&ctx(1), query));
    assert!(
        matches!(failed, Err(QueryServiceError::Store(_))),
        "{failed:?}"
    );
}

fn traces(world: &World, filter: Filter) -> Vec<TraceId> {
    let query = TraceListQuery {
        window: window(),
        filter,
        limit: None,
        cursor: None,
    };
    let page = run(world.service.trace_list(&ctx(1), query)).unwrap();
    page.data.iter().map(|item| item.head.trace_id).collect()
}

#[test]
fn trace_list_filters_on_span_fields_keep_traces_with_a_matching_span() {
    let mut world = World::new(100);
    world.ack(vec![
        span(1, 10, 0, at(-30)),
        typed(span(1, 11, 10, at(-29)), ObservationType::Tool),
    ]);
    world.ack(vec![
        span(2, 20, 0, at(-20)),
        typed(span(2, 21, 20, at(-19)), ObservationType::Generation),
    ]);
    let mut named = span(3, 30, 0, at(-10));
    named.summary.name = Arc::from("Checkout Agent");
    named.summary.user_id = Arc::from("ada");
    world.ack(vec![named]);
    world.settle();
    let tool = Filter::ObservationType(vec![ObservationType::Tool]);
    assert_eq!(traces(&world, tool), [trace(1)]);
    let either = Filter::ObservationType(vec![ObservationType::Tool, ObservationType::Generation]);
    assert_eq!(traces(&world, either), [trace(2), trace(1)]);
    // Head conditions and span conditions together.
    let both = Filter::And(vec![
        Filter::ObservationType(vec![ObservationType::Generation]),
        Filter::Name("span-20".to_owned()),
    ]);
    assert_eq!(traces(&world, both), [trace(2)]);
    assert_eq!(
        traces(&world, Filter::Name("checkout".to_owned())),
        [trace(3)]
    );
    let by_user = Filter::Text(TextField::UserId, vec!["ada".to_owned()]);
    assert_eq!(traces(&world, by_user), [trace(3)]);
    // Metadata, through the payload tier.
    let entry = span(2, 21, 20, at(-19));
    world
        .store
        .insert(TableId::SpanPayload, payload(&entry, "tier=gold"));
    let gold = Filter::Metadata {
        key: "tier".to_owned(),
        value: "gold".to_owned(),
    };
    assert_eq!(traces(&world, gold), [trace(2)]);
    // Metadata and an observation type cannot be evaluated by one table: the request is refused, naming the
    // field.
    let query = TraceListQuery {
        window: window(),
        filter: Filter::And(vec![
            Filter::Metadata {
                key: "tier".to_owned(),
                value: "gold".to_owned(),
            },
            Filter::ObservationType(vec![ObservationType::Tool]),
        ]),
        limit: None,
        cursor: None,
    };
    let refused = run(world.service.trace_list(&ctx(1), query));
    assert_eq!(
        refused,
        Err(QueryServiceError::Validation("observation_type"))
    );
}
