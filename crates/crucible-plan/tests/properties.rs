//! Properties of the plan IR and its builders (contract sections 8.1 and 8.3, invariant I6).

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support;
use crucible_plan::{
    AggOutput, Aggregate, BatchProbe, Caller, CmpOp, DeletePlan, DeleteTarget, Direction, Having,
    Literal, MAX_LIST, Measure, Node, Order, PlanError, PointLookup, Pred, ProbeKind, QueryClass,
    QueryPlan, Scan, SemiJoin, TRACE_LIST_MAX_LIMIT, TraceBounds, TraceListRequest, data_bounds,
    effective_window, pad_in_list, padded_in_len, span_children, trace_bounds_window, trace_hash,
    trace_list, trace_spans, trace_time_bounds,
};
use crucible_records::{
    CivilDate, Lsn, PartitionId, ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp,
    TraceId, Uuid, WalEpoch,
};
use crucible_schema::{
    Column, ColumnRef, DedupSelftestCol, FactCorrectionsCol, FactFlagsCol, FactScoresCol,
    FactTagsCol, McpContentCol, McpDeploysCol, McpOpsCol, McpToolInventoryCol, McpToolRollupCol,
    PROJECT_ID, RetentionSettings, ServiceIdentity, SessionRollupCol, SpanCoreCol, SpanMetricsCol,
    SpanPayloadCol, SqlProbeCol, TABLES, TableKind, TraceHeadsCol, TraceStatsCol, Ty,
    UserRollupCol, table,
};
use proptest::prelude::*;

/// `trace_hash` of sixteen zero bytes.
const HASH_ZERO: u32 = 3_344_075_112;
/// `trace_hash` of the bytes 0x00 to 0x0f.
const HASH_COUNTING: u32 = 4_128_731_355;

fn refs<C: Column>() -> Vec<ColumnRef> {
    C::ALL.iter().map(|c| c.column_ref()).collect()
}

/// Every column any plan can name, per table, in table-id order.
fn nameable() -> Vec<Vec<ColumnRef>> {
    vec![
        refs::<SpanCoreCol>(),
        refs::<SpanPayloadCol>(),
        refs::<McpOpsCol>(),
        refs::<McpContentCol>(),
        refs::<McpToolInventoryCol>(),
        refs::<FactScoresCol>(),
        refs::<FactTagsCol>(),
        refs::<FactFlagsCol>(),
        refs::<FactCorrectionsCol>(),
        refs::<TraceHeadsCol>(),
        refs::<TraceStatsCol>(),
        refs::<SessionRollupCol>(),
        refs::<UserRollupCol>(),
        refs::<SpanMetricsCol>(),
        refs::<McpToolRollupCol>(),
        refs::<McpDeploysCol>(),
        refs::<DedupSelftestCol>(),
        refs::<SqlProbeCol>(),
    ]
}

fn all_columns() -> Vec<ColumnRef> {
    nameable().into_iter().flatten().collect()
}

fn scope_strategy() -> impl Strategy<Value = ProjectScope> {
    any::<[u8; 16]>()
        .prop_filter("the nil project cannot be scoped", |b| *b != [0; 16])
        .prop_map(|b| test_support::scope(ProjectId::from_bytes(b)).unwrap())
}

fn window_strategy() -> impl Strategy<Value = TimeWindow> {
    let max = Timestamp::MAX_EXCLUSIVE.micros();
    (0..max - 1, 1..max).prop_map(move |(from, len)| {
        let to = from.saturating_add(len).min(max);
        TimeWindow::new(Timestamp::from_micros(from), Timestamp::from_micros(to)).unwrap()
    })
}

fn caller_strategy() -> impl Strategy<Value = Caller> {
    prop_oneof![Just(Caller::Session), Just(Caller::Agent)]
}

fn time_strategy() -> impl Strategy<Value = Timestamp> {
    (0..Timestamp::MAX_EXCLUSIVE.micros()).prop_map(Timestamp::from_micros)
}

fn literal_strategy() -> impl Strategy<Value = Literal> {
    prop_oneof![
        any::<[u8; 16]>().prop_map(|b| Literal::Uuid(Uuid::from_bytes(b))),
        prop::collection::vec(any::<u8>(), 0..34).prop_map(Literal::Bytes),
        prop_oneof![Just(8usize), Just(16), Just(32)]
            .prop_flat_map(|n| prop::collection::vec(any::<u8>(), n))
            .prop_map(Literal::Bytes),
        ".{0,8}".prop_map(Literal::String),
        any::<u64>().prop_map(Literal::UInt),
        (0u64..70_000).prop_map(Literal::UInt),
        any::<i64>().prop_map(Literal::Int),
        any::<f64>().prop_map(Literal::Float),
        time_strategy().prop_map(Literal::Time),
        (0i64..86_400 * 47_482).prop_map(|s| Literal::Time(Timestamp::from_micros(s * 1_000_000))),
        (0i64..47_482).prop_map(|d| Literal::Date(CivilDate::from_day_number(d))),
        (0u8..12).prop_map(Literal::Enum),
    ]
}

/// The reference for `Literal::fits`, written from the type strings rather than the type enum.
fn fits_by_type_string(literal: &Literal, ty: &Ty) -> bool {
    let wire = ty.wire().to_string();
    let wire = wire
        .strip_prefix("Nullable(")
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(&wire);
    match literal {
        Literal::Uuid(_) => wire == "UUID",
        Literal::Bytes(b) => wire == format!("FixedString({})", b.len()),
        Literal::String(_) => wire == "String",
        Literal::UInt(v) => match wire {
            "UInt8" => *v <= u64::from(u8::MAX),
            "UInt16" => *v <= u64::from(u16::MAX),
            "UInt32" => *v <= u64::from(u32::MAX),
            "UInt64" => true,
            _ => false,
        },
        Literal::Int(v) => match wire {
            "Int32" => i32::try_from(*v).is_ok(),
            "Int64" => true,
            _ => false,
        },
        Literal::Float(_) => wire == "Float32" || wire == "Float64",
        Literal::Time(t) => {
            wire == "DateTime64(6, 'UTC')"
                || (wire == "DateTime('UTC')" && t.micros() % 1_000_000 == 0)
        }
        Literal::Date(_) => wire == "Date",
        Literal::Enum(v) => {
            wire.starts_with("Enum8(")
                && (wire.contains(&format!("' = {v},")) || wire.ends_with(&format!("' = {v})")))
        }
    }
}

fn walk_preds<'a>(pred: &'a Pred, out: &mut Vec<&'a Pred>) {
    out.push(pred);
    match pred {
        Pred::And(operands) | Pred::Or(operands) => {
            operands.iter().for_each(|p| walk_preds(p, out));
        }
        Pred::Not(operand) => walk_preds(operand, out),
        _ => {}
    }
}

/// Every column a node names, and every predicate it holds.
fn names(node: &Node) -> (Vec<ColumnRef>, Vec<&Pred>) {
    let mut columns = Vec::new();
    let mut preds = Vec::new();
    match node {
        Node::Scan(scan) => {
            columns.extend_from_slice(scan.columns());
            if let Some(order) = scan.order() {
                columns.extend_from_slice(order.columns());
            }
            walk_preds(scan.pred(), &mut preds);
        }
        Node::Aggregate(aggregate) => {
            columns.extend_from_slice(aggregate.group());
            for measure in aggregate.measures() {
                match measure {
                    Measure::Count => {}
                    Measure::Sum(c)
                    | Measure::Min(c)
                    | Measure::Max(c)
                    | Measure::Uniq(c)
                    | Measure::HistogramMerge(c)
                    | Measure::PickMerge(c)
                    | Measure::LatestByVersion(c) => columns.push(*c),
                }
            }
            walk_preds(aggregate.pred(), &mut preds);
        }
        other => panic!("the first builders produce scans and aggregates, not {other:?}"),
    }
    for pred in &preds {
        match pred {
            Pred::Cmp(c, _, _) | Pred::In(c, _) | Pred::StartsWith(c, _) | Pred::HasAny(c, _) => {
                columns.push(*c);
            }
            _ => {}
        }
    }
    (columns, preds)
}

/// What every plan from a public builder must satisfy.
fn assert_scoped(
    plan: &QueryPlan,
    scope: ProjectScope,
    window: TimeWindow,
    caller: Caller,
    session_class: QueryClass,
    table_id: TableId,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(plan.scope(), scope);
    prop_assert_eq!(plan.window(), window);
    prop_assert!(!plan.scope().project_id().is_nil());
    match caller {
        Caller::Session => {
            prop_assert_eq!(plan.class(), session_class);
            prop_assert_eq!(plan.identity(), ServiceIdentity::ReadUi);
        }
        Caller::Agent => {
            prop_assert_eq!(plan.class(), QueryClass::Agent);
            prop_assert_eq!(plan.identity(), ServiceIdentity::ReadAgent);
        }
    }
    prop_assert_eq!(plan.root().tables(), vec![table_id]);
    let (columns, _) = names(plan.root());
    for column in columns {
        prop_assert_eq!(column.table(), table_id);
        prop_assert_ne!(column.name(), PROJECT_ID);
    }
    // Aggregating tables are only ever read through an aggregate.
    let aggregating = table(table_id).kind == TableKind::Aggregating;
    prop_assert_eq!(matches!(plan.root(), Node::Aggregate(_)), aggregating);
    Ok(())
}

#[test]
fn no_column_enum_exposes_project_id() {
    let nameable = nameable();
    assert_eq!(nameable.len(), TABLES.len());
    for (def, columns) in TABLES.iter().zip(&nameable) {
        assert!(columns.iter().all(|c| c.table() == def.id));
        assert!(
            columns.iter().all(|c| c.name() != PROJECT_ID),
            "{}",
            def.name()
        );
        assert!(def.key_columns().iter().all(|c| c.name() != PROJECT_ID));
        // The enum is everything else: the table's columns minus the scope column.
        let scope_columns = def.columns.iter().filter(|c| c.name == PROJECT_ID).count();
        assert_eq!(columns.len() + scope_columns, def.columns.len());
        assert_eq!(scope_columns, usize::from(def.id != TableId::DedupSelftest));
    }
}

#[test]
fn scans_and_lookups_refuse_aggregating_tables() {
    for (def, columns) in TABLES.iter().zip(nameable()) {
        let scan = Scan::new(def.id, columns.clone(), Pred::all());
        let lookup = PointLookup::new(
            def.id,
            [columns[0]],
            vec![vec![Literal::UInt(0)]],
            columns.clone(),
        );
        if !def.is_scoped() {
            // No project column, so no project predicate: not readable through a plan at all.
            let unscoped = PlanError::Table(def.id);
            assert_eq!(def.id, TableId::DedupSelftest);
            assert_eq!(scan, Err(unscoped.clone()));
            assert_eq!(lookup, Err(unscoped.clone()));
            let aggregate = Aggregate::new(def.id, Pred::all(), [columns[0]], vec![Measure::Count]);
            assert_eq!(aggregate, Err(unscoped));
            continue;
        }
        match def.kind {
            TableKind::Plain => assert!(scan.is_ok(), "{}", def.name()),
            TableKind::Aggregating => {
                assert_eq!(scan, Err(PlanError::AggregatingTable(def.id)));
                assert_eq!(lookup, Err(PlanError::AggregatingTable(def.id)));
                // Reading it through an aggregate over its key works.
                let dimensions: Vec<ColumnRef> = def
                    .key_columns()
                    .into_iter()
                    .filter(|c| !c.name().starts_with("marker_"))
                    .collect();
                let anchor = Measure::Max(dimensions[0]);
                assert!(
                    Aggregate::new(def.id, Pred::all(), dimensions.clone(), vec![anchor]).is_ok(),
                    "{}",
                    def.name()
                );
                // Neither a measure column nor a marker column is a dimension, and counting partial rows is
                // refused.
                for bad in columns
                    .iter()
                    .filter(|c| c.ty().agg_fn().is_some() || c.name().starts_with("marker_"))
                {
                    assert_eq!(
                        Aggregate::new(def.id, Pred::all(), [*bad], vec![anchor]),
                        Err(PlanError::GroupDimension(bad.name()))
                    );
                }
                assert_eq!(
                    Aggregate::new(def.id, Pred::all(), dimensions, vec![Measure::Count]),
                    Err(PlanError::Measure("count"))
                );
            }
        }
    }
}

#[test]
fn measures_must_match_their_partials() {
    let stats = |measure| {
        Aggregate::new(
            TableId::TraceStats,
            Pred::all(),
            [TraceStatsCol::TraceId],
            vec![measure],
        )
    };
    assert!(stats(Measure::Sum(TraceStatsCol::SpanCount.column_ref())).is_ok());
    assert!(stats(Measure::PickMerge(TraceStatsCol::UserIdPick.column_ref())).is_ok());
    assert!(stats(Measure::Min(TraceStatsCol::RowDay.column_ref())).is_ok());
    assert!(stats(Measure::Sum(TraceStatsCol::MinStart.column_ref())).is_err());
    assert!(stats(Measure::Max(TraceStatsCol::MinStart.column_ref())).is_err());
    assert!(stats(Measure::PickMerge(TraceStatsCol::MaxEnd.column_ref())).is_err());
    assert!(
        stats(Measure::HistogramMerge(
            TraceStatsCol::SpanCount.column_ref()
        ))
        .is_err()
    );
    assert!(
        stats(Measure::LatestByVersion(
            TraceStatsCol::SpanCount.column_ref()
        ))
        .is_err()
    );
    assert!(stats(Measure::Sum(SpanCoreCol::DurationUs.column_ref())).is_err());
    let metrics = Aggregate::new(
        TableId::SpanMetrics,
        Pred::all(),
        [SpanMetricsCol::Environment],
        vec![Measure::HistogramMerge(
            SpanMetricsCol::DurHist.column_ref(),
        )],
    );
    assert!(metrics.is_ok());
    let facts = |measure| {
        Aggregate::new(
            TableId::FactScores,
            Pred::all(),
            [FactScoresCol::TraceId],
            vec![measure],
        )
    };
    assert!(
        facts(Measure::LatestByVersion(
            FactScoresCol::ValueNum.column_ref()
        ))
        .is_ok()
    );
    assert!(facts(Measure::Count).is_ok());
    let core = |measure| {
        Aggregate::new(
            TableId::SpanCore,
            Pred::all(),
            [SpanCoreCol::ParentSpanId],
            vec![measure],
        )
    };
    assert!(core(Measure::Count).is_ok());
    assert!(core(Measure::Sum(SpanCoreCol::DurationUs.column_ref())).is_ok());
    assert!(core(Measure::Sum(SpanCoreCol::Name.column_ref())).is_err());
    assert!(core(Measure::Uniq(SpanCoreCol::Tags.column_ref())).is_err());
    assert!(core(Measure::LatestByVersion(SpanCoreCol::Name.column_ref())).is_err());
}

#[test]
fn special_predicates_need_their_columns() {
    let heads = |pred| Scan::new(TableId::TraceHeads, [TraceHeadsCol::SpanId], pred);
    let kv = Pred::MetadataEq {
        key: "k".into(),
        value: "v".into(),
    };
    assert!(Scan::new(TableId::SpanPayload, [SpanPayloadCol::SpanId], kv.clone()).is_ok());
    assert!(Scan::new(TableId::SpanCore, [SpanCoreCol::SpanId], kv).is_err());
    let text = Pred::TextMatch("checkout".into());
    assert!(Scan::new(TableId::SpanCore, [SpanCoreCol::SpanId], text.clone()).is_ok());
    assert!(heads(text.clone()).is_ok());
    assert!(Scan::new(TableId::McpOps, [McpOpsCol::SpanId], text).is_err());
    let has = |values| Pred::HasAny(TraceHeadsCol::Tags.into(), values);
    assert!(heads(has(vec!["a".into()])).is_ok());
    assert!(heads(has(vec![])).is_err());
    assert!(heads(has(vec![1u64.into()])).is_err());
    assert!(heads(Pred::eq(TraceHeadsCol::Tags, "a")).is_err());
    let prefix = |column: ColumnRef| Pred::StartsWith(column, "pro".into());
    assert!(heads(prefix(TraceHeadsCol::Environment.into())).is_ok());
    assert!(heads(prefix(TraceHeadsCol::TraceHash.into())).is_err());
    let long = Pred::is_in(TraceHeadsCol::UserId, vec!["u".into(); MAX_LIST + 1]);
    assert_eq!(heads(long), Err(PlanError::ListLength("user_id")));
    assert_eq!(
        heads(Pred::is_in(TraceHeadsCol::UserId, vec![])),
        Err(PlanError::ListLength("user_id"))
    );
}

#[test]
fn semi_joins_line_up_one_id_column() {
    let trace = TraceId::from_bytes([7; 16]);
    let left = || Scan::new(TableId::TraceHeads, [TraceHeadsCol::TraceId], Pred::all()).unwrap();
    let flagged = Aggregate::new(
        TableId::FactFlags,
        Pred::eq(FactFlagsCol::Key, "bookmarked"),
        [FactFlagsCol::TraceId],
        vec![Measure::LatestByVersion(FactFlagsCol::Value.column_ref())],
    )
    .unwrap();
    let right = || Node::Aggregate(flagged.clone());
    let join = SemiJoin::new(left(), TraceHeadsCol::TraceId, right(), 10_000).unwrap();
    assert_eq!(
        Node::SemiJoin(join).tables(),
        vec![TableId::TraceHeads, TableId::FactFlags]
    );
    // The id types must agree, the right side must produce one column, and the cap cannot be zero.
    assert_eq!(
        SemiJoin::new(left(), TraceHeadsCol::SpanId, right(), 10_000),
        Err(PlanError::SemiJoin)
    );
    assert_eq!(
        SemiJoin::new(left(), TraceHeadsCol::TraceId, right(), 0),
        Err(PlanError::SemiJoin)
    );
    let two = Scan::new(
        TableId::SpanCore,
        [SpanCoreCol::TraceId, SpanCoreCol::SpanId],
        Pred::eq(SpanCoreCol::TraceId, trace),
    )
    .unwrap();
    assert_eq!(
        SemiJoin::new(left(), TraceHeadsCol::TraceId, Node::Scan(two), 10_000),
        Err(PlanError::SemiJoin)
    );
}

#[test]
fn probes_and_deletes_are_typed() {
    let epoch = WalEpoch::from_bytes([1; 16]);
    let day = PartitionId::parse("20261117").unwrap();
    let month = PartitionId::parse("202611").unwrap();
    let all = PartitionId::UNPARTITIONED;
    let probe = |table_id, partition| {
        BatchProbe::for_shipper(table_id, partition, epoch, Lsn::new(7)).map(|p| p.kind())
    };
    assert_eq!(probe(TableId::SpanCore, day), Ok(ProbeKind::BatchSeq));
    assert_eq!(probe(TableId::TraceStats, day), Ok(ProbeKind::Marker));
    assert_eq!(probe(TableId::SpanMetrics, month), Ok(ProbeKind::Marker));
    assert_eq!(probe(TableId::McpDeploys, all), Ok(ProbeKind::Marker));
    assert_eq!(
        probe(TableId::SpanCore, month),
        Err(PlanError::Partition(TableId::SpanCore))
    );
    assert_eq!(
        probe(TableId::SpanMetrics, day),
        Err(PlanError::Partition(TableId::SpanMetrics))
    );
    assert_eq!(
        probe(TableId::McpDeploys, day),
        Err(PlanError::Partition(TableId::McpDeploys))
    );
    assert_eq!(
        probe(TableId::SqlProbe, all),
        Err(PlanError::Table(TableId::SqlProbe))
    );
    assert_eq!(
        probe(TableId::DedupSelftest, all),
        Err(PlanError::Table(TableId::DedupSelftest))
    );

    let scope = test_support::scope(ProjectId::from_bytes([9; 16])).unwrap();
    let cut = Timestamp::from_micros(1_794_907_800_000_000);
    let by_arrival = || Pred::cmp(SpanCoreCol::ReceivedAt, CmpOp::Lt, cut);
    let delete = DeletePlan::project(scope, TableId::SpanCore, day, by_arrival()).unwrap();
    assert_eq!(delete.target(), DeleteTarget::Project(scope));
    assert_eq!(
        (delete.table(), delete.partition()),
        (TableId::SpanCore, day)
    );
    assert_eq!(delete.pred(), &by_arrival());
    assert!(DeletePlan::project(scope, TableId::SpanCore, month, by_arrival()).is_err());
    assert!(DeletePlan::project(scope, TableId::McpOps, day, by_arrival()).is_err());
    assert!(DeletePlan::project(scope, TableId::DedupSelftest, all, Pred::all()).is_err());
    let purge = DeletePlan::marker_purge(TableId::SpanMetrics, month, cut, Pred::all()).unwrap();
    assert_eq!(
        purge.target(),
        DeleteTarget::MarkerPurge { older_than: cut }
    );
    assert!(DeletePlan::marker_purge(TableId::SpanCore, day, cut, Pred::all()).is_err());
}

#[test]
fn trace_hash_is_the_low_half_of_xxh3() {
    // The published XXH3-64 value of the empty input anchors the hash function itself.
    assert_eq!(xxhash_rust::xxh3::xxh3_64(&[]), 0x2D06_8005_38D3_94C2);
    // Known answers for the shipper to match: sixteen zero bytes, and the bytes 0x00 to 0x0f.
    let counting = TraceId::from_bytes(core::array::from_fn(|i| i as u8));
    assert_eq!(trace_hash(TraceId::ZERO), HASH_ZERO);
    assert_eq!(trace_hash(counting), HASH_COUNTING);
    for id in [TraceId::ZERO, counting] {
        let full = xxhash_rust::xxh3::xxh3_64(id.as_bytes());
        assert_eq!(u64::from(trace_hash(id)), full & 0xffff_ffff);
    }
}

#[test]
fn trace_bounds_include_a_span_that_starts_at_the_end() {
    let (start, end) = (Timestamp::from_micros(1_000), Timestamp::from_micros(5_000));
    let window = trace_bounds_window(start, end).unwrap();
    assert!(window.contains(start) && window.contains(end));
    assert!(!window.contains(Timestamp::from_micros(5_001)));
    // A trace of one instantaneous span still has a window.
    let instant = trace_bounds_window(start, start).unwrap();
    assert!(instant.contains(start));
    assert_eq!(instant.len_micros(), 1);
    assert_eq!(trace_bounds_window(end, start), Err(PlanError::Bounds));
    let max = Timestamp::from_micros(i64::MAX);
    assert_eq!(trace_bounds_window(start, max), Err(PlanError::Bounds));

    let scope = test_support::scope(ProjectId::from_bytes([9; 16])).unwrap();
    let trace_id = TraceId::from_bytes([3; 16]);
    let plan = trace_spans(scope, start, start, Caller::Session, trace_id).unwrap();
    assert_eq!(plan.window(), instant);
    let reversed = trace_spans(scope, end, start, Caller::Session, trace_id);
    assert_eq!(reversed, Err(PlanError::Bounds));
    let trace = TraceBounds {
        trace_id,
        min_start: start,
        max_end: end,
    };
    let children = span_children(scope, trace, Caller::Agent, SpanId::ZERO, None, None).unwrap();
    assert_eq!(children.window(), window);
}

#[test]
fn having_values_are_typed_by_their_measure() {
    let aggregate = || {
        Aggregate::new(
            TableId::TraceStats,
            Pred::all(),
            [TraceStatsCol::TraceId],
            vec![
                Measure::Sum(TraceStatsCol::TotalCostNusd.column_ref()),
                Measure::Sum(TraceStatsCol::ErrorCount.column_ref()),
                Measure::Max(TraceStatsCol::MaxEnd.column_ref()),
                Measure::PickMerge(TraceStatsCol::UserIdPick.column_ref()),
            ],
        )
        .unwrap()
    };
    let having = |measure, value| {
        aggregate().having(vec![Having {
            measure,
            op: CmpOp::Gt,
            value,
        }])
    };
    assert!(having(0, Literal::Int(5)).is_ok());
    assert!(having(1, Literal::UInt(0)).is_ok());
    assert!(having(2, Literal::Time(Timestamp::EPOCH)).is_ok());
    assert_eq!(
        having(0, Literal::UInt(5)),
        Err(PlanError::LiteralType("total_cost_nusd"))
    );
    assert_eq!(
        having(1, Literal::String("x".into())),
        Err(PlanError::LiteralType("error_count"))
    );
    assert_eq!(
        having(2, Literal::UInt(1)),
        Err(PlanError::LiteralType("max_end"))
    );
    assert_eq!(
        having(3, Literal::String("x".into())),
        Err(PlanError::LiteralType("user_id_pick"))
    );
    assert_eq!(having(4, Literal::UInt(1)), Err(PlanError::OutputIndex));
    let ordered = |output| aggregate().ordered(vec![(output, Direction::Desc)]);
    assert!(ordered(AggOutput::Measure(3)).is_ok());
    assert!(ordered(AggOutput::Group(0)).is_ok());
    assert_eq!(ordered(AggOutput::Group(1)), Err(PlanError::OutputIndex));
    assert_eq!(aggregate().limited(0), Err(PlanError::Limit));
}

#[test]
fn windows_are_clamped_to_retention() {
    let day = 86_400_000_000i64;
    let at = |d: i64| Timestamp::from_micros(d * day);
    let window = TimeWindow::new(at(100), at(200)).unwrap();
    let settings = RetentionSettings {
        retention_days: Some(30),
        payload_retention_days: Some(7),
        mcp_content_retention_days: None,
    };
    let now = at(190).saturating_add_micros(12_345);
    let clamp = |table_id| effective_window(table_id, window, &settings, now).unwrap();
    assert_eq!(
        clamp(TableId::SpanCore),
        TimeWindow::new(at(160), at(200)).ok()
    );
    assert_eq!(
        clamp(TableId::SpanPayload),
        TimeWindow::new(at(183), at(200)).ok()
    );
    assert_eq!(
        clamp(TableId::McpContent),
        TimeWindow::new(at(160), at(200)).ok()
    );
    assert_eq!(clamp(TableId::SqlProbe), Some(window));
    let expired = effective_window(TableId::SpanCore, window, &settings, at(400)).unwrap();
    assert_eq!(expired, None);
    let forever = RetentionSettings::default();
    assert_eq!(
        effective_window(TableId::SpanCore, window, &forever, now),
        Ok(Some(window))
    );
}

#[test]
fn padding_edges() {
    assert_eq!(pad_in_list(&[]), Vec::<Literal>::new());
    for (len, padded) in [
        (0, 1),
        (1, 1),
        (2, 2),
        (3, 4),
        (4, 4),
        (5, 8),
        (1000, 1024),
        (MAX_LIST, 16_384),
    ] {
        assert_eq!(padded_in_len(len), padded);
    }
}

proptest! {
    #[test]
    fn every_built_plan_carries_its_scope_and_window(
        scope in scope_strategy(),
        window in window_strategy(),
        caller in caller_strategy(),
        trace in any::<[u8; 16]>(),
        parent in any::<[u8; 8]>(),
        limit in 1..=TRACE_LIST_MAX_LIMIT,
        child_limit in prop::option::of(1u32..=2_000),
        cursor in prop::option::of((time_strategy(), any::<u32>(), any::<[u8; 16]>())),
        child_cursor in prop::option::of((time_strategy(), any::<[u8; 8]>())),
        as_of in time_strategy(),
        since in prop::option::of(time_strategy()),
        user in ".{0,12}",
    ) {
        let trace_id = TraceId::from_bytes(trace);
        let parent = SpanId::from_bytes(parent);
        let hash = Pred::Cmp(SpanCoreCol::TraceHash.into(), CmpOp::Eq, trace_hash(trace_id).into());

        let bounds = trace_time_bounds(scope, window, caller, trace_id).unwrap();
        assert_scoped(&bounds, scope, window, caller, QueryClass::Point, TableId::TraceStats)?;
        let (_, preds) = names(bounds.root());
        prop_assert!(preds.contains(&&Pred::eq(TraceStatsCol::TraceId, trace_id)));

        // The bounds of a trace whose last span starts at the last instant of the window.
        let last = Timestamp::from_micros(window.to().micros() - 1);
        let spans = trace_spans(scope, window.from(), last, caller, trace_id).unwrap();
        assert_scoped(&spans, scope, window, caller, QueryClass::Point, TableId::SpanCore)?;
        let (_, preds) = names(spans.root());
        prop_assert!(preds.contains(&&Pred::eq(SpanCoreCol::TraceId, trace_id)));
        prop_assert!(preds.contains(&&hash));

        let request = TraceListRequest {
            filter: Pred::eq(TraceHeadsCol::UserId, user.as_str()),
            limit,
            cursor: cursor.map(|(t, h, id)| (t, h, TraceId::from_bytes(id))),
            as_of,
            since,
        };
        let list = trace_list(scope, window, caller, request).unwrap();
        assert_scoped(&list, scope, window, caller, QueryClass::List, TableId::TraceHeads)?;
        let Node::Scan(scan) = list.root() else { panic!("a scan") };
        prop_assert_eq!(scan.limit(), Some(limit * 3));
        prop_assert_eq!(scan.arrival().as_of, Some(as_of));
        prop_assert_eq!(scan.arrival().since, since);
        let order = scan.order().unwrap();
        prop_assert_eq!(order.direction(), Direction::Desc);
        let key: Vec<&str> = order.columns().iter().map(|c| c.name()).collect();
        prop_assert_eq!(key, ["start_time", "trace_hash", "trace_id"]);
        prop_assert_eq!(scan.cursor().is_some(), cursor.is_some());

        let child_cursor = child_cursor.map(|(t, id)| (t, SpanId::from_bytes(id)));
        let trace_bounds = TraceBounds { trace_id, min_start: window.from(), max_end: last };
        let children =
            span_children(scope, trace_bounds, caller, parent, child_cursor, child_limit).unwrap();
        assert_scoped(&children, scope, window, caller, QueryClass::Point, TableId::SpanCore)?;
        let Node::Scan(scan) = children.root() else { panic!("a scan") };
        prop_assert_eq!(scan.limit(), Some(child_limit.unwrap_or(500) + 1));
        prop_assert_eq!(scan.cursor().is_some(), child_cursor.is_some());
        let (_, preds) = names(children.root());
        prop_assert!(preds.contains(&&Pred::eq(SpanCoreCol::ParentSpanId, parent)));
        prop_assert!(preds.contains(&&Pred::eq(SpanCoreCol::TraceId, trace_id)));
        prop_assert!(preds.contains(&&hash));

        let data = data_bounds(scope, window, caller).unwrap();
        assert_scoped(&data.spans, scope, window, caller, QueryClass::Analytics, TableId::SpanMetrics)?;
        assert_scoped(&data.mcp, scope, window, caller, QueryClass::Analytics, TableId::McpToolRollup)?;
    }

    #[test]
    fn page_sizes_outside_the_caps_are_refused(
        scope in scope_strategy(),
        window in window_strategy(),
        limit in prop_oneof![Just(0u32), 201u32..10_000],
        child_limit in prop_oneof![Just(0u32), 2_001u32..100_000],
    ) {
        let request = TraceListRequest {
            filter: Pred::all(),
            limit,
            cursor: None,
            as_of: Timestamp::EPOCH,
            since: None,
        };
        prop_assert_eq!(trace_list(scope, window, Caller::Session, request), Err(PlanError::Limit));
        let trace = TraceBounds {
            trace_id: TraceId::ZERO,
            min_start: window.from(),
            max_end: window.from(),
        };
        let children =
            span_children(scope, trace, Caller::Session, SpanId::ZERO, None, Some(child_limit));
        prop_assert_eq!(children, Err(PlanError::Limit));
    }

    /// A comparison is accepted exactly when the literal has the column's type and the column is not an
    /// aggregated measure, for every column of every table.
    #[test]
    fn comparisons_are_typed(index in any::<prop::sample::Index>(), literal in literal_strategy()) {
        let columns = all_columns();
        let column = columns[index.index(columns.len())];
        let composite = matches!(column.ty().scalar(), Ty::Array(_) | Ty::Tuple2(..));
        let fits = !composite && fits_by_type_string(&literal, column.ty());
        prop_assert_eq!(literal.fits(column.ty()), fits);
        let def = table(column.table());
        let expected = fits && column.ty().agg_fn().is_none() && def.is_scoped();
        let accepts = |pred: Pred| match def.kind {
            TableKind::Plain => Scan::new(def.id, [column], pred).is_ok(),
            TableKind::Aggregating => {
                let anchor = Measure::Max(def.key_columns()[0]);
                Aggregate::new(def.id, pred, Vec::<ColumnRef>::new(), vec![anchor]).is_ok()
            }
        };
        prop_assert_eq!(accepts(Pred::Cmp(column, CmpOp::Eq, literal.clone())), expected);
        prop_assert_eq!(accepts(Pred::is_in(column, vec![literal.clone(), literal])), expected);
    }

    /// A node never accepts a column of another table, in its outputs, its predicate or its order.
    #[test]
    fn foreign_columns_are_refused(a in any::<prop::sample::Index>(), b in any::<prop::sample::Index>()) {
        let columns = all_columns();
        let (own, other) = (columns[a.index(columns.len())], columns[b.index(columns.len())]);
        prop_assume!(own.table() != other.table());
        prop_assume!(table(own.table()).kind == TableKind::Plain && table(own.table()).is_scoped());
        let foreign = PlanError::ForeignColumn { table: own.table(), column: other.name() };
        prop_assert_eq!(Scan::new(own.table(), [own, other], Pred::all()), Err(foreign.clone()));
        let pred = Pred::Not(Box::new(Pred::Or(vec![Pred::StartsWith(other, "x".into())])));
        prop_assert_eq!(Scan::new(own.table(), [own], pred), Err(foreign.clone()));
        let order = Order::by([other], Direction::Asc);
        let scan = Scan::new(own.table(), [own], Pred::all()).unwrap();
        prop_assert_eq!(scan.ordered(order), Err(foreign));
    }

    /// A cursor must give one value of the right type per order column.
    #[test]
    fn cursors_match_the_order(
        start in time_strategy(),
        hash in any::<u32>(),
        trace in any::<[u8; 16]>(),
        drop in 0usize..3,
    ) {
        let scan = || {
            Scan::new(TableId::TraceHeads, [TraceHeadsCol::TraceId], Pred::all())
                .unwrap()
                .ordered(Order::key(TableId::TraceHeads, Direction::Desc))
                .unwrap()
        };
        let full: Vec<Literal> = vec![start.into(), hash.into(), TraceId::from_bytes(trace).into()];
        prop_assert!(scan().after(full.clone()).is_ok());
        let mut short = full.clone();
        short.remove(drop);
        prop_assert_eq!(scan().after(short), Err(PlanError::Cursor));
        let mut swapped = full;
        swapped.swap(0, 2);
        prop_assert_eq!(scan().after(swapped), Err(PlanError::Cursor));
        let unordered = Scan::new(TableId::TraceHeads, [TraceHeadsCol::TraceId], Pred::all()).unwrap();
        prop_assert_eq!(unordered.after(vec![start.into()]), Err(PlanError::Cursor));
    }

    /// `In` lists are padded to the next power of two without changing the set of values.
    #[test]
    fn in_lists_pad_to_a_power_of_two(values in prop::collection::vec(any::<u64>(), 1..300)) {
        let list: Vec<Literal> = values.iter().copied().map(Literal::UInt).collect();
        let padded = pad_in_list(&list);
        let n = padded.len();
        prop_assert_eq!(n, padded_in_len(list.len()));
        prop_assert!(n.is_power_of_two());
        prop_assert!(n >= list.len() && n < 2 * list.len());
        prop_assert_eq!(&padded[..list.len()], &list[..]);
        prop_assert!(padded[list.len()..].iter().all(|v| Some(v) == list.last()));
        // A padded length is its own padded length: one statement shape per arity class.
        prop_assert_eq!(padded_in_len(n), n);
    }
}
