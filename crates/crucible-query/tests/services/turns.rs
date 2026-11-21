//! The metrics queries one project runs at once (`metrics.queries_per_project`): a dashboard sends one query per
//! widget, and the queries past the limit wait for a turn within their deadline instead of failing. The fake
//! store's reads take time here, so it records how many were in flight at once.

use std::pin::pin;
use std::time::Duration;

use crucible_env::QueryError;
use crucible_plan::{
    Aggregation, Fill, Filter, Granularity, MeasureId, MetricsQuery, Source, Step,
};
use crucible_query::{MetricsRequest, QueryServiceError, ReadContext};
use crucible_records::TimeWindow;
use futures::FutureExt as _;

use crate::world::{NOW, World, ctx, run};

/// The default of `metrics.queries_per_project`.
const PER_PROJECT: usize = 4;

/// A widget's query: the span count over the hour that ends at [`NOW`].
fn widget() -> MetricsRequest {
    MetricsRequest {
        window: TimeWindow::new(NOW.saturating_add_micros(-3_600_000_000), NOW).unwrap(),
        query: MetricsQuery {
            source: Source::Spans,
            measures: vec![(MeasureId::Count, Aggregation::Sum)],
            dimensions: Vec::new(),
            filter: Filter::all(),
            granularity: Granularity::Step(Step::FiveMinutes),
            fill: Fill::Zero,
            top_n: None,
        },
    }
}

/// The widget queries of a dashboard, all sent at once.
fn dashboard(
    world: &World,
    ctx: &ReadContext,
    widgets: usize,
) -> Vec<Result<(), QueryServiceError>> {
    let queries =
        (0..widgets).map(|_| async { world.service.metrics(ctx, widget()).await.map(|_| ()) });
    run(futures::future::join_all(queries))
}

#[test]
fn a_dashboard_runs_at_most_the_limit_of_queries_at_once_and_the_rest_wait() {
    let world = World::new(10);
    world.store.slow_reads();
    let answers = dashboard(&world, &ctx(1), 12);
    assert!(answers.iter().all(Result::is_ok), "{answers:?}");
    assert_eq!(world.store.executed().len(), 12);
    assert_eq!(world.store.most_in_flight(), PER_PROJECT);
}

#[test]
fn the_limit_is_a_setting() {
    let mut world = World::new(10);
    world.service = world.service.clone().with_metrics_per_project(2);
    world.store.slow_reads();
    let answers = dashboard(&world, &ctx(1), 7);
    assert!(answers.iter().all(Result::is_ok), "{answers:?}");
    assert_eq!(world.store.most_in_flight(), 2);
}

/// A query whose project's turns stay taken until its deadline (the analytics class time plus 2 s) ends as
/// `query_timeout`, not as unavailable. Another project's query does not wait at all.
#[test]
fn a_query_that_gets_no_turn_by_its_deadline_times_out_and_other_projects_do_not_wait() {
    let mut world = World::new(10);
    world.service = world.service.clone().with_metrics_per_project(1);
    world.store.hold_reads();
    let (one, two) = (ctx(1), ctx(2));
    let mut first = pin!(world.service.metrics(&one, widget()));
    assert!(first.as_mut().now_or_never().is_none());
    assert_eq!(world.store.in_flight(), 1);
    let mut second = pin!(world.service.metrics(&one, widget()));
    assert!(second.as_mut().now_or_never().is_none());
    assert_eq!(
        world.store.in_flight(),
        1,
        "the second query waits for a turn"
    );
    let mut other = pin!(world.service.metrics(&two, widget()));
    assert!(other.as_mut().now_or_never().is_none());
    assert_eq!(
        world.store.in_flight(),
        2,
        "another project's query reads at once"
    );

    world
        .clock
        .advance(Duration::from_secs(12) - Duration::from_nanos(1));
    assert!(
        second.as_mut().now_or_never().is_none(),
        "before the deadline"
    );
    world.clock.advance(Duration::from_nanos(1));
    let late = second
        .as_mut()
        .now_or_never()
        .expect("the deadline has passed");
    assert_eq!(
        late.map(|_| ()),
        Err(QueryServiceError::Store(QueryError::Timeout))
    );
    world.store.release_reads();
    assert!(run(first).is_ok());
    assert!(run(other).is_ok());
}
