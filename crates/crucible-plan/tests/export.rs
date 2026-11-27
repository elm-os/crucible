//! The export plans (contracts 8.4, 8.7 and 12.8): every plan runs in class `export` as the session identity,
//! carries its scope and its window clamped to retention, and returns every column of its table in the table's
//! order; the page scan reads in the key order of `span_core` under the export's `as_of`.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support;
use crucible_plan::{
    Direction, EXPORT_LOOKUP_SPANS, EXPORT_PAGE_MAX, ExportSelection, FactTable, MAX_LIST, Node,
    PlanError, QueryClass, QueryPlan, RetentionClamp, effective_window, export_facts,
    export_mcp_content, export_mcp_ops, export_payloads, export_spans, export_ties,
};
use crucible_records::{ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{
    Column, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol, McpContentCol, McpOpsCol,
    PROJECT_ID, RetentionSettings, ServiceIdentity, SpanCoreCol, SpanPayloadCol,
};
use proptest::prelude::*;

fn names<C: Column>() -> Vec<String> {
    C::ALL
        .iter()
        .map(|c| c.column_ref().name().to_owned())
        .collect()
}

fn output(plan: &QueryPlan) -> Vec<String> {
    plan.output()
        .columns()
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

fn scope_strategy() -> impl Strategy<Value = ProjectScope> {
    any::<[u8; 16]>()
        .prop_filter("the nil project cannot be scoped", |b| *b != [0; 16])
        .prop_map(|b| test_support::scope(ProjectId::from_bytes(b)).unwrap())
}

fn time_strategy() -> impl Strategy<Value = Timestamp> {
    (0..Timestamp::MAX_EXCLUSIVE.micros()).prop_map(Timestamp::from_micros)
}

fn window_strategy() -> impl Strategy<Value = TimeWindow> {
    let max = Timestamp::MAX_EXCLUSIVE.micros();
    (0..max - 1, 1..max).prop_map(move |(from, len)| {
        let to = from.saturating_add(len).min(max);
        TimeWindow::new(Timestamp::from_micros(from), Timestamp::from_micros(to)).unwrap()
    })
}

/// A span key of a page: `(start_time, trace_id, span_id)`.
fn span_strategy() -> impl Strategy<Value = (Timestamp, TraceId, SpanId)> {
    (time_strategy(), any::<[u8; 16]>(), any::<[u8; 8]>())
        .prop_map(|(t, trace, span)| (t, TraceId::from_bytes(trace), SpanId::from_bytes(span)))
}

fn keep() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), Timestamp::EPOCH)
}

/// What every export plan shares: scope, class, identity and no column named `project_id`.
fn assert_export(
    plan: &QueryPlan,
    scope: ProjectScope,
    table_id: TableId,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(plan.scope(), scope);
    prop_assert_eq!(plan.class(), QueryClass::Export);
    prop_assert_eq!(plan.identity(), ServiceIdentity::ReadUi);
    prop_assert_eq!(plan.root().tables(), vec![table_id]);
    prop_assert!(!output(plan).iter().any(|name| name == PROJECT_ID));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn every_export_plan_is_scoped_windowed_and_in_class_export(
        scope in scope_strategy(),
        window in window_strategy(),
        as_of in time_strategy(),
        limit in 1..=EXPORT_PAGE_MAX,
        after in prop::option::of((time_strategy(), any::<u32>(), time_strategy(), any::<[u8; 8]>())),
        spans in prop::collection::vec(span_strategy(), 1..8),
        traces in prop::collection::vec(any::<[u8; 16]>(), 0..4),
        user in prop::option::of(".{0,8}"),
    ) {
        let trace_ids: Vec<TraceId> = traces.into_iter().map(TraceId::from_bytes).collect();
        let selected = ExportSelection { trace_ids: &trace_ids, user_id: user.as_deref(), session_id: None };
            // A position comes from a stored row, whose `start_minute` is a whole minute.
        let after = after.map(|(m, h, t, s)| (m.floor_minute(), h, t, SpanId::from_bytes(s)));

        let page = export_spans(scope, window, &keep(), selected, as_of, after, limit).unwrap().unwrap();
        assert_export(&page, scope, TableId::SpanCore)?;
        prop_assert_eq!(page.window(), window);
        prop_assert_eq!(output(&page), names::<SpanCoreCol>());
        let Node::Scan(scan) = page.root() else { panic!("a scan") };
        prop_assert_eq!(scan.limit(), Some(limit));
        prop_assert_eq!(scan.arrival().as_of, Some(as_of));
        prop_assert_eq!(scan.arrival().since, None);
        prop_assert_eq!(scan.cursor().is_some(), after.is_some());
        let order = scan.order().unwrap();
        prop_assert_eq!(order.direction(), Direction::Asc);
        let key: Vec<&str> = order.columns().iter().map(|c| c.name()).collect();
        prop_assert_eq!(key, ["start_minute", "trace_hash", "start_time", "span_id"]);

        if let Some(key) = after {
            let ties = export_ties(scope, window, &keep(), selected, as_of, key).unwrap().unwrap();
            assert_export(&ties, scope, TableId::SpanCore)?;
            prop_assert_eq!(output(&ties), names::<SpanCoreCol>());
            let Node::Scan(scan) = ties.root() else { panic!("a scan") };
            prop_assert_eq!(scan.limit(), None);
            prop_assert_eq!(scan.arrival().as_of, Some(as_of));
        }

        // The lookups read the window of the page's spans: from the earliest start to just after the latest.
        let from = spans.iter().map(|s| s.0).min().unwrap();
        let last = spans.iter().map(|s| s.0).max().unwrap();
        let lookups = [
            (export_payloads(scope, &keep(), &spans), TableId::SpanPayload, names::<SpanPayloadCol>()),
            (export_mcp_ops(scope, &keep(), &spans), TableId::McpOps, names::<McpOpsCol>()),
            (export_mcp_content(scope, &keep(), &spans), TableId::McpContent, names::<McpContentCol>()),
        ];
        for (plan, table_id, columns) in lookups {
            match last.checked_add_micros(1) {
                Some(to) if to <= Timestamp::MAX_EXCLUSIVE => {
                    let plan = plan.unwrap().unwrap();
                    assert_export(&plan, scope, table_id)?;
                    prop_assert_eq!(plan.window(), TimeWindow::new(from, to).unwrap());
                    prop_assert_eq!(output(&plan), columns);
                    let Node::PointLookup(lookup) = plan.root() else { panic!("a point lookup") };
                    prop_assert!(lookup.keys().len() >= spans.len());
                }
                _ => prop_assert!(plan.is_err()),
            }
        }

        let ids: Vec<TraceId> = spans.iter().map(|s| s.1).collect();
        let facts = [
            (FactTable::Scores, names::<FactScoresCol>()),
            (FactTable::Tags, names::<FactTagsCol>()),
            (FactTable::Flags, names::<FactFlagsCol>()),
            (FactTable::Corrections, names::<FactCorrectionsCol>()),
        ];
        for (table, mut columns) in facts {
            let plan = export_facts(scope, window, &keep(), table, &ids).unwrap().unwrap();
            assert_export(&plan, scope, table.table())?;
            prop_assert_eq!(plan.window(), window);
            // The renderer adds the project predicate; the output names every other column.
            columns.retain(|name| name != PROJECT_ID);
            prop_assert_eq!(output(&plan), columns);
        }
    }

    #[test]
    fn every_export_plan_clamps_its_window_to_retention(
        scope in scope_strategy(),
        window in window_strategy(),
        now in time_strategy(),
        days in prop::option::of(0u32..800),
        spans in prop::collection::vec(span_strategy(), 1..4),
    ) {
        let settings = RetentionSettings {
            retention_days: days,
            payload_retention_days: days,
            mcp_content_retention_days: days,
        };
        let clamp = RetentionClamp::new(settings, now);
        let none = ExportSelection::default();
        let ids: Vec<TraceId> = spans.iter().map(|s| s.1).collect();
        let windowed = [
            (export_spans(scope, window, &clamp, none, now, None, 10).unwrap(), TableId::SpanCore),
            (export_facts(scope, window, &clamp, FactTable::Tags, &ids).unwrap(), TableId::FactTags),
        ];
        for (plan, table_id) in windowed {
            let expected = effective_window(table_id, window, &settings, now).unwrap();
            prop_assert_eq!(plan.map(|plan| plan.window()), expected);
        }
        if let Some(to) = spans.iter().map(|s| s.0).max().unwrap().checked_add_micros(1) {
            let from = spans.iter().map(|s| s.0).min().unwrap();
            if let Ok(window) = TimeWindow::new(from, to) {
                let expected = effective_window(TableId::SpanPayload, window, &settings, now).unwrap();
                let plan = export_payloads(scope, &clamp, &spans).unwrap();
                prop_assert_eq!(plan.map(|plan| plan.window()), expected);
            }
        }
    }
}

#[test]
fn the_page_and_lookup_sizes_are_bounded() {
    let scope = test_support::scope(ProjectId::from_bytes([1; 16])).unwrap();
    let window = TimeWindow::new(Timestamp::EPOCH, Timestamp::from_micros(1)).unwrap();
    let none = ExportSelection::default();
    for limit in [0, EXPORT_PAGE_MAX + 1] {
        let refused = export_spans(scope, window, &keep(), none, Timestamp::EPOCH, None, limit);
        assert!(matches!(refused, Err(PlanError::Limit)), "{limit}");
    }
    assert!(matches!(
        export_payloads(scope, &keep(), &[]),
        Err(PlanError::Limit)
    ));
    let span = (
        Timestamp::EPOCH,
        TraceId::from_bytes([1; 16]),
        SpanId::from_bytes([1; 8]),
    );
    // A lookup names at most EXPORT_LOOKUP_SPANS spans, four bound values each.
    assert!(
        export_payloads(scope, &keep(), &vec![span; EXPORT_LOOKUP_SPANS])
            .unwrap()
            .is_some()
    );
    let many = vec![span; EXPORT_LOOKUP_SPANS + 1];
    for refused in [
        export_payloads(scope, &keep(), &many),
        export_mcp_ops(scope, &keep(), &many),
        export_mcp_content(scope, &keep(), &many),
    ] {
        assert!(matches!(refused, Err(PlanError::Limit)));
    }
    let traces = vec![TraceId::from_bytes([2; 16]); MAX_LIST + 1];
    let selected = ExportSelection {
        trace_ids: &traces,
        user_id: None,
        session_id: None,
    };
    let refused = export_spans(scope, window, &keep(), selected, Timestamp::EPOCH, None, 1);
    assert!(matches!(refused, Err(PlanError::Limit)));
}
