//! A request sends its independent store reads together: the trace list its three fact lookups, trace by id its
//! spans with its four fact lookups. The fake store's reads take time here, so reads sent together are all in
//! flight before the first answers, and it records the most at once. Sending reads together changes no answer
//! and no read count.

use crucible_plan::Filter;
use crucible_query::TraceListQuery;

use crate::list::window;
use crate::world::{NOW, World, ctx, run, span, trace};

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
