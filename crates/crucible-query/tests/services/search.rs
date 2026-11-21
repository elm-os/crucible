//! Full-text search: pages along the hits' key, highlights, phrases, metadata entries, store outages, the
//! recent index and retention (contract 8.7, 8.9 and 12.4).

use crucible_env::QueryError;
use crucible_plan::{Caller, Filter, SearchField, TokenMode};
use crucible_query::{
    Consistency, QueryServiceError, ReadContext, RetentionSettings, SearchCursor, SearchHit,
    SearchPage, SearchQuery,
};
use crucible_recent::SpanEntry;
use crucible_records::{SpanId, TableId, TimeWindow};

use crate::world::{DAY, NOW, World, ctx, project, run, scoped, span, span_id, span_payload_row};

const MINUTE: i64 = 60_000_000;

fn window() -> TimeWindow {
    TimeWindow::new(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(DAY),
    )
    .unwrap()
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
            span_payload_row(entry, lsn, NOW, input, metadata),
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
        filter: Filter::all(),
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
        span_payload_row(&entry, lsn, NOW, "refund then", &[]),
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

/// A page's spans are read from their core rows a few dozen at a time, so the key lists of one lookup leave room,
/// within the 1,000 values one ClickHouse request holds, for the filter and the pending deletions it carries.
#[test]
fn a_large_page_reads_its_spans_in_lookups_of_a_bounded_size() {
    let inputs: Vec<(&str, &[&str])> = (0..150).map(|_| ("refund", &[][..])).collect();
    let world = world_with(&inputs);
    let mut large = query("refund", TokenMode::All, &TEXT);
    large.limit = Some(150);
    let page = search(&world, large).unwrap();
    assert_eq!(page.data.len(), 150);
    // The hits, then three lookups of at most 64 spans each.
    assert_eq!(world.store.executed().len(), 4);
}

/// The filter of a search keeps the hits whose span matches: conditions on the payload restrict the hits, the
/// others test the core rows, and a cursor's canonical form covers the filter without changing for a search
/// without one.
#[test]
fn a_filter_keeps_the_hits_whose_span_matches_and_binds_the_cursor() {
    let world = world_with(&[
        ("refund", &["team=pay"]),
        ("refund", &["team=ops"]),
        ("refund", &["team=pay"]),
    ]);
    let metadata = Filter::And(vec![Filter::Metadata {
        key: "team".to_owned(),
        value: "pay".to_owned(),
    }]);
    let named = Filter::And(vec![Filter::Name("SPAN-11".to_owned())]);
    let mut by_team = query("refund", TokenMode::All, &TEXT);
    by_team.filter = metadata.clone();
    assert_eq!(
        ids(&search(&world, by_team.clone()).unwrap().data),
        [span_id(10), span_id(12)]
    );
    let mut by_name = query("refund", TokenMode::All, &TEXT);
    by_name.filter = named.clone();
    assert_eq!(
        ids(&search(&world, by_name.clone()).unwrap().data),
        [span_id(11)]
    );

    let plain = query("refund", TokenMode::All, &TEXT);
    let empty = SearchQuery {
        filter: Filter::And(Vec::new()),
        ..plain.clone()
    };
    assert_eq!(plain.canonical(), empty.canonical());
    assert!(!plain.canonical().windows(6).any(|w| w == b"filter"));
    assert_ne!(plain.canonical(), by_team.canonical());
    assert_ne!(by_team.canonical(), by_name.canonical());
}
