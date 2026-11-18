//! Full-text search: pages along the hits' key, highlights, phrases, metadata entries, store outages, the
//! recent index and retention (contract 8.7, 8.9 and 12.4).

use crucible_env::QueryError;
use crucible_plan::{Caller, SearchField, TokenMode, Value};
use crucible_query::{
    Consistency, QueryServiceError, ReadContext, RetentionSettings, SearchCursor, SearchHit,
    SearchPage, SearchQuery,
};
use crucible_recent::SpanEntry;
use crucible_records::{Lsn, SpanId, TableId, TimeWindow};

use crate::store::stored_row;
use crate::world::{DAY, NOW, World, ctx, project, run, scoped, span, span_id};

const MINUTE: i64 = 60_000_000;

fn window() -> TimeWindow {
    TimeWindow::new(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(DAY),
    )
    .unwrap()
}

/// The `span_payload` row of a span, as the shipper writes it.
fn payload_row(entry: &SpanEntry, lsn: Lsn, input: &str, metadata: &[&str]) -> Vec<Value> {
    let s = &entry.summary;
    stored_row(
        TableId::SpanPayload,
        &[
            ("project_id", Value::Uuid(entry.project_id.uuid())),
            ("start_minute", Value::Time(s.start_time.floor_minute())),
            (
                "trace_hash",
                Value::UInt(u64::from(s.trace_id.trace_hash())),
            ),
            ("span_id", Value::Bytes(s.span_id.as_bytes().to_vec())),
            ("trace_id", Value::Bytes(s.trace_id.as_bytes().to_vec())),
            ("start_time", Value::Time(s.start_time)),
            ("input", Value::String(input.to_owned())),
            (
                "metadata_kv",
                Value::Array(
                    metadata
                        .iter()
                        .map(|m| Value::String((*m).to_owned()))
                        .collect(),
                ),
            ),
            ("lsn", Value::UInt(lsn.get())),
            ("received_at", Value::Time(NOW)),
        ],
    )
}

/// A node whose store holds one span per input, a minute apart, the first the newest, in trace 1.
fn world_with(inputs: &[(&str, &[&str])]) -> World {
    let mut world = World::new(100);
    let entries: Vec<SpanEntry> = inputs
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let start = NOW.saturating_add_micros(-(i as i64) * MINUTE);
            span(1, 10 + i as u64, 0, start)
        })
        .collect();
    let lsn = world.ack(entries.clone());
    for (entry, (input, metadata)) in entries.iter().zip(inputs) {
        world.store.insert(
            TableId::SpanPayload,
            payload_row(entry, lsn, input, metadata),
        );
    }
    world.settle();
    world
}

fn query(text: &str, mode: TokenMode, fields: &[SearchField]) -> SearchQuery {
    SearchQuery {
        window: window(),
        text: text.to_owned(),
        mode,
        fields: fields.to_vec(),
        limit: None,
        cursor: None,
    }
}

fn search(world: &World, query: SearchQuery) -> Result<SearchPage, QueryServiceError> {
    run(world.service.search(&ctx(1), query))
}

fn ids(hits: &[SearchHit]) -> Vec<SpanId> {
    hits.iter().map(|hit| hit.span.summary.span_id).collect()
}

fn highlighted(hit: &SearchHit) -> Vec<(SearchField, String)> {
    hit.matches
        .iter()
        .flat_map(|m| {
            m.fragments
                .iter()
                .filter(|f| f.highlighted)
                .map(|f| (m.field, f.text.clone()))
        })
        .collect()
}

const TEXT: [SearchField; 2] = [SearchField::Input, SearchField::Output];

#[test]
fn pages_follow_the_hit_key_newest_first() {
    let world = world_with(&[
        ("a Refund was issued", &[]),
        ("no match here", &[]),
        ("refund denied", &[]),
        ("REFUND!", &[]),
    ]);
    let mut first = query("refund", TokenMode::All, &TEXT);
    first.limit = Some(2);
    let page = search(&world, first.clone()).unwrap();
    assert_eq!(ids(&page.data), [span_id(10), span_id(12)]);
    assert!(page.has_more);
    assert_eq!(page.meta.consistency, Consistency::Strong);
    assert!(!page.meta.partial);
    assert_eq!(
        highlighted(&page.data[0]),
        [(SearchField::Input, "Refund".to_owned())]
    );
    let fragments = &page.data[0].matches[0].fragments;
    assert_eq!(
        fragments
            .iter()
            .map(|f| f.text.as_str())
            .collect::<String>(),
        "a Refund was issued"
    );
    assert!(!page.data[0].matches[0].cut_before && !page.data[0].matches[0].cut_after);

    let next = SearchQuery {
        cursor: page.next_cursor,
        ..first.clone()
    };
    let page = search(&world, next).unwrap();
    assert_eq!(ids(&page.data), [span_id(13)]);
    assert!(!page.has_more && page.next_cursor.is_none());
    assert_eq!(
        highlighted(&page.data[0]),
        [(SearchField::Input, "REFUND".to_owned())]
    );

    // A cursor belongs to its project and window.
    let (_, window, as_of, last) = search(&world, first.clone())
        .unwrap()
        .next_cursor
        .unwrap()
        .parts();
    let foreign = SearchCursor::from_parts(project(2), window, as_of, last);
    let moved = SearchQuery {
        cursor: Some(foreign),
        ..first
    };
    assert_eq!(search(&world, moved), Err(QueryServiceError::CursorInvalid));
}

#[test]
fn a_phrase_keeps_hits_whose_snippet_holds_it_and_pages_past_the_rest() {
    let world = world_with(&[
        ("limit the rate", &[]),
        ("the rate-limit was hit", &[]),
        ("nothing", &[]),
    ]);
    let mut phrase = query("Rate Limit", TokenMode::Phrase, &TEXT);
    phrase.limit = Some(1);
    // The newest candidate has both tokens out of order: dropped, yet the page moves on.
    let page = search(&world, phrase.clone()).unwrap();
    assert!(page.data.is_empty());
    assert!(page.has_more);
    let next = SearchQuery {
        cursor: page.next_cursor,
        ..phrase
    };
    let page = search(&world, next).unwrap();
    assert_eq!(ids(&page.data), [span_id(11)]);
    assert_eq!(
        highlighted(&page.data[0]),
        [(SearchField::Input, "rate-limit".to_owned())]
    );
    assert!(!page.has_more);
}

#[test]
fn metadata_entries_match_whole_and_merge_with_text_hits() {
    let world = world_with(&[
        ("tenant acme onboarding", &[]),
        ("unrelated", &["tenant=Acme", "env=prod"]),
        ("unrelated", &["tenant=acme"]),
    ]);
    let both = [SearchField::Input, SearchField::Metadata];
    let page = search(&world, query("tenant=Acme", TokenMode::All, &both)).unwrap();
    // The text tokens `tenant` and `acme` find the first span; the case-sensitive entry finds the second.
    assert_eq!(ids(&page.data), [span_id(10), span_id(11)]);
    assert_eq!(page.data[1].matches.len(), 1);
    assert_eq!(page.data[1].matches[0].field, SearchField::Metadata);
    assert!(page.data[1].matches[0].fragments.is_empty());
    let entries = search(
        &world,
        query("env=prod", TokenMode::Any, &[SearchField::Metadata]),
    )
    .unwrap();
    assert_eq!(ids(&entries.data), [span_id(11)]);
}

#[test]
fn a_query_without_tokens_is_refused() {
    let world = world_with(&[("text", &[])]);
    assert_eq!(
        search(&world, query("--- ...", TokenMode::All, &TEXT)),
        Err(QueryServiceError::Validation("query"))
    );
    assert_eq!(
        search(&world, query("text", TokenMode::All, &[])),
        Err(QueryServiceError::Validation("fields"))
    );
}

#[test]
fn a_store_outage_fails_the_search() {
    let world = world_with(&[("refund", &[])]);
    world.store.set_unavailable(true);
    let failed = search(&world, query("refund", TokenMode::All, &TEXT));
    assert!(matches!(
        failed,
        Err(QueryServiceError::Store(QueryError::Unavailable(_)))
    ));
}

#[test]
fn spans_the_store_may_not_serve_yet_make_the_answer_lagging() {
    let mut world = world_with(&[("refund", &[])]);
    // A newer span is acked and in the index, but not shipped: search cannot see it.
    world.ack(vec![span(2, 20, 0, NOW)]);
    let page = search(&world, query("refund", TokenMode::All, &TEXT)).unwrap();
    assert_eq!(ids(&page.data), [span_id(10)]);
    assert_eq!(page.meta.consistency, Consistency::Lagging);
    // Outside the window it does not matter.
    let mut early = query("refund", TokenMode::All, &TEXT);
    early.window = TimeWindow::new(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(-1),
    )
    .unwrap();
    let page = search(&world, early).unwrap();
    assert_eq!(page.meta.consistency, Consistency::Strong);
}

#[test]
fn expired_payloads_are_not_searched() {
    let world = world_with(&[("refund now", &[])]);
    let mut old = World::new(10);
    let entry = span(1, 30, 0, NOW.saturating_add_micros(-3 * DAY));
    let lsn = old.ack(vec![entry.clone()]);
    old.store.insert(
        TableId::SpanPayload,
        payload_row(&entry, lsn, "refund then", &[]),
    );
    old.settle();
    let wide = TimeWindow::new(
        NOW.saturating_add_micros(-5 * DAY),
        NOW.saturating_add_micros(DAY),
    )
    .unwrap();
    let retention = RetentionSettings {
        retention_days: Some(30),
        payload_retention_days: Some(2),
        mcp_content_retention_days: None,
    };
    let short = ReadContext::new(scoped(1), retention, Caller::Session);
    let mut request = query("refund", TokenMode::All, &TEXT);
    request.window = wide;
    let kept = run(old.service.search(&ctx(1), request.clone())).unwrap();
    assert_eq!(ids(&kept.data), [span_id(30)]);
    let expired = run(old.service.search(&short, request.clone())).unwrap();
    assert!(expired.data.is_empty());
    let recent = run(world.service.search(&short, request)).unwrap();
    assert_eq!(ids(&recent.data), [span_id(10)]);
}
