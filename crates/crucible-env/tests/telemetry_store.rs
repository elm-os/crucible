//! The `TelemetryStore` trait (contract 2.6): object safety, a fake that proves the trait is implementable and
//! that its rows follow `QueryPlan::output`, and the error mappings.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use crucible_auth::test_support::scope;
use crucible_chfmt::{Block, Compression, EncodedBlock};
use crucible_env::{
    EngineFamily, InsertClass, InsertError, InsertSettings, MutationId, MutationStatus, QueryError,
    RetryReason, RowStream, StoreError, StoreFacts, StoreFlavour, StoreHealth, StoreVersion,
    Stores, TelemetryStore,
};
use crucible_plan::test_support::batch_probe;
use crucible_plan::{
    Aggregate, BatchProbe, Caller, DeletePlan, Measure, Node, Pred, QueryPlan, RetentionClamp, Row,
    RowBlock, ShapeError, TraceBounds, TraceListRequest, Value, data_bounds, span_children,
    trace_list, trace_spans, trace_time_bounds,
};
use crucible_records::{
    BatchToken, CivilDate, Lsn, PartitionId, ProjectId, ProjectScope, SpanId, TableId, TimeWindow,
    Timestamp, TraceId, Uuid, WalEpoch,
};
use crucible_schema::{
    ColumnRef, MigrationStep, RetentionSettings, SCHEMA_BASE, StepKind, TABLES, Ty,
};
use futures::StreamExt;
use futures::executor::block_on;

const QUERY_ID: Uuid = Uuid::from_bytes([7; 16]);

/// A value of a stored column type, written against the `TableDef` types (storage wrappers included) and
/// independently of `QueryPlan::output`.
fn stored(ty: &Ty) -> Value {
    match ty {
        Ty::Uuid => Value::Uuid(Uuid::from_bytes([1; 16])),
        Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64 => Value::UInt(1),
        Ty::Int32 | Ty::Int64 => Value::Int(-1),
        Ty::Float32 | Ty::Float64 => Value::Float(0.5),
        Ty::String | Ty::LowCardinalityString => Value::String("x".to_owned()),
        Ty::FixedString(n) => Value::Bytes(vec![0xab; usize::from(*n)]),
        Ty::Date => Value::Date(CivilDate::new(2026, 11, 17).unwrap()),
        Ty::DateTime | Ty::DateTime64 => Value::Time(Timestamp::from_micros(1_794_907_800_000_000)),
        Ty::Enum8(kind) => Value::Enum(kind.variants()[0].1),
        Ty::Nullable(_) => Value::Null,
        Ty::Array(element) => Value::Array(vec![stored(element)]),
        Ty::Tuple2(a, b) => Value::Tuple2(Box::new(stored(a)), Box::new(stored(b))),
        Ty::Saf(_, inner) => stored(inner),
    }
}

fn stored_row(columns: &[ColumnRef]) -> Row {
    Row::new(columns.iter().map(|c| stored(c.ty())).collect())
}

fn aggregate_row(aggregate: &Aggregate) -> Row {
    let mut values: Vec<Value> = aggregate.group().iter().map(|c| stored(c.ty())).collect();
    values.extend(aggregate.measures().iter().map(|measure| match measure {
        Measure::Count | Measure::Uniq(_) => Value::UInt(3),
        Measure::Sum(c) => match c.ty().scalar() {
            Ty::Int32 | Ty::Int64 => Value::Int(-3),
            Ty::Float32 | Ty::Float64 => Value::Float(1.5),
            _ => Value::UInt(3),
        },
        Measure::Min(c)
        | Measure::Max(c)
        | Measure::HistogramMerge(c)
        | Measure::PickMerge(c)
        | Measure::LatestByVersion(c)
        | Measure::SetMerge(c) => stored(c.ty()),
    }));
    Row::new(values)
}

/// The rows the fake answers a plan with: two rows per result, typed by the node, not by `output()`.
fn fake_rows(node: &Node) -> Vec<Row> {
    let row = match node {
        Node::Scan(n) => stored_row(n.columns()),
        Node::PointLookup(n) => stored_row(n.columns()),
        Node::Aggregate(n) => aggregate_row(n),
        Node::SemiJoin(n) => stored_row(n.left().columns()),
        Node::TextSearch(n) => stored_row(&crucible_schema::table(n.table()).key_columns()),
        Node::TimeSeries(n) => {
            let mut values = vec![stored(&Ty::DateTime)];
            values.extend(aggregate_row(n.aggregate()).into_values());
            Row::new(values)
        }
        Node::FieldRange(n) => Row::new(vec![stored(n.field().ty())]),
    };
    vec![row.clone(), row]
}

#[derive(Debug, Default)]
struct State {
    landed: BTreeSet<(u8, String, [u8; 16], u64)>,
    killed: Vec<Uuid>,
    mutations: u64,
}

/// An in-memory store: enough behaviour to exercise every method through `Arc<dyn TelemetryStore>`.
#[derive(Debug, Default)]
struct FakeStore {
    state: Mutex<State>,
    fail_reads_with: Option<QueryError>,
}

impl FakeStore {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mutation(&self, table: TableId) -> MutationId {
        let mut state = self.state();
        state.mutations += 1;
        MutationId::new(table, format!("mutation_{}", state.mutations))
    }
}

#[async_trait]
impl TelemetryStore for FakeStore {
    async fn insert_block(
        &self,
        block: EncodedBlock,
        settings: InsertSettings,
    ) -> Result<(), InsertError> {
        if block.rows() == 0 {
            return Err(InsertError::Poison {
                code: 27,
                message: "empty block".to_owned(),
            });
        }
        let token = settings.token();
        // A repeated token is deduplicated: the set keeps one entry.
        self.state().landed.insert((
            settings.table().id(),
            settings.partition().to_string(),
            token.wal_epoch().to_bytes(),
            token.batch_seq().get(),
        ));
        Ok(())
    }

    async fn execute(&self, plan: QueryPlan) -> Result<RowStream, QueryError> {
        let rows = fake_rows(plan.root());
        // One row per block: block boundaries carry no meaning.
        let mut items: Vec<Result<RowBlock, QueryError>> = rows
            .into_iter()
            .map(|row| Ok(RowBlock::new(vec![row]).unwrap()))
            .collect();
        if let Some(error) = &self.fail_reads_with {
            items.truncate(1);
            items.push(Err(error.clone()));
        }
        Ok(RowStream::new(QUERY_ID, futures::stream::iter(items)))
    }

    async fn probe(&self, probe: BatchProbe) -> Result<bool, StoreError> {
        Ok(self.state().landed.contains(&(
            probe.table().id(),
            probe.partition().to_string(),
            probe.epoch().to_bytes(),
            probe.batch_seq().get(),
        )))
    }

    async fn delete(&self, plan: DeletePlan) -> Result<MutationId, StoreError> {
        Ok(self.mutation(plan.table()))
    }

    async fn drop_partition(
        &self,
        table: TableId,
        partition: PartitionId,
    ) -> Result<(), StoreError> {
        let partition = partition.to_string();
        self.state()
            .landed
            .retain(|(t, p, _, _)| (*t, p) != (table.id(), &partition));
        Ok(())
    }

    async fn apply_deleted_mask(
        &self,
        table: TableId,
        _partition: PartitionId,
    ) -> Result<MutationId, StoreError> {
        Ok(self.mutation(table))
    }

    async fn mutation_status(&self, id: &MutationId) -> Result<MutationStatus, StoreError> {
        Ok(if id.name() == "mutation_1" {
            MutationStatus::Pending
        } else {
            MutationStatus::Done
        })
    }

    async fn partition_projects(
        &self,
        _table: TableId,
        _partition: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError> {
        Ok(vec![ProjectId::from_bytes([9; 16])])
    }

    async fn masked_parts(
        &self,
        _table: TableId,
        _partition: PartitionId,
    ) -> Result<u64, StoreError> {
        Ok(0)
    }

    async fn partitions(&self, _table: TableId) -> Result<Vec<PartitionId>, StoreError> {
        Ok(Vec::new())
    }

    async fn holds_project(
        &self,
        _table: TableId,
        _partition: Option<PartitionId>,
        _project: ProjectId,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn apply_migration(&self, step: &MigrationStep) -> Result<(), StoreError> {
        match step.statements.iter().find(|s| s.contains(';')) {
            Some(statement) => Err(StoreError::Rejected {
                code: 62,
                message: statement.clone(),
            }),
            None => Ok(()),
        }
    }

    async fn detect(&self) -> Result<StoreFacts, StoreError> {
        Ok(StoreFacts {
            version: StoreVersion {
                major: 26,
                minor: 8,
            },
            flavour: StoreFlavour::SingleNode,
            engines: TABLES
                .iter()
                .map(|def| (def.id, EngineFamily::MergeTree))
                .collect(),
            missing_settings: Vec::new(),
            text_search_functions: true,
            text_index: true,
            ddl_initial_user_and_roles: None,
        })
    }

    async fn dedup_selftest(&self) -> Result<(), StoreError> {
        Ok(())
    }

    async fn kill(&self, query_id: Uuid) -> Result<(), StoreError> {
        self.state().killed.push(query_id);
        Ok(())
    }

    async fn health(&self) -> StoreHealth {
        StoreHealth::Healthy
    }
}

fn project() -> ProjectScope {
    scope(ProjectId::from_bytes([9; 16])).unwrap()
}

fn window() -> (Timestamp, Timestamp, TimeWindow) {
    let from = Timestamp::from_micros(1_794_900_000_000_000);
    let to = Timestamp::from_micros(1_794_907_800_000_000);
    (from, to, TimeWindow::new(from, to).unwrap())
}

/// One plan of every M1 builder: trace bounds, trace spans, trace list, span children and both data-bounds plans.
fn m1_plans() -> Vec<(&'static str, QueryPlan)> {
    let (from, to, window) = window();
    let trace_id = TraceId::from_bytes([3; 16]);
    let trace = TraceBounds {
        trace_id,
        min_start: from,
        max_end: to,
    };
    let list = TraceListRequest {
        filter: Pred::all(),
        limit: 50,
        cursor: Some((from, trace_id.trace_hash(), trace_id)),
        as_of: to,
        since: None,
    };
    // A project that keeps its data forever: the clamp leaves every window as it is.
    let keep = RetentionClamp::new(RetentionSettings::default(), to);
    let bounds = data_bounds(project(), window, &keep, Caller::Session).unwrap();
    vec![
        (
            "trace bounds",
            trace_time_bounds(project(), window, &keep, Caller::Session, trace_id)
                .unwrap()
                .unwrap(),
        ),
        (
            "trace spans",
            trace_spans(project(), from, to, &keep, Caller::Agent, trace_id)
                .unwrap()
                .unwrap(),
        ),
        (
            "trace list",
            trace_list(project(), window, &keep, Caller::Session, list)
                .unwrap()
                .unwrap(),
        ),
        (
            "span children",
            span_children(
                project(),
                trace,
                &keep,
                Caller::Session,
                SpanId::ZERO,
                None,
                None,
            )
            .unwrap()
            .unwrap(),
        ),
        ("data bounds, spans", bounds.spans.unwrap()),
        ("data bounds, mcp", bounds.mcp.unwrap()),
    ]
}

fn token(table: TableId, first_lsn: u64) -> BatchToken {
    BatchToken::new(
        WalEpoch::from_bytes([1; 16]),
        Lsn::new(first_lsn),
        Lsn::new(first_lsn + 9),
        table,
        PartitionId::parse("20261117").unwrap(),
    )
    .unwrap()
}

fn one_row_block() -> EncodedBlock {
    let values = vec![1u64];
    let mut block = Block::new(1).unwrap();
    block.push("v", "UInt64", &values).unwrap();
    block.encode(Compression::Lz4).unwrap()
}

#[test]
fn the_trait_is_object_safe_and_its_types_cross_threads() {
    fn send_sync<T: ?Sized + Send + Sync>() {}
    fn send_static<T: Send + 'static>() {}
    fn comparable<T: Clone + PartialEq + core::fmt::Debug + Send + Sync + 'static>() {}
    send_sync::<dyn TelemetryStore>();
    send_sync::<Arc<dyn TelemetryStore>>();
    send_sync::<Stores>();
    send_static::<RowStream>();
    comparable::<InsertError>();
    comparable::<QueryError>();
    comparable::<StoreError>();
    comparable::<StoreFacts>();
    comparable::<StoreHealth>();
    comparable::<MutationId>();
    comparable::<MutationStatus>();
    comparable::<InsertSettings>();
    comparable::<RowBlock>();

    // The futures of a boxed store are `Send`, so a spawned task can hold them.
    fn send<T: Send>(value: T) -> T {
        value
    }
    let stores = Stores::new(Arc::new(FakeStore::default()));
    let clone = stores.clone();
    let health = block_on(send(async move { clone.telemetry.health().await }));
    assert_eq!(health, StoreHealth::Healthy);
}

#[test]
fn the_fakes_rows_follow_the_output_schema_of_every_m1_plan() {
    let stores = Stores::new(Arc::new(FakeStore::default()));
    for (name, plan) in m1_plans() {
        let output = plan.output();
        assert!(!output.columns().is_empty(), "{name}");
        let mut stream = block_on(stores.telemetry.execute(plan)).unwrap();
        assert_eq!(stream.query_id(), QUERY_ID);
        let mut rows = 0;
        while let Some(block) = block_on(stream.next()) {
            let block = block.unwrap();
            assert_eq!(output.check_block(&block), Ok(()), "{name}");
            rows += block.rows().len();
        }
        assert_eq!(rows, 2, "{name}");
    }
}

#[test]
fn rows_of_the_wrong_shape_are_refused() {
    let (_, _, window) = window();
    let trace_id = TraceId::from_bytes([3; 16]);
    let keep = RetentionClamp::new(RetentionSettings::default(), window.to());
    let plan = trace_time_bounds(project(), window, &keep, Caller::Session, trace_id)
        .unwrap()
        .unwrap();
    let output = plan.output();
    let good = fake_rows(plan.root()).remove(0);
    assert_eq!(output.check_row(&good), Ok(()));

    let mut short = good.clone().into_values();
    short.pop();
    assert_eq!(
        output.check_row(&Row::new(short)),
        Err(ShapeError::Arity {
            expected: 6,
            actual: 5
        })
    );
    // `sum(span_count)` is unsigned.
    let mut signed = good.into_values();
    signed[3] = Value::Int(3);
    assert_eq!(
        output.check_row(&Row::new(signed)),
        Err(ShapeError::Type {
            index: 3,
            name: "sum_span_count".to_owned()
        })
    );
}

#[test]
fn an_error_ends_the_stream_and_the_query_can_be_killed() {
    let fake = Arc::new(FakeStore {
        fail_reads_with: Some(QueryError::Timeout),
        ..FakeStore::default()
    });
    let stores = Stores::new(fake.clone());
    let (_, plan) = m1_plans().remove(0);
    let mut stream = block_on(stores.telemetry.execute(plan.clone())).unwrap();
    assert!(block_on(stream.next()).unwrap().is_ok());
    assert_eq!(block_on(stream.next()), Some(Err(QueryError::Timeout)));
    assert_eq!(block_on(stream.next()), None);
    block_on(stores.telemetry.kill(stream.query_id())).unwrap();
    assert_eq!(fake.state().killed, [QUERY_ID]);

    let all = block_on(stores.telemetry.execute(plan)).unwrap();
    assert_eq!(block_on(all.rows()), Err(QueryError::Timeout));
    let whole = Stores::new(Arc::new(FakeStore::default()));
    let (_, plan) = m1_plans().remove(1);
    let rows = block_on(block_on(whole.telemetry.execute(plan)).unwrap().rows()).unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn query_errors_map_one_to_one_to_problem_codes() {
    let cases = [
        (QueryError::Timeout, "query_timeout"),
        (QueryError::MemoryExceeded, "query_memory_exceeded"),
        (QueryError::TooBroad, "query_too_broad"),
        (
            QueryError::Unavailable("refused".to_owned()),
            "telemetry_store_unavailable",
        ),
        (QueryError::Internal("bug".to_owned()), "internal_error"),
    ];
    let codes: BTreeSet<&str> = cases.iter().map(|(e, _)| e.problem_code()).collect();
    assert_eq!(codes.len(), cases.len(), "no two variants share a code");
    for (error, code) in cases {
        assert_eq!(error.problem_code(), code);
    }
}

#[test]
fn store_errors_map_to_problem_codes() {
    let unavailable = StoreError::Unavailable("refused".to_owned());
    assert_eq!(unavailable.problem_code(), "telemetry_store_unavailable");
    for error in [
        StoreError::Rejected {
            code: 62,
            message: "syntax".to_owned(),
        },
        StoreError::SelfTest("two rows".to_owned()),
        StoreError::Internal("bug".to_owned()),
    ] {
        assert_eq!(error.problem_code(), "internal_error");
    }
}

#[test]
fn insert_errors_carry_the_class_of_contract_5_6() {
    let message = String::new;
    let cases = [
        (
            InsertError::Retry {
                reason: RetryReason::TooManyParts,
                message: message(),
            },
            InsertClass::Retry,
        ),
        (
            InsertError::UnknownOutcome {
                code: Some(159),
                message: message(),
            },
            InsertClass::UnknownOutcome,
        ),
        (
            InsertError::FatalConfig {
                code: 516,
                message: message(),
            },
            InsertClass::FatalConfig,
        ),
        (
            InsertError::Poison {
                code: 53,
                message: message(),
            },
            InsertClass::Poison,
        ),
    ];
    for (error, class) in cases {
        assert_eq!(error.class(), class);
    }
}

#[test]
fn insert_settings_are_the_explicit_list_of_contract_5_4() {
    let token = token(TableId::SpanCore, 1_048_577);
    let single = InsertSettings::new(token, StoreFlavour::SingleNode);
    assert_eq!(single.token(), token);
    assert_eq!(single.table(), TableId::SpanCore);
    assert_eq!(single.partition(), token.partition());
    let expected = [
        ("async_insert", "0"),
        ("insert_deduplicate", "1"),
        (
            "insert_deduplication_token",
            "01010101-0101-0101-0101-010101010101:1048577-1048586:1:20261117",
        ),
        ("max_insert_threads", "1"),
        ("max_partitions_per_insert_block", "1"),
        ("throw_on_max_partitions_per_insert_block", "1"),
        ("input_format_native_allow_types_conversion", "1"),
        ("max_execution_time", "60"),
    ];
    let pairs = single.pairs();
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_eq!(pairs, expected);
    assert_eq!(
        InsertSettings::new(token, StoreFlavour::Cloud).pairs(),
        single.pairs()
    );

    let clustered = InsertSettings::new(token, StoreFlavour::Clustered).pairs();
    assert_eq!(clustered[..8], single.pairs()[..]);
    assert_eq!(clustered[8], ("insert_quorum", "auto".to_owned()));
    let names: Vec<&str> = clustered.iter().map(|(k, _)| *k).collect();
    assert_eq!(names, InsertSettings::NAMES);
}

#[test]
fn every_method_is_callable_through_the_bundle() {
    let stores = Stores::new(Arc::new(FakeStore::default()));
    let store = &stores.telemetry;
    let day = PartitionId::parse("20261117").unwrap();
    block_on(async {
        let token = token(TableId::SpanCore, 7);
        let probe = batch_probe(TableId::SpanCore, day, token.wal_epoch(), token.batch_seq());
        let probe = probe.unwrap();
        assert_eq!(store.probe(probe).await, Ok(false));
        let settings = InsertSettings::new(token, StoreFlavour::SingleNode);
        assert_eq!(store.insert_block(one_row_block(), settings).await, Ok(()));
        assert_eq!(store.insert_block(one_row_block(), settings).await, Ok(()));
        assert_eq!(store.probe(probe).await, Ok(true));
        let other = batch_probe(TableId::SpanCore, day, token.wal_epoch(), Lsn::new(8)).unwrap();
        assert_eq!(store.probe(other).await, Ok(false));

        let delete = DeletePlan::project(project(), TableId::SpanCore, day, Pred::all()).unwrap();
        let first = store.delete(delete).await.unwrap();
        assert_eq!(first.table(), TableId::SpanCore);
        assert_eq!(
            store.mutation_status(&first).await,
            Ok(MutationStatus::Pending)
        );
        let mask = store
            .apply_deleted_mask(TableId::SpanCore, day)
            .await
            .unwrap();
        assert_eq!(store.mutation_status(&mask).await, Ok(MutationStatus::Done));
        assert_eq!(store.masked_parts(TableId::SpanCore, day).await, Ok(0));
        let projects = store.partition_projects(TableId::SpanCore, day).await;
        assert_eq!(projects, Ok(vec![project().project_id()]));
        assert_eq!(store.drop_partition(TableId::SpanCore, day).await, Ok(()));
        assert_eq!(store.probe(probe).await, Ok(false));

        let step = |statement: &str| MigrationStep {
            id: SCHEMA_BASE,
            kind: StepKind::Schema,
            statements: vec![statement.to_owned()],
            settings: &[],
        };
        assert_eq!(store.apply_migration(&step("SELECT 1")).await, Ok(()));
        assert!(
            store
                .apply_migration(&step("SELECT 1; SELECT 2"))
                .await
                .is_err()
        );

        let facts = store.detect().await.unwrap();
        assert!(facts.version >= StoreVersion::MINIMUM);
        assert_eq!(facts.engines.len(), TABLES.len());
        assert_eq!(store.dedup_selftest().await, Ok(()));
        assert_eq!(store.health().await, StoreHealth::Healthy);
    });
}
