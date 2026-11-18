//! Properties of the read and maintenance side of the simulation's store model (`ChModel`, contract 2.6) that
//! need no server: answers do not depend on the merge schedule and equal a reference fold, deletes hide rows at
//! once, partition drops remove them, every row follows the plan's output schema, and the ledger of landed parts
//! is untouched by all of it. The comparison with real servers is the differential row of the suite.
//!
//! The tests live here because this crate has the block builder for any `TableDef`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use crucible_auth::test_support::scope;
use crucible_conformance::blocks::Rows;
use crucible_env::{
    ChaChaRng, InsertSettings, MutationStatus, QueryError, StoreError, StoreFlavour, TelemetryStore,
};
use crucible_plan::test_support::{batch_probe, query_plan};
use crucible_plan::{
    Aggregate, CmpOp, DeletePlan, Fill, Measure, Node, Pred, QueryClass, QueryPlan, Row, Scan,
    Step, TimeSeries, Value,
};
use crucible_records::{
    BatchToken, Lsn, ProjectId, TableId, TimeWindow, Timestamp, Uuid, WalEpoch,
};
use crucible_schema::{
    Column, ColumnRef, Granularity, McpOpsCol, SpanMetricsCol, TABLES, TableDef, TableKind,
    TraceStatsCol, Ty, route,
};
use crucible_sim::ch_model::{ChFaults, ChModel};
use futures::StreamExt;
use proptest::prelude::*;

/// 2026-11-17T09:30:00Z.
const T0: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);
const MINUTE: i64 = 60_000_000;
const DAY: i64 = 86_400_000_000;

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
}

fn project(n: u8) -> ProjectId {
    ProjectId::from_bytes([n; 16])
}

fn epoch() -> WalEpoch {
    WalEpoch::from_uuid(Uuid::from_bytes([0xee; 16]))
}

fn model(seed: u64, merge_ppm: u32) -> ChModel {
    let model = ChModel::new(
        &ChaChaRng::from_u64(seed),
        StoreFlavour::SingleNode,
        Granularity::Day,
        Duration::from_secs(60),
    );
    model.set_merge_ppm(merge_ppm);
    model
}

fn window() -> TimeWindow {
    TimeWindow::new(at(-DAY), at(DAY)).unwrap()
}

fn plan(project_id: ProjectId, class: QueryClass, root: Node) -> QueryPlan {
    query_plan(scope(project_id).unwrap(), window(), class, root)
}

fn token(seq: u64, table: TableId, time: Timestamp) -> BatchToken {
    let partition = route(table, Granularity::Day, time).unwrap();
    BatchToken::new(epoch(), Lsn::new(seq), Lsn::new(seq), table, partition).unwrap()
}

async fn insert(model: &ChModel, rows: Rows, seq: u64, time: Timestamp) {
    let rows = rows.sorted();
    let settings = InsertSettings::new(token(seq, rows.table(), time), StoreFlavour::SingleNode);
    model
        .insert_block(rows.encode().unwrap(), settings)
        .await
        .unwrap_or_else(|error| panic!("{error:?}: {:?}", model.violations()));
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

fn sorted(model: &ChModel, plan: &QueryPlan) -> Vec<String> {
    let mut rows: Vec<String> = model
        .read(plan)
        .unwrap()
        .into_rows()
        .iter()
        .map(|row| format!("{:?}", row.values()))
        .collect();
    rows.sort_unstable();
    rows
}

/// One partial row of `span_metrics`: its minute, environment and release, its count and its histogram.
type Partial = (i64, u8, u8, u64, Vec<(u64, u64)>);

fn histogram(entries: &[(u64, u64)]) -> Value {
    let array = |values: Vec<u64>| Value::Array(values.into_iter().map(Value::UInt).collect());
    Value::Tuple2(
        Box::new(array(entries.iter().map(|e| e.0).collect())),
        Box::new(array(entries.iter().map(|e| e.1).collect())),
    )
}

/// A block of `span_metrics` for one project: the partial rows, then the marker row of batch `seq`.
fn metrics(project_id: ProjectId, seq: u64, partials: &[Partial]) -> Rows {
    let data = partials.len();
    let partials = partials.to_vec();
    let field = move |row: usize| partials.get(row).cloned();
    let (minute, environment, release, count, hist) = (
        field.clone(),
        field.clone(),
        field.clone(),
        field.clone(),
        field,
    );
    Rows::new(TableId::SpanMetrics, data + 1)
        .each("project_id", |row| {
            Value::Uuid(if row < data {
                project_id.uuid()
            } else {
                Uuid::NIL
            })
        })
        .each("minute", move |row| {
            Value::Time(at(minute(row).map_or(0, |p| p.0) * MINUTE).floor_minute())
        })
        .each("environment", move |row| {
            Value::String(environment(row).map_or(String::new(), |p| format!("env{}", p.1)))
        })
        .each("release", move |row| {
            Value::String(release(row).map_or(String::new(), |p| format!("r{}", p.2)))
        })
        .each("count", move |row| {
            Value::UInt(count(row).map_or(0, |p| p.3))
        })
        .each("dur_hist", move |row| {
            histogram(&hist(row).map_or(Vec::new(), |p| p.4))
        })
        .each("marker_epoch", |row| {
            Value::Uuid(if row < data {
                Uuid::NIL
            } else {
                epoch().uuid()
            })
        })
        .each("marker_batch_seq", |row| {
            Value::UInt(if row < data { 0 } else { seq })
        })
        .all("marker_at", Value::Time(T0.floor_minute()))
}

/// A block of `trace_stats` with one partial row of one trace and the marker row of batch `seq`.
fn stats(project_id: ProjectId, seq: u64, trace: u8, start: i64, end: i64, spans: u64) -> Rows {
    Rows::new(TableId::TraceStats, 2)
        .each("project_id", |row| {
            Value::Uuid(if row == 0 {
                project_id.uuid()
            } else {
                Uuid::NIL
            })
        })
        .each("trace_id", |row| {
            Value::Bytes(vec![if row == 0 { trace } else { 0 }; 16])
        })
        .all("row_day", Value::Date(T0.date()))
        .all("min_start", Value::Time(at(start)))
        .all("max_end", Value::Time(at(end)))
        .each("span_count", |row| {
            Value::UInt(if row == 0 { spans } else { 0 })
        })
        .each("marker_epoch", |row| {
            Value::Uuid(if row == 0 { Uuid::NIL } else { epoch().uuid() })
        })
        .each("marker_batch_seq", |row| {
            Value::UInt(if row == 0 { 0 } else { seq })
        })
}

fn metric_plans(project_id: ProjectId) -> Vec<QueryPlan> {
    let measures = || {
        vec![
            Measure::Sum(SpanMetricsCol::Count.column_ref()),
            Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
        ]
    };
    let aggregate = |group: Vec<SpanMetricsCol>| {
        Aggregate::new(TableId::SpanMetrics, Pred::all(), group, measures()).unwrap()
    };
    let filtered = Aggregate::new(
        TableId::SpanMetrics,
        Pred::cmp(SpanMetricsCol::Release, CmpOp::Ne, "r0"),
        [SpanMetricsCol::Minute],
        measures(),
    )
    .unwrap();
    let series = TimeSeries::new(
        aggregate(vec![SpanMetricsCol::Release]),
        SpanMetricsCol::Minute,
        Step::FiveMinutes,
        Fill::None,
    )
    .unwrap();
    vec![
        plan(
            project_id,
            QueryClass::Analytics,
            Node::Aggregate(aggregate(Vec::new())),
        ),
        plan(
            project_id,
            QueryClass::Analytics,
            Node::Aggregate(aggregate(vec![SpanMetricsCol::Environment])),
        ),
        plan(
            project_id,
            QueryClass::Analytics,
            Node::Aggregate(aggregate(vec![
                SpanMetricsCol::Minute,
                SpanMetricsCol::Release,
            ])),
        ),
        plan(project_id, QueryClass::Analytics, Node::Aggregate(filtered)),
        plan(project_id, QueryClass::Analytics, Node::TimeSeries(series)),
    ]
}

fn trace_plan(project_id: ProjectId) -> QueryPlan {
    let bounds = Aggregate::new(
        TableId::TraceStats,
        Pred::all(),
        [TraceStatsCol::TraceId],
        vec![
            Measure::Min(TraceStatsCol::MinStart.column_ref()),
            Measure::Max(TraceStatsCol::MaxEnd.column_ref()),
            Measure::Sum(TraceStatsCol::SpanCount.column_ref()),
        ],
    )
    .unwrap();
    plan(project_id, QueryClass::Point, Node::Aggregate(bounds))
}

fn partials() -> impl Strategy<Value = Vec<Vec<Partial>>> {
    let entry = (0u64..5, 0u64..4);
    let partial = (
        0i64..4,
        0u8..2,
        0u8..3,
        0u64..1_000,
        proptest::collection::vec(entry, 0..4),
    );
    proptest::collection::vec(proptest::collection::vec(partial, 1..5), 1..7)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// The answers over partial rows are the same when no merge ran, when merges ran on the seeded schedule
    /// and when every partition was merged down to one part, and they equal the reference fold. The ledger of
    /// landed parts does not notice a merge.
    #[test]
    fn answers_do_not_depend_on_the_merge_schedule(
        blocks in partials(),
        traces in proptest::collection::vec((0u8..3, 0i64..1_000, 0i64..1_000, 0u64..50), 1..8),
        seed in any::<u64>(),
    ) {
        block_on(async {
            let project_id = project(1);
            let models = [model(seed, 0), model(seed, 1_000_000), model(seed, 500_000)];
            for model in &models {
                let mut seq = 0;
                for block in &blocks {
                    seq += 1;
                    insert(model, metrics(project_id, seq, block), seq, T0).await;
                }
                for (trace, start, end, spans) in &traces {
                    seq += 1;
                    let rows = stats(project_id, seq, *trace + 1, *start, *end, *spans);
                    insert(model, rows, seq, T0).await;
                }
            }
            let ledger = models[2].digest();
            models[2].merge_all();
            let mut plans = metric_plans(project_id);
            plans.push(trace_plan(project_id));
            for plan in &plans {
                let unmerged = sorted(&models[0], plan);
                prop_assert_eq!(&unmerged, &sorted(&models[1], plan), "seeded merges: {:?}", plan);
                prop_assert_eq!(&unmerged, &sorted(&models[2], plan), "full merge: {:?}", plan);
            }
            prop_assert_eq!(ledger, models[2].digest());
            prop_assert_eq!(
                models[0].parts(TableId::SpanMetrics).len(),
                models[2].parts(TableId::SpanMetrics).len()
            );

            // The reference fold of the ungrouped read: the sum of the counts, and per bucket the sum of its
            // counts, without the buckets that sum to zero.
            let total: u64 = blocks.iter().flatten().map(|p| p.3).sum();
            let mut buckets: BTreeMap<u64, u64> = BTreeMap::new();
            for (key, count) in blocks.iter().flatten().flat_map(|p| p.4.iter()) {
                *buckets.entry(*key).or_default() += count;
            }
            buckets.retain(|_, count| *count != 0);
            let expected = Row::new(vec![
                Value::UInt(total),
                histogram(&buckets.into_iter().collect::<Vec<_>>()),
            ]);
            for model in &models {
                prop_assert_eq!(model.read(&plans[0]).unwrap().into_rows(), vec![expected.clone()]);
            }
            // The reference fold of the trace bounds: the earliest start, the latest end and the span count.
            let mut bounds: BTreeMap<u8, (i64, i64, u64)> = BTreeMap::new();
            for (trace, start, end, spans) in &traces {
                let entry = bounds.entry(*trace + 1).or_insert((*start, *end, 0));
                *entry = (entry.0.min(*start), entry.1.max(*end), entry.2 + spans);
            }
            let mut expected: Vec<String> = bounds
                .into_iter()
                .map(|(trace, (start, end, spans))| {
                    format!(
                        "{:?}",
                        [
                            Value::Bytes(vec![trace; 16]),
                            Value::Time(at(start)),
                            Value::Time(at(end)),
                            Value::UInt(spans),
                        ]
                    )
                })
                .collect();
            expected.sort_unstable();
            for model in &models {
                prop_assert_eq!(&sorted(model, &plans[plans.len() - 1]), &expected);
            }
            Ok(())
        })?;
    }
}

/// Ten rows of `mcp_ops` of one project at `time`, with `duration_us` counting up.
fn ops(project_id: ProjectId, seq: u64, time: Timestamp) -> Rows {
    Rows::new(TableId::McpOps, 10)
        .all("project_id", Value::Uuid(project_id.uuid()))
        .all("start_time", Value::Time(time))
        .all("start_minute", Value::Time(time.floor_minute()))
        .each("span_id", |row| Value::Bytes(vec![row as u8 + 1; 8]))
        .each("duration_us", |row| Value::UInt(row as u64))
        .all("wal_epoch", Value::Uuid(epoch().uuid()))
        .all("batch_seq", Value::UInt(seq))
        .all("lsn", Value::UInt(seq))
        .all("received_at", Value::Time(time))
}

fn durations(model: &ChModel, project_id: ProjectId) -> Vec<u64> {
    let scan = Scan::new(TableId::McpOps, [McpOpsCol::DurationUs], Pred::all()).unwrap();
    let rows = model
        .read(&plan(project_id, QueryClass::List, Node::Scan(scan)))
        .unwrap()
        .into_rows();
    let mut found: Vec<u64> = rows
        .iter()
        .map(|row| match row.values() {
            [Value::UInt(duration)] => *duration,
            other => panic!("{other:?}"),
        })
        .collect();
    found.sort_unstable();
    found
}

/// Waits until a mutation is done: the model keeps one pending for at most 50 ms of its clock.
async fn done(model: &ChModel, id: &crucible_env::MutationId) {
    while model.mutation_status(id).await.unwrap() != MutationStatus::Done {
        tokio::task::yield_now().await;
    }
}

#[test]
fn a_delete_hides_its_rows_at_once_and_the_mask_is_applied_later() {
    block_on(async {
        for merge_ppm in [0, 1_000_000] {
            let model = model(3, merge_ppm);
            let table = TableId::McpOps;
            let partition = route(table, Granularity::Day, T0).unwrap();
            insert(&model, ops(project(1), 1, T0), 1, T0).await;
            insert(&model, ops(project(2), 2, T0), 2, T0).await;
            assert_eq!(
                model.partition_projects(table, partition).await.unwrap(),
                vec![project(1), project(2)]
            );

            let some = DeletePlan::project(
                scope(project(1)).unwrap(),
                table,
                partition,
                Pred::cmp(McpOpsCol::DurationUs, CmpOp::Ge, 4u64),
            )
            .unwrap();
            let delete = model.delete(some).await.unwrap();
            // Hidden on return, whatever the mutation's status; the other project keeps every row.
            assert_eq!(durations(&model, project(1)), vec![0, 1, 2, 3]);
            assert_eq!(durations(&model, project(2)), (0..10).collect::<Vec<_>>());
            assert!(model.masked_parts(table, partition).await.unwrap() >= 1);
            done(&model, &delete).await;

            // A merge and the mask application both leave the answer alone and remove the mask.
            if merge_ppm > 0 {
                model.merge_all();
            } else {
                let mask = model.apply_deleted_mask(table, partition).await.unwrap();
                done(&model, &mask).await;
            }
            assert_eq!(model.masked_parts(table, partition).await.unwrap(), 0);
            assert_eq!(durations(&model, project(1)), vec![0, 1, 2, 3]);

            // Rows that land after the delete are not hidden by it.
            insert(&model, ops(project(1), 3, at(MINUTE)), 3, at(MINUTE)).await;
            assert_eq!(durations(&model, project(1)).len(), 14);

            let rest =
                DeletePlan::project(scope(project(1)).unwrap(), table, partition, Pred::all());
            model.delete(rest.unwrap()).await.unwrap();
            assert!(durations(&model, project(1)).is_empty());
            assert_eq!(
                model.partition_projects(table, partition).await.unwrap(),
                vec![project(2)]
            );
            // The probe scans what is left: the deleted batch is absent, the other present.
            let probe = |seq| batch_probe(table, partition, epoch(), Lsn::new(seq)).unwrap();
            assert!(!model.probe(probe(1)).await.unwrap());
            assert!(model.probe(probe(2)).await.unwrap());
            // The ledger of landed parts keeps every block.
            assert_eq!(model.parts(table).len(), 3);
            assert!(model.violations().is_empty());
        }
    });
}

#[test]
fn a_partition_drop_removes_rows_markers_and_deduplication_entries() {
    block_on(async {
        let model = model(5, 300_000);
        let table = TableId::McpOps;
        let later = at(3 * DAY);
        let (here, there) = (
            route(table, Granularity::Day, T0).unwrap(),
            route(table, Granularity::Day, later).unwrap(),
        );
        insert(&model, ops(project(1), 1, T0), 1, T0).await;
        insert(&model, ops(project(1), 2, later), 2, later).await;
        let wide = |project_id| {
            let scan = Scan::new(table, [McpOpsCol::DurationUs], Pred::all()).unwrap();
            query_plan(
                scope(project_id).unwrap(),
                TimeWindow::new(at(-DAY), at(5 * DAY)).unwrap(),
                QueryClass::List,
                Node::Scan(scan),
            )
        };
        assert_eq!(model.read(&wide(project(1))).unwrap().into_rows().len(), 20);

        // Dropping twice, and dropping a partition that never existed, succeeds.
        for partition in [
            there,
            there,
            route(table, Granularity::Day, at(9 * DAY)).unwrap(),
        ] {
            model.drop_partition(table, partition).await.unwrap();
        }
        assert_eq!(model.read(&wide(project(1))).unwrap().into_rows().len(), 10);
        assert!(
            model
                .partition_projects(table, there)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            model.partition_projects(table, here).await.unwrap(),
            vec![project(1)]
        );
        let probe = batch_probe(table, there, epoch(), Lsn::new(2)).unwrap();
        assert!(!model.probe(probe).await.unwrap());

        // The dropped parts took their deduplication entries with them: the block lands again, as on a server.
        insert(&model, ops(project(1), 2, later), 2, later).await;
        assert_eq!(model.read(&wide(project(1))).unwrap().into_rows().len(), 20);
        assert!(model.probe(probe).await.unwrap());
        assert!(model.violations().is_empty());

        // A dropped partition of an aggregating table loses its marker rows too.
        let rollup = TableId::SpanMetrics;
        let month = route(rollup, Granularity::Day, T0).unwrap();
        insert(
            &model,
            metrics(project(1), 7, &[(0, 0, 0, 5, Vec::new())]),
            7,
            T0,
        )
        .await;
        let marker = batch_probe(rollup, month, epoch(), Lsn::new(7)).unwrap();
        assert!(model.probe(marker).await.unwrap());
        assert_eq!(
            model.partition_projects(rollup, month).await.unwrap(),
            vec![project(1)]
        );
        model.drop_partition(rollup, month).await.unwrap();
        assert!(!model.probe(marker).await.unwrap());
        assert!(
            model
                .read(&metric_plans(project(1))[0])
                .unwrap()
                .runs
                .is_empty()
        );
    });
}

#[test]
fn a_marker_purge_removes_older_markers_only_and_no_data() {
    block_on(async {
        let model = model(9, 1_000_000);
        let rollup = TableId::SpanMetrics;
        let month = route(rollup, Granularity::Day, T0).unwrap();
        insert(
            &model,
            metrics(project(1), 1, &[(0, 0, 0, 5, Vec::new())]),
            1,
            T0,
        )
        .await;
        insert(
            &model,
            metrics(project(1), 2, &[(0, 0, 0, 7, Vec::new())]),
            2,
            T0,
        )
        .await;
        let probe = |seq| batch_probe(rollup, month, epoch(), Lsn::new(seq)).unwrap();
        let purge = |older_than| DeletePlan::marker_purge(rollup, month, older_than, Pred::all());
        model.delete(purge(at(-DAY)).unwrap()).await.unwrap();
        assert!(model.probe(probe(1)).await.unwrap() && model.probe(probe(2)).await.unwrap());
        model.delete(purge(at(DAY)).unwrap()).await.unwrap();
        assert!(!model.probe(probe(1)).await.unwrap() && !model.probe(probe(2)).await.unwrap());
        let total = model
            .read(&metric_plans(project(1))[0])
            .unwrap()
            .into_rows();
        assert_eq!(total[0].values()[0], Value::UInt(12));
    });
}

#[test]
fn every_row_follows_the_output_schema_and_no_read_crosses_projects() {
    block_on(async {
        let model = model(11, 300_000);
        insert(&model, ops(project(1), 1, T0), 1, T0).await;
        insert(
            &model,
            metrics(project(1), 2, &[(0, 0, 1, 5, vec![(1, 2)])]),
            2,
            T0,
        )
        .await;
        let mut plans = metric_plans(project(1));
        let scan = Scan::new(TableId::McpOps, McpOpsCol::ALL.iter().copied(), Pred::all()).unwrap();
        plans.push(plan(project(1), QueryClass::List, Node::Scan(scan)));
        for plan in plans {
            let schema = plan.output();
            let mut stream = model.execute(plan.clone()).await.unwrap();
            let mut rows = 0;
            while let Some(block) = stream.next().await {
                let block = block.unwrap();
                schema.check_block(&block).unwrap();
                rows += block.rows().len();
            }
            assert!(rows > 0, "{plan:?}");
            // Another project sees nothing, and the marker project cannot be asked for at all.
            let other = query_plan(
                scope(project(2)).unwrap(),
                plan.window(),
                plan.class(),
                plan.root().clone(),
            );
            assert!(model.read(&other).unwrap().runs.is_empty());
        }
        assert!(scope(ProjectId::from_uuid(Uuid::NIL)).is_err());
    });
}

/// Two rows of any shipped table at the fixture instant: for a plain table two rows of batch `seq`, for an
/// aggregating table one partial row and the marker row of batch `seq`. Every other column is at its default.
fn block_of(def: &'static TableDef, project_id: ProjectId, seq: u64) -> Rows {
    let aggregating = def.kind == TableKind::Aggregating;
    let data = move |row: usize| !aggregating || row == 0;
    let mut rows = Rows::new(def.id, 2).each("project_id", move |row| {
        Value::Uuid(if data(row) {
            project_id.uuid()
        } else {
            Uuid::NIL
        })
    });
    // Every time column that routes or bounds the rows holds the fixture instant.
    for column in def.columns {
        let bounds = Some(column.name) == def.partition.column()
            || column.name == def.retention.time_column
            || matches!(
                column.name,
                "start_minute" | "target_day" | "first_seen" | "last_seen"
            );
        let value = match column.ty.scalar() {
            Ty::DateTime64 => Value::Time(T0),
            Ty::DateTime => Value::Time(T0.floor_minute()),
            Ty::Date => Value::Date(T0.date()),
            _ => continue,
        };
        if bounds {
            rows = rows.all(column.name, value);
        }
    }
    if aggregating {
        rows.each("marker_epoch", move |row| {
            Value::Uuid(if data(row) { Uuid::NIL } else { epoch().uuid() })
        })
        .each("marker_batch_seq", move |row| {
            Value::UInt(if data(row) { 0 } else { seq })
        })
    } else {
        rows.all("wal_epoch", Value::Uuid(epoch().uuid()))
            .all("batch_seq", Value::UInt(seq))
            .all("lsn", Value::UInt(seq))
    }
}

/// The tables the shipper writes: project tables whose rows carry a batch identity, which leaves out
/// `dedup_selftest` and `sql_probe`.
fn shipped() -> impl Iterator<Item = &'static TableDef> {
    TABLES.iter().filter(|def| {
        def.is_scoped()
            && (def.column("wal_epoch").is_some() || def.column("marker_epoch").is_some())
    })
}

#[test]
fn every_shipped_table_lands_merges_and_reads_back() {
    block_on(async {
        let model = model(17, 1_000_000);
        let mut seq = 0;
        for def in shipped() {
            // Two blocks per table, so that a merge has rows of one sort key to combine.
            for _ in 0..2 {
                seq += 1;
                insert(&model, block_of(def, project(1), seq), seq, T0).await;
            }
        }
        assert_eq!(seq, 32, "16 of the 18 tables are shipped");
        let read = |def: &'static TableDef| {
            let keys: Vec<ColumnRef> = def
                .key_columns()
                .into_iter()
                .filter(|column| !column.name().starts_with("marker_"))
                .collect();
            let root = match def.kind {
                TableKind::Plain => Node::Scan(Scan::new(def.id, keys, Pred::all()).unwrap()),
                TableKind::Aggregating => {
                    let measure = Measure::Max(keys[0]);
                    Node::Aggregate(
                        Aggregate::new(def.id, Pred::all(), keys, vec![measure]).unwrap(),
                    )
                }
            };
            let rows = model.read(&plan(project(1), QueryClass::List, root));
            rows.unwrap_or_else(|error| panic!("{}: {error}", def.name()))
                .into_rows()
                .len()
        };
        for merged in [false, true] {
            if merged {
                model.merge_all();
            }
            for def in shipped() {
                // Four plain rows, or one group of the two partial rows that share every key column.
                let expected = match def.kind {
                    TableKind::Plain => 4,
                    TableKind::Aggregating => 1,
                };
                assert_eq!(read(def), expected, "{} (merged: {merged})", def.name());
                let partition = route(def.id, Granularity::Day, T0).unwrap();
                assert_eq!(
                    model.partition_projects(def.id, partition).await.unwrap(),
                    vec![project(1)],
                    "{}",
                    def.name()
                );
            }
        }
        assert!(model.stats().merges > 0);
        assert!(model.violations().is_empty(), "{:?}", model.violations());
    });
}

#[test]
fn reads_and_maintenance_fail_as_unavailable_under_an_outage_or_the_read_fault() {
    block_on(async {
        let model = model(13, 0);
        insert(&model, ops(project(1), 1, T0), 1, T0).await;
        let table = TableId::McpOps;
        let partition = route(table, Granularity::Day, T0).unwrap();
        let scan = Scan::new(table, [McpOpsCol::DurationUs], Pred::all()).unwrap();
        let read = plan(project(1), QueryClass::List, Node::Scan(scan));
        let unavailable = async |model: &ChModel| {
            let read = model.execute(read.clone()).await;
            assert!(matches!(read, Err(QueryError::Unavailable(_))), "{read:?}");
            let projects = model.partition_projects(table, partition).await;
            assert!(matches!(projects, Err(StoreError::Unavailable(_))));
            let dropped = model.drop_partition(table, partition).await;
            assert!(matches!(dropped, Err(StoreError::Unavailable(_))));
        };
        model.start_outage(Duration::from_secs(600));
        unavailable(&model).await;
        model.end_outage();
        model.set_faults(ChFaults {
            read_error_ppm: 1_000_000,
            ..ChFaults::default()
        });
        unavailable(&model).await;
        model.set_faults(ChFaults::default());
        // A failed call changed nothing.
        assert_eq!(
            model
                .execute(read.clone())
                .await
                .unwrap()
                .rows()
                .await
                .unwrap()
                .len(),
            10
        );
        let stats = model.stats();
        assert_eq!((stats.reads, stats.read_errors), (3, 6));
    });
}
