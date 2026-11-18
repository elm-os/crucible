//! Properties: keyset pagination against a sorted-vector oracle, the retention clamp on merged output, and
//! read-your-ack under any interleaving of acks, shipping, checkpoints, outages and restarts.

use std::collections::BTreeMap;

use crucible_plan::{Caller, Pred};
use crucible_query::{
    Consistency, QueryServiceError, ReadContext, SpanChildrenQuery, TraceListQuery,
};
use crucible_recent::SpanEntry;
use crucible_records::{HeadKind, Lsn, SpanId, TimeWindow, Timestamp, TraceId};
use crucible_schema::RetentionSettings;
use proptest::prelude::*;

use crate::world::{DAY, GRACE, NOW, World, ctx, run, scoped, span, span_id, trace};

/// Where a frame's rows are when the read runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    /// In the store only: shipped, checkpointed and evicted from the index.
    Store,
    /// In both: shipped and checkpointed, still inside the visibility grace.
    Both,
    /// In the index only.
    Index,
}

fn place() -> impl Strategy<Value = Place> {
    prop_oneof![Just(Place::Store), Just(Place::Both), Just(Place::Index)]
}

/// Builds a node in which each span is where its `Place` says.
fn populate(world: &mut World, spans: &[(Place, SpanEntry)]) {
    let of = |place: Place| -> Vec<SpanEntry> {
        let placed = spans.iter().filter(|(p, _)| *p == place);
        placed.map(|(_, entry)| entry.clone()).collect()
    };
    // One frame per span, so that frames and checkpoints interleave like real traffic.
    for entry in of(Place::Store) {
        world.ack(vec![entry]);
    }
    world.settle();
    let mut last = Lsn::ZERO;
    for entry in of(Place::Both) {
        last = world.ack(vec![entry]);
    }
    if last > Lsn::ZERO {
        world.ship(last);
        world.checkpoint(last);
    }
    for entry in of(Place::Index) {
        world.ack(vec![entry]);
    }
}

fn at(ms: i64) -> Timestamp {
    NOW.saturating_add_micros(ms * 1_000)
}

/// The heads of one generated trace.
#[derive(Clone, Debug)]
struct TraceShape {
    /// A span that is not a root and arrives first, at this offset.
    provisional: Option<(i64, Place)>,
    /// The root span, at this offset.
    root: Option<(i64, Place)>,
}

fn trace_shape() -> impl Strategy<Value = TraceShape> {
    // Few distinct start times, so that heads tie on `start_time`.
    let head = || (0i64..6, place());
    prop_oneof![
        1 => head().prop_map(|root| TraceShape { provisional: None, root: Some(root) }),
        1 => head().prop_map(|p| TraceShape { provisional: Some(p), root: None }),
        // Weighted up: a provisional head that a root hides is the row the list over-reads for.
        4 => (head(), head()).prop_map(|(p, root)| TraceShape { provisional: Some(p), root: Some(root) }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Paging the trace list with any page size returns exactly the visible heads, newest first, once each:
    /// the list of an oracle that sorts a vector. Heads come from the store, the index or both; hidden
    /// provisional heads are dense; start times tie.
    #[test]
    fn trace_list_pages_equal_the_sorted_oracle(
        shapes in prop::collection::vec(trace_shape(), 0..40),
        limit in 1u32..8,
    ) {
        let mut spans = Vec::new();
        let mut oracle: Vec<(Timestamp, u32, TraceId, HeadKind)> = Vec::new();
        for (n, shape) in shapes.iter().enumerate() {
            let n = n as u64 + 1;
            if let Some((offset, place)) = shape.provisional {
                let entry = SpanEntry {
                    head: Some(HeadKind::Provisional),
                    ..span(n, n * 10 + 1, 999, at(offset))
                };
                spans.push((place, entry));
            }
            if let Some((offset, place)) = shape.root {
                // A root never arrives before the provisional head of its trace.
                let place = match (shape.provisional, place) {
                    (Some((_, Place::Index)), _) => Place::Index,
                    (Some((_, Place::Both)), Place::Store) => Place::Both,
                    (_, place) => place,
                };
                spans.push((place, span(n, n * 10, 0, at(offset))));
            }
            // The rule of contract 3.9, on the generated shape: a root head is shown; a provisional head only
            // when the trace has no root.
            let shown = match (shape.root, shape.provisional) {
                (Some((offset, _)), _) => (offset, HeadKind::Root),
                (None, Some((offset, _))) => (offset, HeadKind::Provisional),
                (None, None) => unreachable!("a trace has a head"),
            };
            oracle.push((at(shown.0), trace(n).trace_hash(), trace(n), shown.1));
        }
        oracle.sort_by(|a, b| b.cmp(a));
        let mut world = World::new(1_000);
        populate(&mut world, &spans);

        let window = crate::list::window();
        let mut listed = Vec::new();
        let mut cursor = None;
        for page_no in 0.. {
            prop_assert!(page_no <= oracle.len() + 2, "paging does not end");
            let query = TraceListQuery { window, filter: Pred::all(), limit: Some(limit), cursor };
            let page = run(world.service.trace_list(&ctx(1), query)).unwrap();
            prop_assert!(page.data.len() <= limit as usize);
            prop_assert_eq!(page.has_more, page.next_cursor.is_some());
            prop_assert_eq!(page.head_cursor.is_some(), cursor.is_none());
            prop_assert!(!page.meta.partial && page.meta.consistency == Consistency::Strong);
            for item in &page.data {
                let (start, hash, id) = item.head.key();
                listed.push((start, hash, id, item.head.head_kind));
                prop_assert_eq!(item.summary.has_root, item.head.head_kind == HeadKind::Root);
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        prop_assert_eq!(listed, oracle);
    }

    /// Paging the children of a span returns exactly its children in `(start_time, span_id)` order, once each,
    /// wherever each child is, with the number of its own children.
    #[test]
    fn span_children_pages_equal_the_sorted_oracle(
        children in prop::collection::vec((0i64..6, place(), prop::collection::vec(place(), 0..3)), 0..30),
        strangers in prop::collection::vec((0i64..6, place()), 0..6),
        limit in 1u32..8,
    ) {
        let mut spans = vec![(Place::Store, span(1, 1, 0, at(0)))];
        let mut oracle: Vec<(Timestamp, SpanId, u64)> = Vec::new();
        let mut next = 100u64;
        for (offset, place, grandchildren) in &children {
            let child = next;
            next += 1;
            spans.push((*place, span(1, child, 1, at(*offset))));
            oracle.push((at(*offset), span_id(child), grandchildren.len() as u64));
            for place in grandchildren {
                spans.push((*place, span(1, next, child, at(*offset + 1))));
                next += 1;
            }
        }
        // Spans with another parent, and children of the same span id in another trace.
        for (i, (offset, place)) in strangers.iter().enumerate() {
            let (trace_n, parent) = if i % 2 == 0 { (1, 2) } else { (2, 1) };
            spans.push((*place, span(trace_n, next, parent, at(*offset))));
            next += 1;
        }
        oracle.sort();
        let mut world = World::new(1_000);
        populate(&mut world, &spans);

        let mut listed = Vec::new();
        let mut cursor = None;
        for page_no in 0.. {
            prop_assert!(page_no <= oracle.len() + 1, "paging does not end");
            let query = SpanChildrenQuery { trace_id: trace(1), parent: span_id(1), cursor, limit: Some(limit) };
            let page = run(world.service.span_children(&ctx(1), query)).unwrap();
            prop_assert!(page.data.len() <= limit as usize);
            prop_assert_eq!(page.has_more, page.next_cursor.is_some());
            for child in &page.data {
                let s = &child.summary;
                listed.push((s.start_time, s.span_id, child.child_count.unwrap()));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        prop_assert_eq!(listed, oracle);
    }

    /// No read returns a row that retention has expired, whether the row is in the store, in the index or in
    /// both, and every row that has not expired is returned. Every plan that reached the store was clamped.
    #[test]
    fn expired_rows_are_hidden_from_every_read(
        ages in prop::collection::vec((0i64..40, 0i64..DAY, place()), 1..30),
        retention_days in prop::option::of(0u32..35),
    ) {
        // One trace per span, each a root, `age` days and a bit before now.
        let spans: Vec<(Place, SpanEntry)> = ages
            .iter()
            .enumerate()
            .map(|(n, (days, micros, place))| {
                let start = NOW.saturating_add_micros(-(days * DAY) - micros);
                (*place, span(n as u64 + 1, n as u64 + 1, 0, start))
            })
            .collect();
        let mut world = World::new(1_000);
        populate(&mut world, &spans);
        let cutoff = retention_days
            .map(|days| Timestamp::from_micros((NOW.day_number() - i64::from(days)) * DAY));
        let retained = |start: Timestamp| cutoff.is_none_or(|cutoff| start >= cutoff);
        let settings = RetentionSettings { retention_days, ..RetentionSettings::default() };
        let context = ReadContext::new(scoped(1), settings, Caller::Session);

        // Trace by id: present when retained, `not_found` when expired.
        for (_, entry) in &spans {
            let s = &entry.summary;
            let found = run(world.service.trace_by_id(&context, s.trace_id));
            if retained(s.start_time) {
                let found = found.unwrap();
                prop_assert_eq!(found.spans.len(), 1);
                prop_assert_eq!(&found.spans[0].summary, s);
            } else {
                prop_assert_eq!(found, Err(QueryServiceError::NotFound));
            }
        }

        // The trace list over a window that reaches far behind the horizon.
        let window = TimeWindow::new(NOW.saturating_add_micros(-60 * DAY), NOW.saturating_add_micros(DAY));
        let query = TraceListQuery { window: window.unwrap(), filter: Pred::all(), limit: Some(200), cursor: None };
        let page = run(world.service.trace_list(&context, query)).unwrap();
        let mut listed: Vec<TraceId> = page.data.iter().map(|item| item.head.trace_id).collect();
        let mut expected: Vec<TraceId> = spans
            .iter()
            .filter(|(_, entry)| retained(entry.summary.start_time))
            .map(|(_, entry)| entry.summary.trace_id)
            .collect();
        listed.sort();
        expected.sort();
        prop_assert_eq!(listed, expected);
        prop_assert!(page.data.iter().all(|item| retained(item.summary.min_start)));

        // Data bounds start at or after the horizon, which is the cut-off.
        let bounds = run(world.service.data_bounds(&context)).unwrap();
        prop_assert_eq!(bounds.retention_horizon, cutoff);
        if let Some(range) = bounds.spans {
            prop_assert!(cutoff.is_none_or(|cutoff| range.first >= cutoff.floor_minute()));
        }

        // Structural: no plan reached the store with a window that starts before the cut-off.
        let executed = world.store.executed();
        prop_assert!(!executed.is_empty());
        for plan in executed {
            prop_assert!(retained(plan.window().from()), "{:?}", plan.root().tables());
        }
    }
}

/// One step of a node's life.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// A client's span is acknowledged and stage B applies it.
    Ack,
    /// The shipper lands everything acknowledged so far, if the store is up.
    Ship,
    /// The checkpoint of everything shipped.
    Checkpoint,
    /// The visibility grace passes.
    Grace,
    /// The store goes down or comes back.
    Outage(bool),
    /// The node crashes and recovers.
    Restart,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => Just(Step::Ack),
        2 => Just(Step::Ship),
        2 => Just(Step::Checkpoint),
        2 => Just(Step::Grace),
        1 => any::<bool>().prop_map(Step::Outage),
        1 => Just(Step::Restart),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariant I3 through the service: after every step, a trace-by-id read of every acknowledged span
    /// returns it, or the response is marked partial, or the store is down and says so. It never answers
    /// `not_found`, and never answers whole without the span. The index is tiny, so outages open gaps.
    #[test]
    fn an_acknowledged_span_is_never_silently_missing(
        steps in prop::collection::vec(step(), 1..40),
        capacity in 1u64..4,
    ) {
        let mut world = World::new(capacity);
        let mut acked: BTreeMap<u64, Lsn> = BTreeMap::new();
        let (mut shipped, mut down) = (Lsn::ZERO, false);
        for (i, step) in steps.iter().enumerate() {
            match step {
                Step::Ack => {
                    let n = acked.len() as u64 + 1;
                    acked.insert(n, world.ack(vec![span(n, n, 0, NOW)]));
                }
                Step::Ship if !down => {
                    shipped = acked.values().copied().max().unwrap_or(Lsn::ZERO);
                    world.ship(shipped);
                }
                Step::Ship => {}
                Step::Checkpoint => world.checkpoint(shipped),
                Step::Grace => {
                    world.clock.advance(GRACE);
                    world.writer.maintain();
                }
                Step::Outage(now_down) => {
                    down = *now_down;
                    world.store.set_unavailable(down);
                }
                Step::Restart => world.restart(),
            }
            for (&n, &lsn) in &acked {
                match run(world.service.trace_by_id(&ctx(1), trace(n))) {
                    Ok(found) => {
                        let present = found.spans.iter().any(|s| s.summary.span_id == span_id(n));
                        prop_assert!(present || found.meta.partial, "step {i}: lsn {lsn} is silently missing");
                        prop_assert_eq!(found.meta.consistency, Consistency::Strong);
                        // With the store up and this span shipped, it is there whatever the gaps.
                        prop_assert!(present || down || lsn > shipped, "step {i}: lsn {lsn} is in the store");
                    }
                    Err(error) => {
                        prop_assert!(down, "step {i}: {error} with the store up");
                        prop_assert_eq!(error.problem_code(), "telemetry_store_unavailable");
                    }
                }
            }
        }
        // The store returns, the shipper catches up, the grace passes: every span is back, whole.
        world.store.set_unavailable(false);
        world.settle();
        for &n in acked.keys() {
            let found = run(world.service.trace_by_id(&ctx(1), trace(n))).unwrap();
            prop_assert_eq!(found.spans.len(), 1);
            prop_assert!(!found.meta.partial);
        }
    }
}
