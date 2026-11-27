//! The field budget (contract 8.3): every plan the builders of `crucible-plan` make within their documented caps,
//! every delete the lifecycle sends and the existence probe render a request that ClickHouse accepts. A request
//! carries the statement text, the query id, the output format, the class settings, the statement's own settings
//! and every bound value, and the server refuses one with more than `http_max_fields` (default 1,000) fields
//! before the statement starts. The store refuses a request above `render::FIELD_BUDGET` before it sends it, so
//! a plan above it would fail every time.
//!
//! Each plan is checked as product code sends it: with the most values that pending deletions add to a read
//! (`HIDDEN_TRACE_IDS` deletions of one trace each and `HIDDEN_REQUESTS` deletions by user and by session, the
//! costliest spread of the hiding budget), and for a plan read on every replica, on a cluster as well. Lists are
//! drawn up to their caps; a list is padded to the next power of two, so the field count only grows with a
//! list's length and the caps are the worst cases.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use std::collections::BTreeMap;

use crucible_auth::test_support::scope;
use crucible_plan::DeletePlan;
use crucible_plan::test_support::batch_probe;
use crucible_plan::{
    Aggregation, Caller, CaptureClass, DimensionId, ExportSelection, FILTER_VALUES_MAX_LIMIT,
    FactTable, FactsOf, Fill, Filter, FilterField, FilterValuesRequest, Granularity,
    HIDDEN_REQUESTS, HIDDEN_TRACE_IDS, Hidden, HiddenSelector, Literal, MAX_FILTER_VALUES,
    MAX_LIST, MAX_SNIPPET_TOKENS, MCP_LIST_MAX_LIMIT, MCP_USER_ERRORS_PER_READ, METRICS_MAX_SERIES,
    McpContentField, McpOperationsRequest, McpOpsFilter, McpRead, McpSelection, McpUserKey,
    MeasureId, MetricsQuery, PayloadField, PayloadRangeRequest, PlanError, Pred, QueryClass,
    QueryPlan, RetentionClamp, SEARCH_MAX_LIMIT, SEARCH_SPANS_PER_LOOKUP, SESSION_LIST_MAX_LIMIT,
    SPAN_CHILDREN_MAX_LIMIT, SPAN_LIST_MAX_LIMIT, SearchField, SearchRequest, SessionListRequest,
    Source, SourceChoice, SpanListRequest, Step, TRACE_ID_LIST_CHUNK, TRACE_LIST_MAX_LIMIT,
    TRACE_LIST_MAX_TAGS, TRACE_LIST_MAX_VALUES, TRACE_PAGED_SPAN_LIMIT, TextField, TokenMode,
    TraceBounds, TraceListRequest, TraceSelector, USER_LIST_MAX_LIMIT, USER_SESSIONS_MAX_LIMIT,
    UserListRequest, UserSessionsRequest, child_counts, data_bounds, deletion_issuers,
    deletion_traces, expired_before, export_facts, export_spans, export_ties, fact_page, fact_rows,
    filter_values, filtered_trace_list, lifecycle_column, lifecycle_oldest, mcp_client_versions,
    mcp_clients, mcp_compare_tools, mcp_deploys, mcp_error_breakdown, mcp_error_classes,
    mcp_inventory, mcp_metrics_other, mcp_metrics_ranking, mcp_metrics_series, mcp_metrics_totals,
    mcp_operation, mcp_operation_chain, mcp_operation_content, mcp_operation_peers, mcp_operations,
    mcp_presence, mcp_servers, mcp_session_errors, mcp_sessions, mcp_tool_clients, mcp_tool_series,
    mcp_tools, mcp_user, mcp_user_errors, mcp_users, metrics_other, metrics_ranking,
    metrics_series, metrics_totals, payload_range, search, search_spans, session_detail,
    session_list, span_bodies, span_children, span_list, spans_of_parents, trace_list,
    trace_page_stats, trace_roots, trace_spans, trace_time_bounds, user_detail, user_list,
    user_session_counts, user_sessions,
};
use crucible_records::{
    CaptureSource, CivilDate, ErrorClass, Level, Lsn, McpSide, ObservationType, PartitionId,
    ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId, WalEpoch,
};
use crucible_schema::{Ident, RetentionSettings};
use crucible_store_clickhouse::render::{self, FIELD_BUDGET};
use crucible_store_clickhouse::{DELETE_SETTINGS, INACTIVE_REPLICAS};
use proptest::prelude::*;

const T0: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);
const HOUR: i64 = 3_600_000_000;

fn project() -> ProjectScope {
    scope(ProjectId::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b").unwrap()).unwrap()
}

/// An hour and a microsecond.
fn hour() -> TimeWindow {
    TimeWindow::new(T0, T0.saturating_add_micros(HOUR + 1)).unwrap()
}

/// Seven days from half past an hour: the MCP rollup reads take their whole hours from the hourly tier, which
/// renders their predicate in both branches of a union.
fn week() -> TimeWindow {
    TimeWindow::new(T0, T0.saturating_add_micros(7 * 24 * HOUR)).unwrap()
}

fn clamp() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), T0)
}

fn trace(n: usize) -> TraceId {
    let mut bytes = [0xa5; 16];
    bytes[8..].copy_from_slice(&(n as u64).to_be_bytes());
    TraceId::from_bytes(bytes)
}

fn span(n: usize) -> SpanId {
    let mut bytes = [0x5a; 8];
    bytes[4..].copy_from_slice(&(n as u32).to_be_bytes());
    SpanId::from_bytes(bytes)
}

fn texts(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}-{i}")).collect()
}

fn bounds() -> TraceBounds {
    TraceBounds {
        trace_id: trace(0),
        min_start: T0,
        max_end: T0.saturating_add_micros(HOUR),
    }
}

/// The pending deletions that add the most values to a read.
fn worst_hidden() -> Vec<Hidden> {
    let mut hidden: Vec<Hidden> = (0..HIDDEN_TRACE_IDS)
        .map(|n| Hidden {
            selector: HiddenSelector::Traces(vec![trace(10_000 + n)]),
            requested_at: T0,
        })
        .collect();
    hidden.extend((0..HIDDEN_REQUESTS).map(|n| Hidden {
        selector: if n % 2 == 0 {
            HiddenSelector::User(format!("deleted-user-{n}"))
        } else {
            HiddenSelector::Session(format!("deleted-session-{n}"))
        },
        requested_at: T0,
    }));
    hidden
}

/// What a check found: the plans above the budget as they are built, which must be none, and the plans above it
/// once the costliest pending deletions are added to them, with their largest field count.
#[derive(Default)]
struct Budget {
    over: Vec<String>,
    over_hidden: BTreeMap<String, usize>,
    checked: usize,
}

impl Budget {
    /// Checks the plan of a builder that must succeed at its caps.
    fn plan(&mut self, name: &str, plan: Result<Option<QueryPlan>, PlanError>) {
        let plan = plan
            .unwrap_or_else(|error| panic!("{name}: the builder refused its caps: {error}"))
            .unwrap_or_else(|| panic!("{name}: the window expired"));
        self.check(name, &plan);
    }

    fn check(&mut self, name: &str, plan: &QueryPlan) {
        self.checked += 1;
        let fields = |plan: &QueryPlan| {
            render::read_fields(plan)
                .unwrap_or_else(|error| panic!("{name}: the renderer refused it: {error}"))
        };
        let built = fields(plan);
        if built > FIELD_BUDGET {
            self.over.push(format!(
                "{name} ({}): {built} fields",
                render::describe(plan)
            ));
        }
        let hidden = fields(&plan.clone().hiding(&worst_hidden()));
        if hidden > FIELD_BUDGET {
            let most = self.over_hidden.entry(name.to_owned()).or_default();
            *most = (*most).max(hidden);
        }
    }

    /// Every plan fits as it is built; under the costliest pending deletions, exactly the plans of `hidden` do
    /// not, each at most at its count.
    fn finish(self, hidden: &[(&str, usize)]) {
        assert!(
            self.over.is_empty(),
            "{} of {} plans carry more than {FIELD_BUDGET} fields:
{}",
            self.over.len(),
            self.checked,
            self.over.join(
                "
"
            )
        );
        let expected: BTreeMap<String, usize> = hidden
            .iter()
            .map(|(name, fields)| ((*name).to_owned(), *fields))
            .collect();
        assert_eq!(
            self.over_hidden, expected,
            "the plans above {FIELD_BUDGET} fields under the costliest pending deletions"
        );
    }
}

/// Every condition of the list filter at its caps (`MAX_FILTER_VALUES` values per member) that `target` takes.
fn filter_at_caps(text: &[TextField], tags: usize, metadata: bool, types: bool) -> Filter {
    let mut all: Vec<Filter> = text
        .iter()
        .map(|field| Filter::Text(*field, texts(&format!("{field:?}"), MAX_FILTER_VALUES)))
        .collect();
    all.push(Filter::Level(Level::ALL.to_vec()));
    if types {
        all.push(Filter::ObservationType(ObservationType::ALL.to_vec()));
    }
    if tags > 0 {
        all.push(Filter::Tags(texts("tag", tags)));
    }
    if metadata {
        all.extend((0..MAX_FILTER_VALUES).map(|n| Filter::Metadata {
            key: format!("key-{n}"),
            value: format!("value-{n}"),
        }));
    }
    all.push(Filter::Name("checkout".to_owned()));
    Filter::And(all)
}

/// A trace list filter: `text[i]` values in the i-th text member, `tags` tags, every level, and either observation
/// types or `metadata` entries (the trace list tests one span table).
fn trace_filter(text: &[usize; 5], tags: usize, types: bool, metadata: usize) -> Filter {
    let mut all: Vec<Filter> = TEXT_FIELDS
        .iter()
        .zip(text)
        .filter(|(_, n)| **n > 0)
        .map(|(field, n)| Filter::Text(*field, texts(&format!("{field:?}"), *n)))
        .collect();
    all.push(Filter::Level(Level::ALL.to_vec()));
    if types {
        all.push(Filter::ObservationType(ObservationType::ALL.to_vec()));
    }
    if tags > 0 {
        all.push(Filter::Tags(texts("tag", tags)));
    }
    all.extend((0..metadata).map(|n| Filter::Metadata {
        key: format!("key-{n}"),
        value: format!("value-{n}"),
    }));
    all.push(Filter::Name("checkout".to_owned()));
    Filter::And(all)
}

const TEXT_FIELDS: [TextField; 5] = [
    TextField::Environment,
    TextField::Release,
    TextField::Version,
    TextField::UserId,
    TextField::SessionId,
];

#[test]
fn trace_plans_fit_one_request() {
    let (s, c) = (project(), clamp());
    let mut budget = Budget::default();
    for caller in [Caller::Session, Caller::Agent] {
        budget.plan(
            "trace_time_bounds",
            trace_time_bounds(s, hour(), &c, caller, trace(0)),
        );
        budget.plan(
            "trace_spans",
            trace_spans(s, T0, T0.saturating_add_micros(HOUR), &c, caller, trace(0)),
        );
        let request = |cursor, since| TraceListRequest {
            filter: Pred::all(),
            limit: TRACE_LIST_MAX_LIMIT,
            cursor,
            as_of: T0,
            since,
        };
        let cursor = Some((T0, trace(1).trace_hash(), trace(1)));
        budget.plan(
            "trace_list",
            trace_list(s, hour(), &c, caller, request(cursor, None)),
        );
        budget.plan(
            "trace_list since",
            trace_list(s, hour(), &c, caller, request(None, Some(T0))),
        );
        // The trace list filters its heads and semi-joins the conditions of one span table: metadata or
        // observation types, never both. Its members hold at most TRACE_LIST_MAX_VALUES values together,
        // spread here so that padding costs the most.
        for types in [true, false] {
            let filter = trace_filter(&[65, 65, 65, 41, 0], TRACE_LIST_MAX_TAGS, types, 0);
            assert_eq!(filter.values(), TRACE_LIST_MAX_VALUES);
            budget.plan(
                "filtered_trace_list",
                filtered_trace_list(s, hour(), &c, caller, request(cursor, None), &filter),
            );
        }
        let filter = trace_filter(&[65, 65, 65, 0, 0], TRACE_LIST_MAX_TAGS, false, 41);
        budget.plan(
            "filtered_trace_list",
            filtered_trace_list(s, hour(), &c, caller, request(cursor, None), &filter),
        );
        let over = trace_filter(&[65, 65, 65, 42, 0], TRACE_LIST_MAX_TAGS, false, 0);
        assert_eq!(
            filtered_trace_list(s, hour(), &c, caller, request(cursor, None), &over),
            Err(PlanError::ListLength("filter"))
        );
        let ids: Vec<TraceId> = (0..MAX_LIST).map(trace).collect();
        budget.plan(
            "trace_page_stats",
            trace_page_stats(s, hour(), &c, caller, &ids),
        );
        let parents: Vec<SpanId> = (0..MAX_LIST).map(span).collect();
        budget.plan(
            "span_children",
            span_children(
                s,
                bounds(),
                &c,
                caller,
                span(1),
                Some((T0, span(2))),
                Some(SPAN_CHILDREN_MAX_LIMIT),
            ),
        );
        budget.plan(
            "child_counts",
            child_counts(s, bounds(), &c, caller, &parents),
        );
        budget.plan("trace_roots", trace_roots(s, bounds(), &c, caller));
        budget.plan(
            "spans_of_parents",
            spans_of_parents(s, bounds(), &c, caller, &parents, TRACE_PAGED_SPAN_LIMIT),
        );
        for table in FactTable::ALL {
            budget.plan(
                "fact_rows of traces",
                fact_rows(s, hour(), &c, caller, table, FactsOf::Traces(&ids)),
            );
            budget.plan(
                "fact_rows of a trace tree",
                fact_rows(s, hour(), &c, caller, table, FactsOf::TraceTree(&ids)),
            );
            budget.plan(
                "fact_rows of a span",
                fact_rows(
                    s,
                    hour(),
                    &c,
                    caller,
                    table,
                    FactsOf::Span(trace(0), span(0)),
                ),
            );
            budget.plan(
                "fact_rows of a session",
                fact_rows(s, hour(), &c, caller, table, FactsOf::Session("session")),
            );
            budget.plan("fact_page", fact_page(s, hour(), &c, caller, table, &ids));
        }
    }
    // Under the costliest pending deletions the filtered trace list, which carries them on its heads and on its
    // span table, goes above the budget; the store then refuses it as too broad.
    budget.finish(&[("filtered_trace_list", 1_515)]);
}

#[test]
fn span_and_search_plans_fit_one_request() {
    let (s, c) = (project(), clamp());
    let mut budget = Budget::default();
    for caller in [Caller::Session, Caller::Agent] {
        // The span list tests every field on `span_core` and semi-joins its metadata into `span_payload`.
        let filter = filter_at_caps(&TEXT_FIELDS, MAX_FILTER_VALUES, true, true);
        budget.plan(
            "span_list",
            span_list(
                s,
                hour(),
                &c,
                caller,
                SpanListRequest {
                    filter,
                    limit: SPAN_LIST_MAX_LIMIT,
                    cursor: Some((T0, span(1))),
                    as_of: T0,
                },
            ),
        );
        // Search: the hits restricted by the payload table's conditions, then a lookup of the hits' spans under
        // the rest of the filter.
        let filter = filter_at_caps(&TEXT_FIELDS, MAX_FILTER_VALUES, true, true);
        for (mode, fields) in [
            (
                TokenMode::All,
                vec![SearchField::Input, SearchField::Output],
            ),
            (
                TokenMode::Phrase,
                vec![SearchField::Input, SearchField::Output],
            ),
            (TokenMode::Any, vec![SearchField::Metadata]),
        ] {
            // A metadata term is an entry, `key=value`.
            let tokens = if fields.contains(&SearchField::Metadata) {
                (0..MAX_SNIPPET_TOKENS)
                    .map(|n| format!("key-{n}=value-{n}"))
                    .collect()
            } else {
                texts("token", MAX_SNIPPET_TOKENS)
            };
            let request = SearchRequest {
                tokens,
                mode,
                fields,
                filter: filter.clone(),
                limit: SEARCH_MAX_LIMIT,
                cursor: Some((T0, 7, span(1))),
                as_of: T0,
            };
            let planned = search(s, hour(), &c, caller, &request).unwrap().unwrap();
            budget.check("search hits", &planned.hits);
            let hits: Vec<(Timestamp, u32, SpanId)> = (0..SEARCH_SPANS_PER_LOOKUP)
                .map(|n| {
                    (
                        T0.saturating_add_micros(n as i64 * 60_000_000),
                        n as u32,
                        span(n),
                    )
                })
                .collect();
            budget.plan(
                "search_spans",
                search_spans(s, hour(), &c, caller, &hits, &planned.rest),
            );
        }
        budget.plan(
            "payload_range",
            payload_range(
                s,
                &c,
                caller,
                PayloadRangeRequest {
                    trace_id: trace(0),
                    span_id: span(0),
                    start_time: T0,
                    field: PayloadField::Output,
                    offset: 4_096,
                    len: 65_536,
                },
            ),
        );
        budget.plan(
            "span_bodies",
            span_bodies(s, &c, caller, trace(0), span(0), T0),
        );
    }
    // Under the costliest pending deletions the span list, which carries them on `span_core` and on
    // `span_payload`, and the lookup of a search's hits go above the budget; the store then refuses them as too
    // broad.
    budget.finish(&[("search_spans", 1_061), ("span_list", 1_557)]);
}

#[test]
fn session_user_and_value_plans_fit_one_request() {
    let (s, c) = (project(), clamp());
    let mut budget = Budget::default();
    for caller in [Caller::Session, Caller::Agent] {
        budget.plan(
            "session_list",
            session_list(
                s,
                hour(),
                &c,
                caller,
                SessionListRequest {
                    limit: SESSION_LIST_MAX_LIMIT,
                    cursor: Some((T0, "session".to_owned())),
                },
            ),
        );
        budget.plan(
            "session_detail",
            session_detail(s, hour(), &c, caller, "session"),
        );
        budget.plan(
            "user_list",
            user_list(
                s,
                hour(),
                &c,
                caller,
                UserListRequest {
                    limit: USER_LIST_MAX_LIMIT,
                    cursor: Some((T0, "user".to_owned())),
                },
            ),
        );
        budget.plan("user_detail", user_detail(s, hour(), &c, caller, "user"));
        budget.plan(
            "user_sessions",
            user_sessions(
                s,
                hour(),
                &c,
                caller,
                UserSessionsRequest {
                    user_id: "user".to_owned(),
                    limit: USER_SESSIONS_MAX_LIMIT,
                    cursor: Some((T0, "session".to_owned())),
                },
            ),
        );
        budget.plan(
            "user_session_counts",
            user_session_counts(s, hour(), &c, caller, &texts("user", MAX_LIST)),
        );
        let bounds = data_bounds(s, hour(), &c, caller).unwrap();
        for plan in [bounds.spans, bounds.mcp].into_iter().flatten() {
            budget.check("data_bounds", &plan);
        }
        for field in FilterField::ALL {
            let request = FilterValuesRequest {
                source: Source::Spans,
                field,
                prefix: Some("prefix".to_owned()),
                limit: FILTER_VALUES_MAX_LIMIT,
            };
            if let Ok(Some(plan)) = filter_values(s, hour(), &c, caller, &request) {
                budget.check("filter_values", &plan);
            }
        }
    }
    budget.finish(&[]);
}

/// A metrics query at the caps of its source: the most measures and dimensions, every filter field of the source
/// at its most values.
fn metrics_query(source: Source) -> MetricsQuery {
    match source {
        Source::McpOps => MetricsQuery {
            source,
            measures: vec![(MeasureId::McpErrorRate, Aggregation::Rate)],
            dimensions: vec![DimensionId::McpServer, DimensionId::McpTarget],
            filter: Filter::all(),
            granularity: Granularity::Auto,
            fill: Fill::Zero,
            top_n: Some(METRICS_MAX_SERIES),
        },
        Source::Traces => MetricsQuery {
            source,
            measures: vec![(MeasureId::Traces, Aggregation::Sum)],
            dimensions: Vec::new(),
            filter: Filter::all(),
            granularity: Granularity::Auto,
            fill: Fill::Zero,
            top_n: None,
        },
        Source::Spans => MetricsQuery {
            source,
            measures: vec![
                (MeasureId::Count, Aggregation::Sum),
                (MeasureId::Duration, Aggregation::P95),
                (MeasureId::TotalCost, Aggregation::Sum),
                (MeasureId::Duration, Aggregation::P50),
            ],
            dimensions: vec![DimensionId::Model, DimensionId::Environment],
            filter: Filter::And(vec![
                Filter::Text(
                    TextField::Environment,
                    texts("environment", MAX_FILTER_VALUES),
                ),
                Filter::Text(TextField::Release, texts("release", MAX_FILTER_VALUES)),
                Filter::Level(Level::ALL.to_vec()),
                Filter::ObservationType(ObservationType::ALL.to_vec()),
            ]),
            granularity: Granularity::Auto,
            fill: Fill::Zero,
            top_n: Some(METRICS_MAX_SERIES),
        },
    }
}

/// The keys of the series a metrics query ranked: at most `METRICS_MAX_SERIES`.
fn series_keys(width: usize) -> Vec<Vec<Literal>> {
    (0..METRICS_MAX_SERIES as usize)
        .map(|n| {
            (0..width)
                .map(|d| Literal::String(format!("key-{n}-{d}")))
                .collect()
        })
        .collect()
}

#[test]
fn metrics_plans_fit_one_request() {
    let (s, c) = (project(), clamp());
    let mut budget = Budget::default();
    for caller in [Caller::Session, Caller::Agent] {
        for window in [hour(), week()] {
            let spans = metrics_query(Source::Spans);
            let keys = series_keys(2);
            budget.plan(
                "metrics_ranking",
                metrics_ranking(s, window, &c, caller, &spans).map_err(|e| panic!("{e}")),
            );
            budget.plan(
                "metrics_series",
                metrics_series(s, window, &c, caller, &spans, Some(&keys))
                    .map_err(|e| panic!("{e}")),
            );
            budget.plan(
                "metrics_other",
                metrics_other(s, window, &c, caller, &spans, &keys).map_err(|e| panic!("{e}")),
            );
            // An alert reads one measure, whole or by a dimension.
            let alert = MetricsQuery {
                measures: vec![(MeasureId::Duration, Aggregation::P95)],
                ..spans.clone()
            };
            budget.plan(
                "metrics_totals",
                metrics_totals(s, window, &c, &alert, true, METRICS_MAX_SERIES + 1)
                    .map_err(|e| panic!("{e}")),
            );
            let traces = metrics_query(Source::Traces);
            budget.plan(
                "metrics_series of traces",
                metrics_series(s, window, &c, caller, &traces, None).map_err(|e| panic!("{e}")),
            );
        }
    }
    budget.finish(&[]);
}

/// How many MCP servers per capture class the selection names. A project's servers have no documented cap; the
/// automatic capture-source selection names every server present in the window, per class.
const MCP_SERVERS_PER_CLASS: usize = 64;

fn mcp_selection(choice: SourceChoice) -> McpSelection {
    let mut presence = Vec::new();
    for (class, source) in [
        (0, CaptureSource::Sdk),
        (1, CaptureSource::ProxyStdio),
        (2, CaptureSource::Otlp),
    ] {
        presence
            .extend((0..MCP_SERVERS_PER_CLASS).map(|n| (format!("server-{class}-{n}"), source)));
    }
    McpSelection::select(choice, presence)
}

#[test]
fn mcp_plans_fit_one_request() {
    let (s, c) = (project(), clamp());
    let mut budget = Budget::default();
    for (caller, window, choice) in [
        (Caller::Session, hour(), SourceChoice::Auto),
        (Caller::Agent, week(), SourceChoice::Auto),
        (
            Caller::Session,
            week(),
            SourceChoice::Class(CaptureClass::Proxy),
        ),
    ] {
        let selection = mcp_selection(choice);
        for side in [McpSide::Client, McpSide::Server] {
            let read = McpRead {
                side,
                selection: &selection,
                include_caller_faults: true,
                server: Some("server-0-1"),
            };
            budget.plan(
                "mcp_presence",
                mcp_presence(
                    s,
                    window,
                    &c,
                    caller.class(QueryClass::Analytics),
                    side,
                    Some("server-0-1"),
                ),
            );
            budget.plan("mcp_servers", mcp_servers(s, window, &c, caller, &read));
            budget.plan("mcp_tools", mcp_tools(s, window, &c, caller, &read));
            budget.plan(
                "mcp_tool_series",
                mcp_tool_series(s, window, &c, caller, &read, "tool", Step::Hour),
            );
            budget.plan(
                "mcp_tool_clients",
                mcp_tool_clients(s, window, &c, caller, &read, "tool"),
            );
            budget.plan(
                "mcp_error_classes",
                mcp_error_classes(s, window, &c, caller, &read),
            );
            budget.plan("mcp_clients", mcp_clients(s, window, &c, caller, &read));
            budget.plan(
                "mcp_compare_tools",
                mcp_compare_tools(s, window, &c, caller, &read, &texts("deploy", 2)),
            );
            budget.plan(
                "mcp_deploys",
                mcp_deploys(s, window, &c, caller, Some("server-0-1")),
            );
            budget.plan(
                "mcp_inventory",
                mcp_inventory(
                    s,
                    window,
                    &c,
                    caller,
                    Some("server-0-1"),
                    &texts("deploy", 2),
                ),
            );
            let limit = MCP_LIST_MAX_LIMIT;
            budget.plan(
                "mcp_error_breakdown",
                mcp_error_breakdown(s, window, &c, caller, &read, limit),
            );
            budget.plan(
                "mcp_client_versions",
                mcp_client_versions(s, window, &c, caller, &read, limit),
            );
            budget.plan("mcp_users", mcp_users(s, window, &c, caller, &read, limit));
            let users: Vec<McpUserKey> = (0..MCP_USER_ERRORS_PER_READ)
                .map(|n| McpUserKey {
                    user_id: format!("user-{n}"),
                    subject_hash: [n as u8; 16],
                })
                .collect();
            budget.plan(
                "mcp_user_errors",
                mcp_user_errors(s, window, &c, caller, &read, &users),
            );
            budget.plan(
                "mcp_sessions",
                mcp_sessions(s, window, &c, caller, &read, limit),
            );
            budget.plan(
                "mcp_session_errors",
                mcp_session_errors(
                    s,
                    window,
                    &c,
                    caller,
                    &read,
                    &texts("group", MCP_LIST_MAX_LIMIT as usize),
                ),
            );
            let request = McpOperationsRequest {
                filter: McpOpsFilter {
                    target: Some("tool".to_owned()),
                    method: Some("tools/call".to_owned()),
                    client: Some("client".to_owned()),
                    error_class: Some(ErrorClass::Timeout),
                    errors_only: true,
                    session: Some("group".to_owned()),
                    user: Some("ab".repeat(16)),
                },
                limit,
                cursor: Some((T0, span(1))),
                as_of: T0,
            };
            budget.plan(
                "mcp_operations",
                mcp_operations(s, window, &c, caller, &read, &request),
            );
            budget.plan(
                "mcp_operation",
                mcp_operation(s, window, &c, caller, side, (trace(0), span(0))),
            );
            budget.plan(
                "mcp_operation_content",
                mcp_operation_content(
                    s,
                    window,
                    &c,
                    caller,
                    (side, trace(0), span(0)),
                    McpContentField::Result,
                    16_384,
                ),
            );
            budget.plan(
                "mcp_operation_peers",
                mcp_operation_peers(s, window, &c, caller, side, (trace(0), span(0), span(1))),
            );
            budget.plan(
                "mcp_operation_chain",
                mcp_operation_chain(s, window, &c, caller, side, (trace(0), CaptureSource::Sdk)),
            );
            let query = metrics_query(Source::McpOps);
            // An additive measure has an other series; a rate has none.
            let additive = MetricsQuery {
                measures: vec![(MeasureId::McpErrorCount, Aggregation::Sum)],
                ..query.clone()
            };
            let keys = series_keys(2);
            budget.plan(
                "mcp_metrics_ranking",
                mcp_metrics_ranking(s, window, &c, caller, &query, &read)
                    .map_err(|e| panic!("{e}")),
            );
            budget.plan(
                "mcp_metrics_series",
                mcp_metrics_series(s, window, &c, caller, &query, (&read, Some(&keys)))
                    .map_err(|e| panic!("{e}")),
            );
            budget.plan(
                "mcp_metrics_other",
                mcp_metrics_other(s, window, &c, caller, &additive, (&read, &keys))
                    .map_err(|e| panic!("{e}")),
            );
            budget.plan(
                "mcp_metrics_totals",
                mcp_metrics_totals(s, window, &c, &query, &read, (true, METRICS_MAX_SERIES + 1))
                    .map_err(|e| panic!("{e}")),
            );
        }
    }
    budget.finish(&[]);
}

#[test]
fn export_plans_fit_one_request() {
    let (s, c) = (project(), clamp());
    let mut budget = Budget::default();
    let ids: Vec<TraceId> = (0..MAX_LIST).map(trace).collect();
    let selected = ExportSelection {
        trace_ids: &ids,
        user_id: Some("user"),
        session_id: Some("session"),
    };
    let key = (T0, 7, T0.saturating_add_micros(5), span(3));
    budget.plan(
        "export_spans",
        export_spans(s, hour(), &c, selected, T0, Some(key), 1_000),
    );
    budget.plan("export_ties", export_ties(s, hour(), &c, selected, T0, key));
    for table in FactTable::ALL {
        budget.plan("export_facts", export_facts(s, hour(), &c, table, &ids));
    }
    budget.finish(&[]);
}

#[test]
fn lifecycle_plans_fit_one_request() {
    let s = project();
    let mut budget = Budget::default();
    let ids: Vec<TraceId> = (0..TRACE_ID_LIST_CHUNK).map(trace).collect();
    let hashes: Vec<[u8; 16]> = (0..TRACE_ID_LIST_CHUNK)
        .map(|n| {
            let mut hash = [0x3c; 16];
            hash[8..].copy_from_slice(&(n as u64).to_be_bytes());
            hash
        })
        .collect();
    for selector in [
        TraceSelector::Traces(&ids),
        TraceSelector::User("user"),
        TraceSelector::Session("session"),
        TraceSelector::McpUser("user", &hashes),
    ] {
        budget.plan(
            "deletion_traces",
            deletion_traces(s, selector, T0, Some(trace(1)), 500).map(Some),
        );
    }
    budget.plan(
        "deletion_issuers",
        deletion_issuers(s, T0, Some("issuer"), TRACE_ID_LIST_CHUNK as u32 + 1).map(Some),
    );
    // A delete and its verification on every replica: a page of traces, the subject hashes of a user, or the
    // retention cut-off, with the request's arrival bound where the table records one.
    let mut deletes = 0;
    for table in TableId::ALL {
        let arrived = lifecycle_column(table, "received_at")
            .map(|received| Pred::cmp(received, crucible_plan::CmpOp::Lt, Literal::Time(T0)));
        let before = |selected: Pred| match &arrived {
            Some(arrived) => Pred::And(vec![selected, arrived.clone()]),
            None => selected,
        };
        let mut preds = Vec::new();
        if let Some(column) = lifecycle_column(table, "trace_id") {
            let traces = ids.iter().copied().map(Literal::from).collect();
            preds.push(before(Pred::In(column, traces)));
        }
        if lifecycle_column(table, "user_subject_hash").is_some() {
            preds.push(before(mcp_user(table, "user", &hashes).unwrap()));
        }
        if let Ok(expired) = expired_before(table, T0) {
            preds.push(expired);
        }
        for pred in preds {
            if let Ok(plan) = lifecycle_oldest(s, table, hour(), pred.clone()) {
                budget.check("lifecycle_oldest", &plan.on_every_replica());
            }
            // The tables without a project take no delete of a project's rows.
            let Some(delete) = [day(), month(), PartitionId::UNPARTITIONED]
                .into_iter()
                .find_map(|partition| DeletePlan::project(s, table, partition, pred.clone()).ok())
            else {
                continue;
            };
            // A delete on a cluster carries the most settings.
            let mut settings: Vec<(&str, String)> = DELETE_SETTINGS
                .iter()
                .map(|(name, value)| (*name, (*value).to_owned()))
                .collect();
            settings.push((INACTIVE_REPLICAS.0, INACTIVE_REPLICAS.1.to_owned()));
            let fields = render::delete(&delete).unwrap().fields(&settings);
            assert!(
                fields <= FIELD_BUDGET,
                "the delete in {table:?} carries {fields} fields"
            );
            deletes += 1;
        }
    }
    assert!(deletes > 20, "only {deletes} deletes were checked");
    // The existence probe of a block, on one server and on every replica in a cluster; a lookup carries its
    // time limit.
    let lookup = [("max_execution_time", "5".to_owned())];
    let cluster = Ident::new("cluster").unwrap();
    let epoch = WalEpoch::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1c").unwrap();
    // A block of a plain table is probed by its batch, one of an aggregating table by its marker row.
    for (table, partition) in [(TableId::SpanCore, day()), (TableId::SpanMetrics, month())] {
        let probe = batch_probe(table, partition, epoch, Lsn::new(9)).unwrap();
        for statement in [
            render::probe(&probe),
            render::probe_replicas(&probe, &cluster),
        ] {
            assert!(statement.fields(&lookup) <= FIELD_BUDGET);
        }
    }
    budget.finish(&[]);
}

/// A day partition, of the tables partitioned by day.
fn day() -> PartitionId {
    PartitionId::date(CivilDate::new(2026, 11, 17).unwrap())
}

/// A month partition, of the tables partitioned by month.
fn month() -> PartitionId {
    PartitionId::month(2026, 11).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every list-taking builder fits at any length up to its cap, not only at the cap.
    #[test]
    fn lists_up_to_their_caps_fit_one_request(
        traces in 1..=MAX_LIST,
        hits in 1..=SEARCH_SPANS_PER_LOOKUP,
    ) {
        let (s, c) = (project(), clamp());
        let mut budget = Budget::default();
        let ids: Vec<TraceId> = (0..traces).map(trace).collect();
        let parents: Vec<SpanId> = (0..traces).map(span).collect();
        let selected = ExportSelection { trace_ids: &ids, user_id: None, session_id: None };
        budget.plan("export_spans", export_spans(s, hour(), &c, selected, T0, None, 1_000));
        budget.plan("export_facts", export_facts(s, hour(), &c, FactTable::Scores, &ids));
        budget.plan(
            "trace_page_stats",
            trace_page_stats(s, hour(), &c, Caller::Session, &ids),
        );
        budget.plan(
            "child_counts",
            child_counts(s, bounds(), &c, Caller::Session, &parents),
        );
        budget.plan(
            "fact_page",
            fact_page(s, hour(), &c, Caller::Agent, FactTable::Tags, &ids),
        );
        let keys: Vec<(Timestamp, u32, SpanId)> = (0..hits).map(|n| (T0, n as u32, span(n))).collect();
        let rest = filter_at_caps(&TEXT_FIELDS[..3], MAX_FILTER_VALUES, false, true);
        budget.plan(
            "search_spans",
            search_spans(s, hour(), &c, Caller::Session, &keys, &rest),
        );
        prop_assert!(budget.over.is_empty(), "{:?}", budget.over);
    }

    /// A trace list filter fits at any spread of its members within TRACE_LIST_MAX_VALUES, sizes of one more
    /// than a power of two included, which padding nearly doubles.
    #[test]
    fn trace_list_filters_within_their_cap_fit_one_request(
        text in prop::array::uniform5(prop::sample::select(vec![0usize, 1, 2, 3, 5, 9, 17, 33, 64, 65, 100])),
        tags in 0..=TRACE_LIST_MAX_TAGS,
        metadata in prop::sample::select(vec![0usize, 1, 17, 33, 65, 100]),
        types in any::<bool>(),
    ) {
        let filter = trace_filter(&text, tags, types && metadata == 0, metadata);
        prop_assume!(filter.values() <= TRACE_LIST_MAX_VALUES);
        let request = TraceListRequest {
            filter: Pred::all(),
            limit: TRACE_LIST_MAX_LIMIT,
            cursor: Some((T0, trace(1).trace_hash(), trace(1))),
            as_of: T0,
            since: None,
        };
        let plan = filtered_trace_list(project(), hour(), &clamp(), Caller::Agent, request, &filter)
            .unwrap()
            .unwrap();
        let fields = render::read_fields(&plan).unwrap();
        prop_assert!(fields <= FIELD_BUDGET, "{fields} fields for {text:?}, {tags} tags, {metadata} entries");
    }
}

/// The store refuses a request above the budget before sending it; a plan of more values than a list may hold
/// is refused when it is built.
#[test]
fn the_budget_leaves_room_under_the_server_default_and_lists_stop_at_their_cap() {
    const { assert!(FIELD_BUDGET < 1_000) };
    let ids: Vec<TraceId> = (0..=MAX_LIST).map(trace).collect();
    assert_eq!(
        trace_page_stats(project(), hour(), &clamp(), Caller::Session, &ids),
        Err(PlanError::ListLength("trace_id"))
    );
}
