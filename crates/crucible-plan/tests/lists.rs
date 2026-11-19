//! The list builders of the sessions, users and filters slice: the filtered trace list and the sessions of one
//! user (contract 8.7 and 8.8). Their output schemas, scope, window, class and retention clamp.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_auth::test_support;
use crucible_plan::{
    Caller, Filter, Measure, Node, OutputSchema, PlanError, Pred, QueryClass, QueryPlan,
    RetentionClamp, TRACE_LIST_MAX_LIMIT, TextField, TraceListRequest, USER_SESSIONS_MAX_LIMIT,
    UserSessionsRequest, effective_window, filtered_trace_list, trace_list, user_sessions,
};
use crucible_records::{
    Level, ObservationType, ProjectId, ProjectScope, TableId, TimeWindow, Timestamp, TraceId,
};
use crucible_schema::{RetentionSettings, TraceHeadsCol};
use proptest::prelude::*;

fn scope(n: u8) -> ProjectScope {
    test_support::scope(ProjectId::from_bytes([n; 16])).unwrap()
}

fn window() -> TimeWindow {
    let from = Timestamp::from_micros(1_794_900_000_000_000);
    TimeWindow::new(from, Timestamp::from_micros(1_794_907_800_000_000)).unwrap()
}

/// A project that keeps its data forever: the clamp leaves every window as it is.
fn keep() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), window().to())
}

fn shape(output: &OutputSchema) -> Vec<(String, String)> {
    output
        .columns()
        .iter()
        .map(|c| (c.name.clone(), c.ty.to_string()))
        .collect()
}

fn request(limit: u32) -> TraceListRequest {
    TraceListRequest {
        filter: Pred::all(),
        limit,
        cursor: None,
        as_of: window().to(),
        since: None,
    }
}

fn types(values: &[ObservationType]) -> Filter {
    Filter::ObservationType(values.to_vec())
}

fn metadata() -> Filter {
    Filter::Metadata {
        key: "team".to_owned(),
        value: "search".to_owned(),
    }
}

#[test]
fn a_filter_on_heads_only_is_the_trace_list_with_its_predicate() {
    let filter = Filter::And(vec![
        Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
        Filter::Level(vec![Level::Error]),
    ]);
    let filtered = filtered_trace_list(
        scope(1),
        window(),
        &keep(),
        Caller::Session,
        request(50),
        &filter,
    )
    .unwrap()
    .unwrap();
    let plain = trace_list(
        scope(1),
        window(),
        &keep(),
        Caller::Session,
        TraceListRequest {
            filter: filter
                .pred(crucible_plan::FilterTarget::TraceHeads)
                .unwrap(),
            ..request(50)
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(filtered, plain);
    // The empty filter is the unfiltered list, and a request predicate is kept beside the filter's.
    let all = filtered_trace_list(
        scope(1),
        window(),
        &keep(),
        Caller::Session,
        request(50),
        &Filter::all(),
    );
    let unfiltered = trace_list(scope(1), window(), &keep(), Caller::Session, request(50));
    assert_eq!(all, unfiltered);
    let extra = Pred::eq(TraceHeadsCol::UserId, "u1");
    let both = filtered_trace_list(
        scope(1),
        window(),
        &keep(),
        Caller::Session,
        TraceListRequest {
            filter: extra.clone(),
            ..request(50)
        },
        &filter,
    )
    .unwrap()
    .unwrap();
    let Node::Scan(scan) = both.root() else {
        panic!("a scan")
    };
    let Pred::And(parts) = scan.pred() else {
        panic!("a conjunction")
    };
    assert!(parts.contains(&extra), "{parts:?}");
}

#[test]
fn span_conditions_semi_join_the_heads_into_the_trace_ids_of_one_span_table() {
    let heads_shape = shape(
        &trace_list(scope(1), window(), &keep(), Caller::Session, request(10))
            .unwrap()
            .unwrap()
            .output(),
    );
    for (filter, table) in [
        (types(&[ObservationType::Tool]), TableId::SpanCore),
        (
            Filter::And(vec![
                types(&[ObservationType::Generation]),
                Filter::Text(TextField::UserId, vec!["u1".to_owned()]),
            ]),
            TableId::SpanCore,
        ),
        (metadata(), TableId::SpanPayload),
        // A condition that heads can evaluate stays on the heads, beside a metadata condition.
        (
            Filter::And(vec![metadata(), Filter::Name("checkout".to_owned())]),
            TableId::SpanPayload,
        ),
    ] {
        let plan = filtered_trace_list(
            scope(1),
            window(),
            &keep(),
            Caller::Session,
            request(10),
            &filter,
        )
        .unwrap()
        .unwrap();
        assert_eq!(shape(&plan.output()), heads_shape, "{filter:?}");
        assert_eq!(plan.class(), QueryClass::List);
        assert_eq!(plan.root().tables(), vec![TableId::TraceHeads, table]);
        let Node::SemiJoin(join) = plan.root() else {
            panic!("a semi-join for {filter:?}")
        };
        assert_eq!(join.on().name(), "trace_id");
        assert_eq!(join.max_ids(), 10_000);
        assert_eq!(join.left().limit(), Some(30));
        let Node::Aggregate(ids) = join.right() else {
            panic!("an aggregate of ids")
        };
        assert_eq!(ids.table(), table);
        let group: Vec<&str> = ids.group().iter().map(|c| c.name()).collect();
        assert_eq!(group, ["trace_id"]);
        assert_eq!(ids.measures(), &[Measure::Count]);
    }
}

#[test]
fn span_conditions_that_no_one_table_holds_are_refused() {
    for filter in [
        Filter::And(vec![metadata(), types(&[ObservationType::Tool])]),
        Filter::Or(vec![metadata(), types(&[ObservationType::Tool])]),
        Filter::Or(vec![
            metadata(),
            Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
        ]),
    ] {
        let refused = filtered_trace_list(
            scope(1),
            window(),
            &keep(),
            Caller::Session,
            request(10),
            &filter,
        );
        assert!(
            matches!(refused, Err(PlanError::UnsupportedFilter { .. })),
            "{filter:?}: {refused:?}"
        );
    }
}

#[test]
fn the_sessions_of_a_user_come_from_its_trace_heads() {
    let plan = user_sessions(
        scope(1),
        window(),
        &keep(),
        Caller::Session,
        UserSessionsRequest {
            user_id: "u1".to_owned(),
            limit: 25,
            cursor: Some((window().from(), "s1".to_owned())),
        },
    )
    .unwrap()
    .unwrap();
    let dt64 = "DateTime64(6, 'UTC')".to_owned();
    assert_eq!(
        shape(&plan.output()),
        [
            ("session_id".to_owned(), "String".to_owned()),
            ("max_start_time".to_owned(), dt64.clone()),
            ("min_start_time".to_owned(), dt64),
            ("uniq_trace_id".to_owned(), "UInt64".to_owned()),
        ]
    );
    let Node::Aggregate(aggregate) = plan.root() else {
        panic!("an aggregate")
    };
    assert_eq!(aggregate.table(), TableId::TraceHeads);
    assert_eq!(aggregate.limit(), Some(26));
    assert_eq!(aggregate.cursor().map(<[_]>::len), Some(2));
    let Pred::And(parts) = aggregate.pred() else {
        panic!("a conjunction")
    };
    assert!(parts.contains(&Pred::eq(TraceHeadsCol::UserId, "u1")));
    for limit in [0, USER_SESSIONS_MAX_LIMIT + 1] {
        let refused = user_sessions(
            scope(1),
            window(),
            &keep(),
            Caller::Session,
            UserSessionsRequest {
                user_id: "u1".to_owned(),
                limit,
                cursor: None,
            },
        );
        assert_eq!(refused, Err(PlanError::Limit));
    }
}

fn window_strategy() -> impl Strategy<Value = TimeWindow> {
    (0i64..4_000_000_000_000_000, 1i64..40 * 86_400_000_000).prop_map(|(from, len)| {
        TimeWindow::new(
            Timestamp::from_micros(from),
            Timestamp::from_micros(from + len),
        )
        .unwrap()
    })
}

proptest! {
    /// Every plan carries its scope, the window clamped to the retention of every table it reads, the class of
    /// its caller, and no plan at all once the window has expired.
    #[test]
    fn the_builders_carry_scope_class_and_the_clamped_window(
        project in 1u8..=255,
        window in window_strategy(),
        now in 0i64..4_000_000_000_000_000,
        days in prop::option::of(0u32..800),
        payload_days in prop::option::of(0u32..800),
        agent in any::<bool>(),
        limit in 1..=TRACE_LIST_MAX_LIMIT,
        user in ".{0,12}",
        cursor in prop::option::of((0i64..4_000_000_000_000_000, any::<[u8; 16]>())),
    ) {
        let settings = RetentionSettings {
            retention_days: days,
            payload_retention_days: payload_days.map(|p| days.map_or(p, |d| p.min(d))),
            ..RetentionSettings::default()
        };
        let now = Timestamp::from_micros(now);
        let clamp = RetentionClamp::new(settings, now);
        let caller = if agent { Caller::Agent } else { Caller::Session };
        let class = if agent { QueryClass::Agent } else { QueryClass::List };
        let scope = scope(project);
        let cursor = cursor.map(|(t, id)| {
            let trace_id = TraceId::from_bytes(id);
            (Timestamp::from_micros(t), trace_id.trace_hash(), trace_id)
        });
        let list = |filter: &Filter| {
            let request = TraceListRequest { cursor, ..request(limit) };
            filtered_trace_list(scope, window, &clamp, caller, request, filter).unwrap()
        };
        let clamped = |tables: &[TableId]| {
            let mut out = Some(window);
            for table in tables {
                out = out.and_then(|w| effective_window(*table, w, &settings, now).unwrap());
            }
            out
        };
        for (filter, tables) in [
            (Filter::all(), vec![TableId::TraceHeads]),
            (Filter::Text(TextField::SessionId, vec![user.clone()]), vec![TableId::TraceHeads]),
            (types(&[ObservationType::Agent]), vec![TableId::TraceHeads, TableId::SpanCore]),
            (metadata(), vec![TableId::TraceHeads, TableId::SpanPayload]),
        ] {
            let plan: Option<QueryPlan> = list(&filter);
            prop_assert_eq!(plan.as_ref().map(QueryPlan::window), clamped(&tables));
            if let Some(plan) = plan {
                prop_assert_eq!(plan.scope(), scope);
                prop_assert_eq!(plan.class(), class);
                prop_assert_eq!(plan.root().tables(), tables);
            }
        }
        let sessions = user_sessions(
            scope,
            window,
            &clamp,
            caller,
            UserSessionsRequest {
                user_id: user.clone(),
                limit: limit.min(USER_SESSIONS_MAX_LIMIT),
                cursor: cursor.map(|(t, _, _)| (t, user.clone())),
            },
        )
        .unwrap();
        prop_assert_eq!(sessions.as_ref().map(QueryPlan::window), clamped(&[TableId::TraceHeads]));
        if let Some(plan) = sessions {
            prop_assert_eq!(plan.scope(), scope);
            prop_assert_eq!(plan.class(), class);
        }
    }
}
