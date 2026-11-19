//! Typed reads under the pending-deletion set (contract 13.4): from the moment a deletion request is accepted,
//! no read returns what it selects, whether the span is still in the recent index or already in the store,
//! while what arrived after the request stays readable.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_plan::{Filter, PayloadField};
use crucible_query::{
    Hidden, HiddenSelector, HiddenSource, PayloadQuery, QueryServiceError, TraceListQuery,
};
use crucible_records::{ProjectId, SpanId, TableId, TraceId};

use crate::list::window;
use crate::world::{NOW, World, ctx, project, run, span, span_id, span_payload_row, trace};

/// A pending-deletion set a test fills in.
#[derive(Debug, Default)]
struct Pending(Mutex<Vec<(ProjectId, Hidden)>>);

impl Pending {
    fn set(&self, hidden: Vec<(ProjectId, Hidden)>) {
        *self.0.lock().unwrap() = hidden;
    }
}

impl HiddenSource for Pending {
    fn hidden(&self, project_id: ProjectId) -> Vec<Hidden> {
        let all = self.0.lock().unwrap();
        all.iter()
            .filter(|(project, _)| *project == project_id)
            .map(|(_, hidden)| hidden.clone())
            .collect()
    }
}

fn node() -> (World, Arc<Pending>) {
    let mut world = World::new(100);
    let pending = Arc::new(Pending::default());
    world.service = world
        .service
        .clone()
        .with_hidden(Arc::clone(&pending) as Arc<dyn HiddenSource>);
    (world, pending)
}

fn spans_of(world: &World, trace_n: u64) -> Result<Vec<SpanId>, QueryServiceError> {
    run(world.service.trace_by_id(&ctx(1), trace(trace_n)))
        .map(|detail| detail.spans.iter().map(|s| s.summary.span_id).collect())
}

fn listed(world: &World) -> Vec<TraceId> {
    let query = TraceListQuery {
        window: window(),
        filter: Filter::all(),
        limit: Some(10),
        cursor: None,
    };
    let page = run(world.service.trace_list(&ctx(1), query)).unwrap();
    let mut traces: Vec<TraceId> = page.data.iter().map(|item| item.head.trace_id).collect();
    traces.sort_unstable();
    traces
}

fn by_trace(trace_n: u64) -> Hidden {
    Hidden {
        selector: HiddenSelector::Traces(vec![trace(trace_n)]),
        requested_at: NOW.saturating_add_micros(1_000_000),
    }
}

#[test]
fn a_pending_deletion_hides_its_trace_in_the_index_and_in_the_store() {
    let (mut world, pending) = node();
    world.ack(vec![span(1, 10, 0, NOW), span(1, 11, 10, NOW)]);
    world.ack(vec![span(2, 20, 0, NOW)]);
    assert_eq!(spans_of(&world, 1).unwrap().len(), 2);
    assert_eq!(listed(&world).len(), 2);

    // Another project's request changes nothing here.
    pending.set(vec![(project(2), by_trace(1))]);
    assert_eq!(spans_of(&world, 1).unwrap().len(), 2);

    // The spans are still in the recent index only.
    pending.set(vec![(project(1), by_trace(1))]);
    assert_eq!(spans_of(&world, 1), Err(QueryServiceError::NotFound));
    assert_eq!(listed(&world), [trace(2)]);
    assert_eq!(spans_of(&world, 2).unwrap(), [span_id(20)]);

    // The store serves them now: the plans carry the request.
    world.settle();
    assert_eq!(world.index.stats().entries, 0);
    assert_eq!(spans_of(&world, 1), Err(QueryServiceError::NotFound));
    assert_eq!(listed(&world), [trace(2)]);

    // What arrives after the request is new data: its head is listed, from the index and from the store.
    world.clock.advance(Duration::from_secs(5));
    world.ack(vec![span(1, 12, 0, NOW)]);
    assert_eq!(listed(&world), [trace(1), trace(2)]);
    world.settle();
    assert_eq!(listed(&world), [trace(1), trace(2)]);

    // Without the request everything is there again: reads hide, they do not delete.
    pending.set(Vec::new());
    assert_eq!(spans_of(&world, 1).unwrap().len(), 3);
    assert_eq!(listed(&world).len(), 2);
}

#[test]
fn a_pending_deletion_by_user_or_session_hides_the_spans_that_carry_it() {
    let (mut world, pending) = node();
    let mut ada = span(1, 10, 0, NOW);
    ada.summary.user_id = Arc::from("ada");
    let mut checkout = span(2, 20, 0, NOW);
    checkout.summary.session_id = Arc::from("s-1");
    world.ack(vec![ada, checkout, span(3, 30, 0, NOW)]);
    let request = |selector| Hidden {
        selector,
        requested_at: NOW.saturating_add_micros(1_000_000),
    };
    for settled in [false, true] {
        if settled {
            world.settle();
        }
        pending.set(vec![(
            project(1),
            request(HiddenSelector::User("ada".to_owned())),
        )]);
        assert_eq!(listed(&world), [trace(2), trace(3)], "settled: {settled}");
        assert_eq!(spans_of(&world, 1), Err(QueryServiceError::NotFound));
        pending.set(vec![
            (project(1), request(HiddenSelector::User("ada".to_owned()))),
            (
                project(1),
                request(HiddenSelector::Session("s-1".to_owned())),
            ),
        ]);
        assert_eq!(listed(&world), [trace(3)], "settled: {settled}");
        // A selector that names nothing hides nothing.
        pending.set(vec![(
            project(1),
            request(HiddenSelector::User(String::new())),
        )]);
        assert_eq!(listed(&world).len(), 3, "settled: {settled}");
    }
}

#[test]
fn a_payload_of_a_trace_under_a_pending_deletion_is_not_found() {
    let (mut world, pending) = node();
    let entry = span(1, 10, 0, NOW.saturating_add_micros(-1_500));
    let lsn = world.ack(vec![entry.clone()]);
    world.store.insert(
        TableId::SpanPayload,
        span_payload_row(&entry, lsn, NOW, "{\"q\":1}", &[]),
    );
    world.settle();
    let query = || PayloadQuery {
        trace_id: entry.summary.trace_id,
        span_id: entry.summary.span_id,
        start_time: entry.summary.start_time,
        field: PayloadField::Input,
        range: None,
    };
    let read = |world: &World| run(world.service.payload(&ctx(1), query()));
    assert_eq!(read(&world).unwrap().bytes, b"{\"q\":1}");
    pending.set(vec![(project(1), by_trace(1))]);
    assert_eq!(read(&world).err(), Some(QueryServiceError::NotFound));
    pending.set(vec![(project(1), by_trace(2))]);
    assert_eq!(read(&world).unwrap().bytes, b"{\"q\":1}");
}
