//! A request sends its independent store reads together: the trace list its three fact lookups, trace by id its
//! spans with its four fact lookups. The fake store's reads take time here, so reads sent together are all in
//! flight before the first answers, and it records the most at once. Sending reads together changes no answer
//! and no read count.

use crucible_plan::{Filter, Value};
use crucible_query::{PartialReason, TraceListQuery};
use crucible_records::{FactTargetKind, ScoreDataType, TableId, Uuid};

use crate::list::window;
use crate::store::stored_row;
use crate::world::{NOW, World, ctx, project, run, span, trace};

/// Trace 1, a root and a child, in the store and no longer in the recent index.
fn stored_trace() -> World {
    let mut world = World::new(100);
    let start = NOW.saturating_add_micros(-60_000_000);
    world.ack(vec![
        span(1, 10, 0, start),
        span(1, 11, 10, start.saturating_add_micros(1_000)),
    ]);
    world.settle();
    world.store.slow_reads();
    world
}

#[test]
fn the_trace_list_sends_its_fact_lookups_together() {
    let world = stored_trace();
    let query = TraceListQuery {
        window: window(),
        filter: Filter::all(),
        limit: Some(10),
        cursor: None,
    };
    let page = run(world.service.trace_list(&ctx(1), query)).unwrap();
    assert_eq!(page.data.len(), 1);
    // The heads, then their stats, then the scores, tags and flags of the page at once.
    assert_eq!(world.store.executed().len(), 5);
    assert_eq!(world.store.most_in_flight(), 3);
}

#[test]
fn trace_by_id_sends_its_spans_and_fact_lookups_together() {
    let world = stored_trace();
    let detail = run(world.service.trace_by_id(&ctx(1), trace(1))).unwrap();
    assert_eq!(detail.spans.len(), 2);
    // The bounds, then the spans and the four fact tables at once.
    assert_eq!(world.store.executed().len(), 6);
    assert_eq!(world.store.most_in_flight(), 5);
}

/// A fact of trace 1 in `table`, written a minute before [`NOW`].
fn put_fact(world: &World, table: TableId, key: &str, body: &[(&str, Value)]) {
    let at = NOW.saturating_add_micros(-60_000_000);
    let mut set = vec![
        ("project_id", Value::Uuid(project(1).uuid())),
        ("key", Value::String(key.to_owned())),
        ("target_day", Value::Date(at.date())),
        ("target_time", Value::Time(at)),
        ("trace_id", Value::Bytes(trace(1).to_bytes().to_vec())),
        ("version", Value::UInt(1)),
        ("fact_id", Value::Uuid(Uuid::from_bytes([0x11; 16]))),
        ("target_kind", Value::Enum(FactTargetKind::Trace.value())),
        ("author_kind", Value::Enum(2)),
        ("created_at", Value::Time(at)),
    ];
    set.extend(body.iter().cloned());
    world.store.insert(table, stored_row(table, &set));
}

/// One read of a wave finds the store unavailable: the answer is partial, and holds what reading the wave one
/// read after another gives. The scores, read before the tags, count; the flags, read after them, do not, as a
/// read the store refused leaves the reads after it unread. The trace is still in the recent index too, which
/// lets the answer be partial rather than an error.
#[test]
fn an_unavailable_read_of_a_wave_leaves_the_answers_after_it_out() {
    let mut world = World::new(100);
    let start = NOW.saturating_add_micros(-60_000_000);
    let lsn = world.ack(vec![
        span(1, 10, 0, start),
        span(1, 11, 10, start.saturating_add_micros(1_000)),
    ]);
    world.ship(lsn);
    world.store.slow_reads();
    put_fact(
        &world,
        TableId::FactScores,
        "quality",
        &[
            ("data_type", Value::Enum(ScoreDataType::Numeric.value())),
            ("value_num", Value::Float(0.5)),
            ("source", Value::Enum(1)),
        ],
    );
    put_fact(
        &world,
        TableId::FactFlags,
        "bookmarked",
        &[("value", Value::UInt(1))],
    );
    let whole = run(world.service.trace_by_id(&ctx(1), trace(1))).unwrap();
    assert_eq!(whole.facts.facts.scores.len(), 1);
    assert!(whole.facts.facts.bookmarked.is_some());
    assert!(!whole.meta.partial);

    world.store.set_unavailable_table(TableId::FactTags);
    let detail = run(world.service.trace_by_id(&ctx(1), trace(1))).unwrap();
    assert_eq!(detail.spans.len(), 2);
    assert_eq!(detail.facts.facts.scores.len(), 1);
    assert_eq!(detail.facts.facts.bookmarked, None);
    assert!(detail.meta.partial);
    assert_eq!(
        detail.meta.partial_reason,
        Some(PartialReason::StoreUnavailable)
    );
    // Every read of the wave was sent: 6 for each request.
    assert_eq!(world.store.executed().len(), 12);
}
