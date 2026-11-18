//! Trace by id and span children.

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crucible_env::QueryError;
use crucible_plan::{TRACE_INLINE_SPAN_LIMIT, TRACE_PAGED_SPAN_LIMIT};
use crucible_query::{
    ChildCursor, Consistency, PartialReason, QueryServiceError, SpanChildrenQuery, SpanSource,
    TraceDetail, TraceSpan,
};
use crucible_recent::{Frame, QuarantineReach, QuarantinedPair};
use crucible_records::{Lsn, PartitionId, SpanId, TableId};

use crate::world::{GRACE, NOW, World, ctx, project, run, span, span_id, trace};

fn ids(spans: &[TraceSpan]) -> Vec<SpanId> {
    spans.iter().map(|s| s.summary.span_id).collect()
}

fn from_store(span: &TraceSpan) -> bool {
    matches!(span.source, SpanSource::Store(_))
}

fn detail(world: &World, trace_n: u64) -> Result<TraceDetail, QueryServiceError> {
    run(world.service.trace_by_id(&ctx(1), trace(trace_n)))
}

fn whole(detail: &TraceDetail) -> bool {
    !detail.meta.partial && detail.meta.consistency == Consistency::Strong
}

#[test]
fn an_acked_span_is_readable_from_the_index_and_then_from_the_store() {
    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, NOW), span(1, 11, 10, NOW)]);
    let found = detail(&world, 1).unwrap();
    assert_eq!(ids(&found.spans), [span_id(10), span_id(11)]);
    assert!(whole(&found) && !found.paged && found.span_count == 2);
    assert_eq!(found.meta.as_of, NOW);
    // The index holds no preview: a recent span points at its WAL frame instead.
    assert!(
        found
            .spans
            .iter()
            .all(|s| !from_store(s) && s.child_count.is_none())
    );

    world.settle();
    assert_eq!(world.index.stats().entries, 0);
    let found = detail(&world, 1).unwrap();
    assert_eq!(ids(&found.spans), [span_id(10), span_id(11)]);
    assert!(whole(&found) && found.span_count == 2);
    assert!(found.spans.iter().all(from_store));
    assert_eq!(found.spans[1].summary, span(1, 11, 10, NOW).summary);
}

#[test]
fn every_acked_span_is_readable_after_a_restart() {
    let mut world = World::new(100);
    // LSN 1 is shipped, checkpointed and aged out of the index; LSN 2 is shipped and checkpointed but still
    // inside the grace; LSN 4 is acked and applied; LSN 5 is acked, and the crash comes before stage B.
    let first = world.ack(vec![span(1, 10, 0, NOW)]);
    world.ship(first);
    world.checkpoint(first);
    world.clock.advance(GRACE);
    world.writer.maintain();
    let second = world.ack(vec![span(2, 20, 0, NOW)]);
    world.ship(second);
    world.checkpoint(second);
    world.ack(vec![span(3, 30, 0, NOW)]);
    world.ack_only(vec![span(4, 40, 0, NOW), span(3, 31, 30, NOW)]);

    world.restart();
    assert!(world.index.caught_up());
    for (trace_n, expected, stored) in [
        (1, vec![10], true),
        (2, vec![20], true),
        (3, vec![30, 31], false),
        (4, vec![40], false),
    ] {
        let found = detail(&world, trace_n).unwrap();
        let expected: Vec<SpanId> = expected.into_iter().map(span_id).collect();
        assert_eq!(ids(&found.spans), expected, "trace {trace_n}");
        assert!(whole(&found), "trace {trace_n}");
        assert!(
            found.spans.iter().all(|s| from_store(s) == stored),
            "trace {trace_n}"
        );
    }
    // A second restart from the same log gives the same answers.
    world.restart();
    assert_eq!(
        ids(&detail(&world, 3).unwrap().spans),
        [span_id(30), span_id(31)]
    );
}

#[test]
fn a_span_in_both_the_index_and_the_store_is_returned_once() {
    let mut world = World::new(100);
    let lsn = world.ack(vec![
        span(1, 10, 0, NOW),
        span(1, 11, 10, NOW),
        span(1, 12, 10, NOW),
    ]);
    // Shipped but not checkpointed: all three are in both places.
    world.ship(lsn);
    assert_eq!(world.index.stats().entries, 3);
    let found = detail(&world, 1).unwrap();
    assert_eq!(ids(&found.spans), [span_id(10), span_id(11), span_id(12)]);
    assert_eq!(found.span_count, 3);
    assert!(
        found.spans.iter().all(from_store),
        "the store's full row wins"
    );

    // A fourth span that only the index has, then the checkpoint inside the grace: still one of each.
    world.ack(vec![span(1, 13, 11, NOW)]);
    world.checkpoint(lsn);
    let found = detail(&world, 1).unwrap();
    assert_eq!(
        ids(&found.spans),
        [span_id(10), span_id(11), span_id(12), span_id(13)]
    );
    assert_eq!(found.span_count, 4);
    assert_eq!(found.spans.iter().filter(|s| from_store(s)).count(), 3);
    assert!(whole(&found));
}

#[test]
fn a_dropped_span_makes_the_read_partial_until_the_store_serves_it() {
    // Room for two entries, and the store is down, so nothing ships and nothing is evictable.
    let mut world = World::new(2);
    world.store.set_unavailable(true);
    for n in 1..=5 {
        world.ack(vec![span(n, n * 10, 0, NOW)]);
    }
    assert!(world.index.stats().entries < 5);
    let held = world.index.trace(ctx(1).scope(), trace(5)).0.len();
    let dropped = world.index.trace(ctx(1).scope(), trace(1)).0.len();
    assert_eq!((held, dropped), (1, 0));

    // The store is down. A trace the index holds is answered from it, marked partial. A trace the index
    // dropped cannot be answered at all, which is an error, never an empty success.
    let found = detail(&world, 5).unwrap();
    assert_eq!(ids(&found.spans), [span_id(50)]);
    assert_eq!(
        found.meta.partial_reason,
        Some(PartialReason::StoreUnavailable)
    );
    let error = detail(&world, 1).unwrap_err();
    assert_eq!(error.problem_code(), "telemetry_store_unavailable");

    // The store is back but the shipper has not caught up. The dropped trace is in neither place: the open
    // gap turns what would be `not_found` into an empty, partial answer.
    world.store.set_unavailable(false);
    let found = detail(&world, 1).unwrap();
    assert!(found.spans.is_empty() && found.meta.partial);
    assert_eq!(
        found.meta.partial_reason,
        Some(PartialReason::StoreUnavailable)
    );
    // So is a trace that never existed: the read could touch the gap. Another project is not affected.
    assert!(detail(&world, 99).unwrap().meta.partial);
    let other = run(world.service.trace_by_id(&ctx(2), trace(1)));
    assert_eq!(other, Err(QueryServiceError::NotFound));

    // Shipped, checkpointed and past the grace: the gap closes and every trace is whole again.
    world.settle();
    for n in 1..=5 {
        let found = detail(&world, n).unwrap();
        assert_eq!(ids(&found.spans), [span_id(n * 10)]);
        assert!(whole(&found), "trace {n}");
    }
    assert_eq!(detail(&world, 99), Err(QueryServiceError::NotFound));
}

fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn the_read_barrier_waits_250_ms_for_stage_b_and_then_answers_lagging() {
    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, NOW)]);
    // Acked, and stage B is stuck.
    world.ack_only(vec![span(1, 11, 10, NOW)]);
    let context = ctx(1);
    {
        let mut read = pin!(world.service.trace_by_id(&context, trace(1)));
        assert!(poll(read.as_mut()).is_pending());
        world.clock.advance(Duration::from_millis(249));
        assert!(poll(read.as_mut()).is_pending());
        assert!(
            world.store.executed().is_empty(),
            "no store read before the barrier resolves"
        );
        world.clock.advance(Duration::from_millis(1));
        let Poll::Ready(Ok(found)) = poll(read.as_mut()) else {
            panic!("the barrier has expired");
        };
        // The acked span is missing, and the response says it may trail.
        assert_eq!(ids(&found.spans), [span_id(10)]);
        assert_eq!(found.meta.consistency, Consistency::Lagging);
        assert!(!found.meta.partial);
    }
    // A trace whose only span is not applied yet is not `not_found`: the answer is empty and lagging.
    world.ack_only(vec![span(2, 20, 0, NOW)]);
    {
        let mut read = pin!(world.service.trace_by_id(&context, trace(2)));
        assert!(poll(read.as_mut()).is_pending());
        world.clock.advance(Duration::from_millis(250));
        let Poll::Ready(Ok(found)) = poll(read.as_mut()) else {
            panic!("the barrier has expired");
        };
        assert!(found.spans.is_empty());
        assert_eq!(found.meta.consistency, Consistency::Lagging);
    }
    // Stage B catches up inside the barrier: the read resumes at once and is strong.
    let service = world.service.clone();
    let mut read = pin!(service.trace_by_id(&context, trace(2)));
    assert!(poll(read.as_mut()).is_pending());
    world.clock.advance(Duration::from_millis(100));
    world.apply_pending();
    let Poll::Ready(Ok(found)) = poll(read.as_mut()) else {
        panic!("stage B has applied the frame");
    };
    assert_eq!(ids(&found.spans), [span_id(20)]);
    assert!(whole(&found));
    assert!(whole(&detail(&world, 1).unwrap()));
}

/// The index is read before the store. While the first store read is in flight, the shipper lands the span,
/// its checkpoint is applied, the grace passes and the index evicts the entry: a read that looked in the store
/// first and in the index second would find the span in neither.
#[test]
fn an_entry_evicted_during_the_store_read_is_still_returned() {
    let mut world = World::new(100);
    let lsn = world.ack(vec![span(1, 10, 0, NOW)]);
    let service = world.service.clone();
    let world = Mutex::new(world);
    let node = Arc::new(world);
    let during = Arc::clone(&node);
    node.lock()
        .unwrap()
        .store
        .clone()
        .during_next_read(move |_| {
            // The store read has computed its (empty) answer. Now the span moves from the index to the store.
            let mut world = during.lock().unwrap();
            world.ship(lsn);
            world.checkpoint(lsn);
            world.clock.advance(GRACE);
            world.writer.maintain();
            assert_eq!(world.index.stats().entries, 0);
        });
    let found = run(service.trace_by_id(&ctx(1), trace(1))).unwrap();
    assert_eq!(ids(&found.spans), [span_id(10)]);
    assert!(!found.meta.partial);
    // Afterwards the store serves it.
    let found = run(service.trace_by_id(&ctx(1), trace(1))).unwrap();
    assert!(found.spans.iter().all(from_store) && found.spans.len() == 1);
}

#[test]
fn a_quarantined_pair_makes_the_read_partial_until_it_is_requeued() {
    let mut world = World::new(100);
    let lsn = world.ack(vec![span(1, 10, 0, NOW)]);
    let day = PartitionId::date(NOW.date());
    // The span's `span_core` pair is quarantined: it counts as resolved, the checkpoint passes it, and the
    // entry leaves the index while the store has no row.
    world.writer.apply(Frame::Quarantine {
        lsn: Lsn::new(lsn.get() + 100),
        pairs: vec![QuarantinedPair {
            batch_seq: lsn,
            table: TableId::SpanCore,
            partition: day,
            reach: QuarantineReach::Projects(vec![project(1)]),
        }],
    });
    world.writer.apply(Frame::Checkpoint {
        lsn: Lsn::new(lsn.get() + 101),
        last_lsn: lsn,
    });
    world.clock.advance(GRACE);
    world.writer.maintain();
    world.wal.set_durable(world.index.applied());
    assert_eq!(world.index.stats().entries, 0);

    let found = detail(&world, 1).unwrap();
    assert!(found.spans.is_empty());
    assert_eq!(found.meta.partial_reason, Some(PartialReason::Quarantined));
    assert_eq!(found.meta.consistency, Consistency::Strong);
    assert_eq!(
        run(world.service.trace_by_id(&ctx(2), trace(1))),
        Err(QueryServiceError::NotFound)
    );

    // The operator requeues the pair: it lands, and the read is whole.
    world.ship(lsn);
    world.writer.requeued(lsn, TableId::SpanCore, day);
    let found = detail(&world, 1).unwrap();
    assert_eq!(ids(&found.spans), [span_id(10)]);
    assert!(whole(&found));
}

#[test]
fn a_trace_is_visible_only_inside_its_project() {
    let mut world = World::new(100);
    let lsn = world.ack(vec![span(1, 10, 0, NOW)]);
    world.ship(lsn);
    assert!(detail(&world, 1).is_ok());
    // In the index and in the store, and still unknown to another project.
    assert_eq!(
        run(world.service.trace_by_id(&ctx(2), trace(1))),
        Err(QueryServiceError::NotFound)
    );
    let children = SpanChildrenQuery {
        trace_id: trace(1),
        parent: span_id(10),
        cursor: None,
        limit: None,
    };
    assert_eq!(
        run(world.service.span_children(&ctx(2), children)),
        Err(QueryServiceError::NotFound)
    );
    // Every plan the service ran carried the caller's scope.
    assert!(!world.store.executed().is_empty());
}

#[test]
fn errors_map_to_the_problem_codes_of_the_contract() {
    let codes = [
        (QueryServiceError::NotFound, "not_found"),
        (QueryServiceError::CursorInvalid, "cursor_invalid"),
        (QueryServiceError::Validation("limit"), "validation_failed"),
        (QueryError::Timeout.into(), "query_timeout"),
        (QueryError::MemoryExceeded.into(), "query_memory_exceeded"),
        (QueryError::TooBroad.into(), "query_too_broad"),
        (
            QueryError::Unavailable(String::new()).into(),
            "telemetry_store_unavailable",
        ),
        (QueryError::Internal(String::new()).into(), "internal_error"),
        (QueryServiceError::Internal(String::new()), "internal_error"),
    ];
    for (error, code) in codes {
        assert_eq!(error.problem_code(), code);
    }
    for reason in [
        PartialReason::StoreUnavailable,
        PartialReason::Deadline,
        PartialReason::Quarantined,
    ] {
        assert!(["store_unavailable", "deadline", "quarantined"].contains(&reason.as_str()));
    }

    let mut world = World::new(100);
    world.ack(vec![span(1, 10, 0, NOW)]);
    let children = |limit, cursor| {
        let query = SpanChildrenQuery {
            trace_id: trace(1),
            parent: span_id(10),
            cursor,
            limit,
        };
        run(world.service.span_children(&ctx(1), query))
    };
    assert_eq!(
        children(Some(0), None),
        Err(QueryServiceError::Validation("limit"))
    );
    assert_eq!(
        children(Some(2_001), None),
        Err(QueryServiceError::Validation("limit"))
    );
    // A cursor of another project, trace or parent is refused.
    let last = (NOW, span_id(1));
    for cursor in [
        ChildCursor::from_parts(project(2), trace(1), span_id(10), last),
        ChildCursor::from_parts(project(1), trace(2), span_id(10), last),
        ChildCursor::from_parts(project(1), trace(1), span_id(11), last),
    ] {
        assert_eq!(
            children(None, Some(cursor)),
            Err(QueryServiceError::CursorInvalid)
        );
    }
    let own = ChildCursor::from_parts(project(1), trace(1), span_id(10), last);
    assert_eq!(own.parts(), (project(1), trace(1), span_id(10), last));
    assert!(children(None, Some(own)).unwrap().data.is_empty());
}

#[test]
fn a_trace_above_the_inline_limit_is_paged_from_its_roots() {
    let limit = u64::from(TRACE_INLINE_SPAN_LIMIT);
    let mut world = World::new(10_000);
    // One root with `limit - 1` children, in the store: exactly at the limit, so returned whole.
    let mut spans = vec![span(1, 1, 0, NOW)];
    spans.extend((2..=limit).map(|n| span(1, n, 1, NOW.saturating_add_micros(n as i64))));
    world.ack(spans);
    world.settle();
    let found = detail(&world, 1).unwrap();
    assert!(!found.paged && found.span_count == limit && found.spans.len() as u64 == limit);

    // One more child, which only the index has: above the limit.
    world.ack(vec![span(1, limit + 1, 1, NOW.saturating_add_micros(1))]);
    let found = detail(&world, 1).unwrap();
    assert!(found.paged && whole(&found));
    assert_eq!(found.span_count, limit + 1);
    assert_eq!(found.spans.len(), TRACE_PAGED_SPAN_LIMIT as usize);
    // The root first, then its earliest children; the recent child sorts first among them.
    assert_eq!(found.spans[0].summary.span_id, span_id(1));
    assert_eq!(found.spans[0].child_count, Some(limit));
    assert_eq!(found.spans[1].summary.span_id, span_id(limit + 1));
    assert!(!from_store(&found.spans[1]));
    assert!(found.spans[1..].iter().all(|s| s.child_count == Some(0)));
    assert!(found.spans.windows(2).all(|pair| {
        let key = |s: &TraceSpan| (s.summary.start_time, s.summary.span_id);
        key(&pair[0]) < key(&pair[1])
    }));

    // The rest is reached through pages of children.
    let page = run(world.service.span_children(
        &ctx(1),
        SpanChildrenQuery {
            trace_id: trace(1),
            parent: span_id(1),
            cursor: None,
            limit: Some(2_000),
        },
    ))
    .unwrap();
    assert!(page.has_more && page.data.len() == 2_000 && page.next_cursor.is_some());
    assert_eq!(page.data[0].summary.span_id, span_id(limit + 1));
}
