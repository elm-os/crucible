//! The read-path checks of facts and metrics at quiescence (invariants I4 and I7), through the real
//! `QueryService`:
//!
//! - **I4** every target that has fact rows reads back, through `QueryService::facts`, exactly the facts the
//!   reference fold of `system_oracle::resolve` picks over the store's rows: the same winning fact for every
//!   key, and no value for a retracted key.
//! - **I7** without a quarantine, the metrics query over the window of the run's spans returns, per project,
//!   the span count, the error count, the token sums and the cost sum of the project's `span_core` rows (the
//!   sums without the usage aggregates), and the `traces` source counts the root heads of `trace_heads`.

use std::collections::{BTreeMap, BTreeSet};

use crucible_plan::{
    Aggregation, Caller, Fill, Filter, Granularity, MeasureId, MetricsQuery, Source,
};
use crucible_query::{FactTarget, MetricValue, MetricsRequest, ReadContext};
use crucible_records::{FactId, ProjectId, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::RetentionSettings;

use super::{Node, START_BASE_MICROS, Shared, Violation};
use crate::ch_model::{ChModel, i8_at, i64_at, u8_at, u64_at, uuid_at};
use crate::runtime::SIM_EPOCH_MICROS;
use crate::system_oracle::{self, FactRow};

/// `Level::Error` as `span_core` stores it.
const LEVEL_ERROR: i8 = 3;
/// `HeadKind::Root` as `trace_heads` stores it.
const ROOT: i8 = 1;

fn context(project: [u8; 16]) -> Option<ReadContext> {
    let scope = crucible_auth::test_support::scope(ProjectId::from_bytes(project)).ok()?;
    Some(ReadContext::new(
        scope,
        RetentionSettings::default(),
        Caller::Session,
    ))
}

/// The target of a fact row: a span when it names one, a trace when it names only a trace, else a session.
fn target(row: &FactRow) -> FactTarget {
    let (trace, span) = (TraceId::from_bytes(row.trace), SpanId::from_bytes(row.span));
    if span != SpanId::ZERO {
        FactTarget::Span(trace, span)
    } else if trace != TraceId::ZERO {
        FactTarget::Trace(trace)
    } else {
        FactTarget::Session(String::from_utf8_lossy(&row.session).into())
    }
}

/// A fact target as the oracle's rows name it: project, trace, span and session.
type Target = ([u8; 16], [u8; 16], [u8; 8], Vec<u8>);

/// I4 through the query service.
async fn facts(shared: &Shared, node: &Node, model: &ChModel) {
    let Some(rows) = system_oracle::fact_rows(model) else {
        shared.ledger().violate(Violation::FactResolution(
            "a fact row does not read".to_owned(),
        ));
        return;
    };
    let Ok(all_time) = TimeWindow::new(Timestamp::EPOCH, Timestamp::MAX_EXCLUSIVE) else {
        return;
    };
    let mut targets: BTreeMap<Target, Vec<&FactRow>> = BTreeMap::new();
    for row in &rows {
        let key = (row.project, row.trace, row.span, row.session.clone());
        targets.entry(key).or_default().push(row);
    }
    for ((project, ..), rows) in targets {
        let Some(ctx) = context(project) else {
            continue;
        };
        let expected: BTreeSet<[u8; 16]> = system_oracle::resolve(rows.iter().copied())
            .into_values()
            .collect();
        let read = node.query.facts(&ctx, &target(rows[0]), all_time).await;
        let found = match read {
            Ok(read) => read.facts,
            Err(error) => {
                shared.ledger().violate(Violation::FactResolution(format!(
                    "the facts of a target do not read: {error}"
                )));
                continue;
            }
        };
        let id = |fact_id: FactId| fact_id.to_bytes();
        let mut winners: BTreeSet<[u8; 16]> = BTreeSet::new();
        winners.extend(found.scores.iter().map(|s| id(s.resolved.fact_id)));
        winners.extend(found.tags.iter().map(|t| id(t.resolved.fact_id)));
        winners.extend(found.bookmarked.iter().map(|f| id(f.resolved.fact_id)));
        winners.extend(found.public.iter().map(|f| id(f.resolved.fact_id)));
        winners.extend(found.correction.iter().map(|c| id(c.resolved.fact_id)));
        if winners != expected {
            shared.ledger().violate(Violation::FactResolution(format!(
                "the query service resolved {} facts of a target where the reference fold picks {}",
                winners.len(),
                expected.len()
            )));
        }
    }
}

/// The totals the span rollup must give for one project: spans, errors, input tokens and cost.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Totals {
    spans: u64,
    errors: u64,
    input_tokens: u64,
    cost: i64,
    traces: u64,
}

/// The reference: the project's rows of `span_core` and its root heads, inside `window`.
fn reference(model: &ChModel, window: TimeWindow) -> Option<BTreeMap<[u8; 16], Totals>> {
    let mut out: BTreeMap<[u8; 16], Totals> = BTreeMap::new();
    let inside = |micros: i64| window.contains(Timestamp::from_micros(micros));
    for part in model.parts(TableId::SpanCore) {
        let block = &part.block;
        for row in 0..block.rows {
            if !inside(i64_at(block, "start_time", row)?) {
                continue;
            }
            let totals = out.entry(uuid_at(block, "project_id", row)?).or_default();
            totals.spans += 1;
            totals.errors += u64::from(i8_at(block, "level", row)? == LEVEL_ERROR);
            // Token and cost sums leave out the usage aggregates, as the rollups do (contract 3.1).
            if u8_at(block, "usage_aggregate", row)? == 0 {
                totals.input_tokens += u64_at(block, "input_tokens", row)?;
                totals.cost += i64_at(block, "total_cost_nusd", row)?;
            }
        }
    }
    for part in model.parts(TableId::TraceHeads) {
        let block = &part.block;
        for row in 0..block.rows {
            if inside(i64_at(block, "start_time", row)?) && i8_at(block, "head_kind", row)? == ROOT
            {
                out.entry(uuid_at(block, "project_id", row)?)
                    .or_default()
                    .traces += 1;
            }
        }
    }
    Some(out)
}

fn sum(points: &[crucible_query::MetricsPoint]) -> i128 {
    points
        .iter()
        .map(|point| match point.value {
            Some(MetricValue::UInt(v)) => i128::from(v),
            Some(MetricValue::Int(v)) => i128::from(v),
            None => 0,
        })
        .sum()
}

/// I7 through the query service: the metrics totals of every project equal the reference.
async fn metrics(shared: &Shared, node: &Node, model: &ChModel, projects: usize) {
    let window = TimeWindow::new(
        Timestamp::from_micros(START_BASE_MICROS),
        Timestamp::from_micros(SIM_EPOCH_MICROS),
    );
    let Ok(window) = window else {
        return;
    };
    let Some(reference) = reference(model, window) else {
        shared.ledger().violate(Violation::Aggregate(
            "a span_core or trace_heads row does not read".to_owned(),
        ));
        return;
    };
    let query = |source, measures: Vec<MeasureId>| MetricsQuery {
        source,
        measures: measures
            .into_iter()
            .map(|m| (m, Aggregation::Sum))
            .collect(),
        dimensions: Vec::new(),
        filter: Filter::all(),
        granularity: Granularity::Auto,
        fill: Fill::Zero,
        top_n: None,
    };
    let spans = query(
        Source::Spans,
        vec![
            MeasureId::Count,
            MeasureId::ErrorCount,
            MeasureId::InputTokens,
            MeasureId::TotalCost,
        ],
    );
    let traces = query(Source::Traces, vec![MeasureId::Traces]);
    for project in 0..projects {
        let id = super::project_id(project).to_bytes();
        let Some(ctx) = context(id) else {
            continue;
        };
        let expected = reference.get(&id).copied().unwrap_or_default();
        let mut found = [0i128; 5];
        for (query, at) in [(spans.clone(), 0), (traces.clone(), 4)] {
            let request = MetricsRequest { window, query };
            match node.query.metrics(&ctx, request).await {
                Ok(result) => {
                    for (i, series) in result.series.iter().enumerate() {
                        found[at + i] = sum(&series.points);
                    }
                }
                Err(error) => {
                    shared.ledger().violate(Violation::Aggregate(format!(
                        "the metrics of project {project} do not read: {error}"
                    )));
                    return;
                }
            }
        }
        let wanted = [
            i128::from(expected.spans),
            i128::from(expected.errors),
            i128::from(expected.input_tokens),
            i128::from(expected.cost),
            i128::from(expected.traces),
        ];
        if found != wanted {
            shared.ledger().violate(Violation::Aggregate(format!(
                "the metrics totals of project {project} (spans, errors, input tokens, cost, traces) are \
                 {found:?}, the reference over span_core and trace_heads gives {wanted:?}"
            )));
        }
    }
}

/// Runs the checks; the metrics check only without a quarantine, whose pairs can hold a rollup's rows apart
/// from their spans.
pub(super) async fn check(shared: &Shared, node: &Node, model: &ChModel, projects: usize) {
    facts(shared, node, model).await;
    let quarantines = shared.ledger().stats.quarantines > 0;
    if !quarantines {
        metrics(shared, node, model, projects).await;
    }
}
