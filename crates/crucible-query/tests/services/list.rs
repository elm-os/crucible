//! The trace list, its live tail and data bounds.

use std::sync::Arc;
use std::time::Duration;

use crucible_plan::{Pred, Value};
use crucible_query::{
    Consistency, HeadCursor, MinuteRange, PageCursor, PartialReason, QueryServiceError,
    TraceListItem, TraceListPage, TraceListQuery, TraceTail, TraceTailQuery,
};
use crucible_recent::SpanEntry;
use crucible_records::{HeadKind, Lsn, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::TraceHeadsCol;

use crate::store::stored_row;
use crate::world::{DAY, NOW, World, ctx, project, run, span, trace};

const MS: i64 = 1_000;

/// The day that holds [`NOW`], plus a day either side.
pub fn window() -> TimeWindow {
    TimeWindow::new(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(DAY),
    )
    .unwrap()
}

fn at(ms: i64) -> Timestamp {
    NOW.saturating_add_micros(ms * MS)
}

fn page(world: &World, limit: u32, cursor: Option<PageCursor>) -> TraceListPage {
    let query = TraceListQuery {
        window: window(),
        filter: Pred::all(),
        limit: Some(limit),
        cursor,
    };
    run(world.service.trace_list(&ctx(1), query)).unwrap()
}

fn tail(world: &World, since: HeadCursor) -> TraceTail {
    let query = TraceTailQuery {
        window: window(),
        filter: Pred::all(),
        limit: Some(10),
        since,
    };
    run(world.service.trace_list_since(&ctx(1), query)).unwrap()
}

fn traces(items: &[TraceListItem]) -> Vec<TraceId> {
    items.iter().map(|item| item.head.trace_id).collect()
}

/// A span that is not a root and that stage B made the provisional head of its trace.
fn provisional(trace_n: u64, id: u64, start: Timestamp) -> SpanEntry {
    SpanEntry {
        head: Some(HeadKind::Provisional),
        ..span(trace_n, id, 999, start)
    }
}

fn named(mut entry: SpanEntry, trace_name: &str, user: &str) -> SpanEntry {
    entry.summary.trace_name = Arc::from(trace_name);
    entry.summary.user_id = Arc::from(user);
    entry
}

#[test]
fn the_first_page_merges_the_store_and_the_index_newest_first() {
    let mut world = World::new(100);
    // Trace 1 is in the store only, trace 2 in both, trace 3 in the index only.
    world.ack(vec![
        named(span(1, 10, 0, at(-30)), "checkout", "ada"),
        span(1, 11, 10, at(-29)),
    ]);
    world.settle();
    let lsn = world.ack(vec![span(2, 20, 0, at(-20))]);
    world.ship(lsn);
    world.checkpoint(lsn);
    world.ack(vec![
        named(span(3, 30, 0, at(-10)), "search", "bob"),
        span(3, 31, 30, at(-9)),
    ]);

    let first = page(&world, 10, None);
    assert_eq!(traces(&first.data), [trace(3), trace(2), trace(1)]);
    let recent: Vec<bool> = first.data.iter().map(|item| item.recent).collect();
    assert_eq!(recent, [true, false, false]);
    assert!(!first.has_more && first.next_cursor.is_none());
    assert!(!first.meta.partial && first.meta.consistency == Consistency::Strong);
    assert_eq!(first.meta.as_of, world_now(&world));

    // The merged aggregates and the resolved picks, from either source.
    let (newest, oldest) = (&first.data[0].summary, &first.data[2].summary);
    assert_eq!(
        (newest.span_count, &*newest.trace_name, &*newest.user_id),
        (2, "search", "bob")
    );
    assert_eq!(
        (oldest.span_count, &*oldest.trace_name, &*oldest.user_id),
        (2, "checkout", "ada")
    );
    assert_eq!((oldest.min_start, oldest.max_end), (at(-30), at(-28)));
    assert_eq!(
        (
            oldest.total_cost_nusd.nanos(),
            oldest.total_tokens,
            oldest.has_root
        ),
        (14, 6, true)
    );
    // Trace 2 is in both places, and its checkpoint has confirmed it: counted once.
    assert_eq!(first.data[1].summary.span_count, 1);
    // A store head carries the columns the index does not keep.
    assert_eq!(&*first.data[2].head.environment, "prod");
    assert_eq!(&*first.data[0].head.environment, "");
    assert_eq!(first.data[0].head.head_kind, HeadKind::Root);
}

fn world_now(world: &World) -> Timestamp {
    crucible_env::Clock::wall_now(&world.clock)
}

#[test]
fn a_provisional_head_is_shown_only_while_its_trace_has_no_root() {
    let mut world = World::new(100);
    // Trace 1: a child arrives first and becomes the provisional head. Trace 2: only ever a child.
    world.ack(vec![
        provisional(1, 11, at(-50)),
        provisional(2, 21, at(-40)),
    ]);
    world.settle();
    let both = page(&world, 10, None);
    assert_eq!(traces(&both.data), [trace(2), trace(1)]);
    assert!(
        both.data
            .iter()
            .all(|item| item.head.head_kind == HeadKind::Provisional)
    );
    assert!(!both.data[0].summary.has_root);

    // The root of trace 1 arrives, later than its child started. The provisional head is hidden, in the index
    // as in the store, and the trace is listed once, at its root.
    world.ack(vec![span(1, 10, 0, at(-45))]);
    let listed = page(&world, 10, None);
    assert_eq!(traces(&listed.data), [trace(2), trace(1)]);
    assert_eq!(listed.data[1].head.head_kind, HeadKind::Root);
    assert_eq!(listed.data[1].head.start_time, at(-45));
    assert!(listed.data[1].recent && listed.data[1].summary.has_root);
    assert_eq!(listed.data[1].summary.span_count, 2);
    world.settle();
    let listed = page(&world, 10, None);
    assert_eq!(traces(&listed.data), [trace(2), trace(1)]);
    assert!(listed.data.iter().all(|item| !item.recent));
}

#[test]
fn live_tail_returns_what_arrived_after_the_head_cursor() {
    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, at(-30))]);
    world.settle();
    let first = page(&world, 10, None);
    let cursor = first.head_cursor.unwrap();

    // Nothing new yet.
    let quiet = tail(&world, cursor);
    assert!(quiet.data.is_empty() && quiet.new_count == 0 && !quiet.has_more);

    // Two traces arrive; one backdated before trace 1, one after it.
    world.clock.advance(Duration::from_millis(5));
    world.ack(vec![span(2, 20, 0, at(-60))]);
    world.ack(vec![span(3, 30, 0, at(-5)), span(1, 11, 10, at(-4))]);
    let fresh = tail(&world, cursor);
    assert_eq!(traces(&fresh.data), [trace(3), trace(2)]);
    assert_eq!((fresh.new_count, fresh.has_more), (2, false));
    assert!(fresh.data.iter().all(|item| item.recent));

    // The same cursor gives the same answer once the store serves the traces and the index forgot them.
    world.settle();
    let settled = tail(&world, cursor);
    assert_eq!(traces(&settled.data), [trace(3), trace(2)]);
    assert!(settled.data.iter().all(|item| !item.recent));
    // The cursor of a tail response moves on, and page cursors are unaffected.
    assert!(tail(&world, fresh.head_cursor).data.is_empty());
    assert_ne!(fresh.head_cursor, cursor);
    assert_eq!(
        traces(&page(&world, 10, None).data),
        [trace(3), trace(1), trace(2)]
    );
}

#[test]
fn new_count_saturates_and_has_more_reports_the_overflow() {
    let mut world = World::new(1_000);
    let cursor = page(&world, 10, None).head_cursor.unwrap();
    world.clock.advance(Duration::from_millis(5));
    for n in 1..=120 {
        world.ack(vec![span(n, n, 0, at(n as i64))]);
    }
    let fresh = tail(&world, cursor);
    assert_eq!(
        (fresh.data.len(), fresh.new_count, fresh.has_more),
        (10, 100, true)
    );
    assert_eq!(fresh.data[0].head.trace_id, trace(120));
    world.settle();
    let settled = tail(&world, cursor);
    assert_eq!(
        (settled.data.len(), settled.new_count, settled.has_more),
        (10, 100, true)
    );
    assert_eq!(traces(&settled.data), traces(&fresh.data));
}

#[test]
fn a_filtered_list_reads_the_store_only_and_says_when_that_trails() {
    let mut world = World::new(100);
    world.ack(vec![named(span(1, 10, 0, at(-30)), "", "ada")]);
    world.ack(vec![named(span(2, 20, 0, at(-20)), "", "bob")]);
    world.settle();
    world.ack(vec![named(span(3, 30, 0, at(-10)), "", "ada")]);
    let filtered = |world: &World| {
        let query = TraceListQuery {
            window: window(),
            filter: Pred::eq(TraceHeadsCol::UserId, "ada"),
            limit: None,
            cursor: None,
        };
        run(world.service.trace_list(&ctx(1), query)).unwrap()
    };
    // The index keeps no column to filter on, so its head is left out and the response is marked.
    let listed = filtered(&world);
    assert_eq!(traces(&listed.data), [trace(1)]);
    assert_eq!(listed.meta.consistency, Consistency::Lagging);
    world.settle();
    let listed = filtered(&world);
    assert_eq!(traces(&listed.data), [trace(3), trace(1)]);
    assert_eq!(listed.meta.consistency, Consistency::Strong);
}

#[test]
fn cursors_are_bound_to_their_project_and_window() {
    let mut world = World::new(100);
    for n in 1..=3 {
        world.ack(vec![span(n, n, 0, at(n as i64))]);
    }
    let first = page(&world, 2, None);
    let (next, head) = (first.next_cursor.unwrap(), first.head_cursor.unwrap());
    assert!(first.has_more);
    let (_, _, as_of, last) = next.parts();
    assert_eq!(last, first.data[1].head.key());
    assert_eq!(
        head.parts(),
        (project(1), window(), as_of, world.index.applied())
    );

    let other_window = TimeWindow::new(window().from(), NOW).unwrap();
    let list = |cursor, window| {
        let query = TraceListQuery {
            window,
            filter: Pred::all(),
            limit: Some(2),
            cursor: Some(cursor),
        };
        run(world.service.trace_list(&ctx(1), query))
    };
    let foreign = PageCursor::from_parts(project(2), window(), as_of, last);
    assert_eq!(
        list(foreign, window()).unwrap_err().problem_code(),
        "cursor_invalid"
    );
    assert_eq!(
        list(next, other_window),
        Err(QueryServiceError::CursorInvalid)
    );
    assert_eq!(traces(&list(next, window()).unwrap().data), [trace(1)]);

    let since = |since, window| {
        let query = TraceTailQuery {
            window,
            filter: Pred::all(),
            limit: None,
            since,
        };
        run(world.service.trace_list_since(&ctx(1), query))
    };
    let foreign = HeadCursor::from_parts(project(2), window(), as_of, Lsn::ZERO);
    assert_eq!(
        since(foreign, window()),
        Err(QueryServiceError::CursorInvalid)
    );
    assert_eq!(
        since(head, other_window),
        Err(QueryServiceError::CursorInvalid)
    );
    assert!(since(head, window()).is_ok());

    for limit in [0, 201] {
        let query = TraceListQuery {
            window: window(),
            filter: Pred::all(),
            limit: Some(limit),
            cursor: None,
        };
        let refused = run(world.service.trace_list(&ctx(1), query));
        assert_eq!(refused, Err(QueryServiceError::Validation("limit")));
    }
}

#[test]
fn with_the_store_down_the_list_answers_from_the_index_or_fails() {
    let mut world = World::new(2);
    world.store.set_unavailable(true);
    let query = || TraceListQuery {
        window: window(),
        filter: Pred::all(),
        limit: None,
        cursor: None,
    };
    // Nothing in the index: the read cannot be answered.
    let failed = run(world.service.trace_list(&ctx(1), query())).unwrap_err();
    assert_eq!(failed.problem_code(), "telemetry_store_unavailable");
    let failed = run(world.service.data_bounds(&ctx(1))).unwrap_err();
    assert_eq!(failed.problem_code(), "telemetry_store_unavailable");

    for n in 1..=5 {
        world.ack(vec![span(n, n, 0, at(n as i64))]);
    }
    let listed = run(world.service.trace_list(&ctx(1), query())).unwrap();
    assert!(!listed.data.is_empty() && listed.data.len() < 5);
    assert_eq!(listed.data[0].head.trace_id, trace(5));
    assert_eq!(
        listed.meta.partial_reason,
        Some(PartialReason::StoreUnavailable)
    );
    assert!(!listed.has_more);

    // The store is back; the dropped traces are in neither place until the shipper catches up, and the open
    // gap keeps the list partial until then.
    world.store.set_unavailable(false);
    let listed = run(world.service.trace_list(&ctx(1), query())).unwrap();
    assert!(listed.data.len() < 5 && listed.meta.partial);
    world.settle();
    let listed = run(world.service.trace_list(&ctx(1), query())).unwrap();
    assert_eq!(listed.data.len(), 5);
    assert!(!listed.meta.partial);
}

#[test]
fn data_bounds_merge_the_rollups_with_the_index() {
    let mut world = World::new(100);
    let minute = |m: i64| NOW.floor_minute().saturating_add_micros(m * 60_000_000);
    let empty = run(world.service.data_bounds(&ctx(1))).unwrap();
    assert_eq!(
        (empty.spans, empty.mcp_client, empty.mcp_server),
        (None, None, None)
    );
    assert_eq!(empty.retention_horizon, None);

    let project_id = Value::Uuid(project(1).uuid());
    for m in [-90, -45] {
        let row = [
            ("project_id", project_id.clone()),
            ("minute", Value::Time(minute(m))),
        ];
        world
            .store
            .insert(TableId::SpanMetrics, stored_row(TableId::SpanMetrics, &row));
    }
    for (side, m) in [(1u8, -30), (1, -20), (2, -10)] {
        let row = [
            ("project_id", project_id.clone()),
            ("side", Value::Enum(side)),
            ("minute", Value::Time(minute(m))),
        ];
        world.store.insert(
            TableId::McpToolRollup,
            stored_row(TableId::McpToolRollup, &row),
        );
    }
    let range = |first, last| {
        Some(MinuteRange {
            first: minute(first),
            last: minute(last),
        })
    };
    let stored = run(world.service.data_bounds(&ctx(1))).unwrap();
    assert_eq!(stored.spans, range(-90, -45));
    assert_eq!(
        (stored.mcp_client, stored.mcp_server),
        (range(-30, -20), range(-10, -10))
    );

    // A span the rollup does not hold yet widens the bounds, at minute precision.
    world.ack(vec![span(1, 10, 0, NOW.saturating_add_micros(1))]);
    let merged = run(world.service.data_bounds(&ctx(1))).unwrap();
    assert_eq!(merged.spans, range(-90, 0));
    assert!(!merged.meta.partial);
    assert_eq!(run(world.service.data_bounds(&ctx(2))).unwrap().spans, None);
}
