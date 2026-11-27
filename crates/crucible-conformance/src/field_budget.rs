//! Reads with long value lists on the real server (contracts 8.3 and 13.6): lists at their filter caps under more
//! pending deletions than a read carries, and an MCP read whose capture-source selection names 1,000 servers.
//! Every read runs on the server and on the store model, and must return the rows the fixture says.
//!
//! The span list, the filtered trace list and a search run with every filter member at its cap under 200 pending
//! deletions. A read carries the oldest 128 trace ids and the oldest 32 deletions by user or session, each with
//! its own instant (contract 13.6); a deletion it does not carry leaves its rows readable until its deletes run.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_plan::{
    Caller, Filter, Hidden, HiddenSelector, MAX_FILTER_VALUES, McpRead, McpSelection, Pred,
    QueryClass, QueryPlan, RetentionClamp, Row, SEARCH_SPANS_PER_LOOKUP, SearchField, SearchKey,
    SearchRequest, SourceChoice, SpanListRequest, TRACE_LIST_MAX_TAGS, TextField, TokenMode,
    TraceListRequest, Value, filtered_trace_list, mcp_presence, mcp_servers, search, search_spans,
    span_list,
};
use crucible_records::{
    CaptureSource, ErrorClass, HeadKind, Level, McpSide, ObservationType, ProjectScope, SpanId,
    TableId, TimeWindow, Timestamp, TraceId,
};
use crucible_schema::{Column, RetentionSettings, SpanCoreCol, TraceHeadsCol};

use crate::blocks::Rows;
use crate::differential::{insert, rows, stamped};
use crate::inserts::{id8, plain_rows, uuid};
use crate::reads::rollup;
use crate::{Ctx, T0, expect};

const SALT: u32 = 211;
const SECOND: i64 = 1_000_000;
const HOUR: i64 = 3_600 * SECOND;
/// The traces of the fixture, of two spans each.
const TRACES: usize = 40;
/// The traces that the carried deletions hide: traces 0 to 9 and 14 by id, 1, 11, 21 and 31 by their user
/// (`user-1`), 5 and 25 by their session (`session-5`).
const HIDDEN: [usize; 15] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 14, 21, 25, 31];

fn at(seconds: i64) -> Timestamp {
    T0.saturating_add_micros(seconds * SECOND)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn texts(values: impl IntoIterator<Item = String>) -> Value {
    Value::Array(values.into_iter().map(Value::String).collect())
}

fn trace(t: usize) -> TraceId {
    let mut bytes = [0x7d; 16];
    bytes[8..].copy_from_slice(&(t as u64).to_be_bytes());
    TraceId::from_bytes(bytes)
}

/// A trace no fixture row has.
fn absent(n: usize) -> TraceId {
    let mut bytes = [0x6e; 16];
    bytes[8..].copy_from_slice(&(n as u64).to_be_bytes());
    TraceId::from_bytes(bytes)
}

/// The span `s` (0 or 1) of trace `t`, which is row `2t + s` of the span tables, started `2t + s` seconds after
/// T0.
fn span(row: usize) -> Vec<u8> {
    match id8(SALT, row) {
        Value::Bytes(bytes) => bytes,
        _ => Vec::new(),
    }
}

fn user(t: usize) -> String {
    format!("user-{}", t % 10)
}

fn session(t: usize) -> String {
    format!("session-{}", t % 20)
}

/// The metadata entries of every span: the span list and search filters test all of them.
fn metadata() -> Vec<String> {
    (0..MAX_FILTER_VALUES)
        .map(|n| format!("key-{n}=value-{n}"))
        .collect()
}

async fn load(ctx: &Ctx) -> Result<ProjectScope> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let spans = 2 * TRACES;
    let start = |row: usize| at(row as i64);
    let core = plain_rows(TableId::SpanCore, project, epoch, 701, T0, spans, SALT)
        .each("start_time", |row| Value::Time(start(row)))
        .each("start_minute", |row| Value::Time(start(row).floor_minute()))
        .each("span_id", |row| Value::Bytes(span(row)))
        .each("trace_id", |row| {
            Value::Bytes(trace(row / 2).to_bytes().to_vec())
        })
        .each("trace_hash", |row| {
            Value::UInt(u64::from(trace(row / 2).trace_hash()))
        })
        .all("environment", text("env-a"))
        .all("release", text("rel-a"))
        .all("version", text("ver-a"))
        .each("user_id", |row| Value::String(user(row / 2)))
        .each("session_id", |row| Value::String(session(row / 2)))
        .all("level", Value::Enum(Level::Default.value()))
        .all(
            "observation_type",
            Value::Enum(ObservationType::Generation.value()),
        )
        .all("tags", texts(["tag-a".to_owned()]))
        .each("name", |row| Value::String(format!("checkout {}", row / 2)))
        .all("received_at", Value::Time(at(1)));
    let payload = plain_rows(TableId::SpanPayload, project, epoch, 702, T0, spans, SALT)
        .each("start_time", |row| Value::Time(start(row)))
        .each("start_minute", |row| Value::Time(start(row).floor_minute()))
        .each("span_id", |row| Value::Bytes(span(row)))
        .each("trace_id", |row| {
            Value::Bytes(trace(row / 2).to_bytes().to_vec())
        })
        .each("trace_hash", |row| {
            Value::UInt(u64::from(trace(row / 2).trace_hash()))
        })
        .each("user_id", |row| Value::String(user(row / 2)))
        .each("session_id", |row| Value::String(session(row / 2)))
        .each("input", |row| {
            Value::String(format!("alpha token{}", row / 2))
        })
        .all("metadata_kv", texts(metadata()))
        .all("received_at", Value::Time(at(1)));
    let heads = Rows::new(TableId::TraceHeads, TRACES)
        .all("project_id", uuid(project.uuid()))
        .each("start_time", |t| Value::Time(start(2 * t)))
        .each("trace_id", |t| Value::Bytes(trace(t).to_bytes().to_vec()))
        .each("trace_hash", |t| {
            Value::UInt(u64::from(trace(t).trace_hash()))
        })
        .each("span_id", |t| Value::Bytes(span(2 * t)))
        .all("head_kind", Value::Enum(HeadKind::Root.value()))
        .each("name", |t| Value::String(format!("checkout {t}")))
        .all("level", Value::Enum(Level::Default.value()))
        .each("user_id", |t| Value::String(user(t)))
        .each("session_id", |t| Value::String(session(t)))
        .all("tags", texts(["tag-a".to_owned()]))
        .all("environment", text("env-a"))
        .all("release", text("rel-a"))
        .all("version", text("ver-a"))
        .all("received_at", Value::Time(at(1)));
    for (seq, rows) in [(701, core), (702, payload), (703, heads)] {
        let table = rows.table();
        insert(
            ctx,
            stamped(rows, epoch, seq),
            ctx.token(epoch, seq, table, T0)?,
        )
        .await
        .with_context(|| format!("the list fixture of {}", table.name()))?;
    }
    scope(project).map_err(|e| anyhow!("{e}"))
}

/// 200 pending deletions, of which a read carries the oldest 128 trace ids and the oldest 32 deletions by user
/// or session. The spans arrived at T0 + 1 s.
fn pending() -> Vec<Hidden> {
    let traces = |ids: Vec<TraceId>, seconds: i64| Hidden {
        selector: HiddenSelector::Traces(ids),
        requested_at: at(seconds),
    };
    let by = |selector: HiddenSelector, seconds: i64| Hidden {
        selector,
        requested_at: at(seconds),
    };
    // Carried trace ids, 128 in all: trace 12 deleted before its spans arrived (nothing hidden), trace 14 deleted
    // before and after (the later hides it), traces 0 to 9, and 115 traces no row has.
    let mut all = vec![
        traces(vec![trace(12)], 0),
        traces(vec![trace(14)], 0),
        traces(vec![trace(14)], 120),
    ];
    all.extend((0..10).map(|t| traces(vec![trace(t)], 100 + t as i64)));
    all.extend((0..115).map(|n| traces(vec![absent(n)], 200 + n as i64)));
    // Not carried: the newest deletions of traces 38 and 39 stay readable.
    all.push(traces(vec![trace(38)], 900));
    all.push(traces(vec![trace(39)], 901));
    // By user or session, 70 in all: user-1 and session-5 among the 32 carried, session-13 not carried.
    all.push(by(HiddenSelector::User("user-1".to_owned()), 150));
    all.push(by(HiddenSelector::Session("session-5".to_owned()), 160));
    all.extend((0..30).map(|n| {
        let selector = if n % 2 == 0 {
            HiddenSelector::User(format!("gone-user-{n}"))
        } else {
            HiddenSelector::Session(format!("gone-session-{n}"))
        };
        by(selector, 300 + n)
    }));
    all.push(by(HiddenSelector::Session("session-13".to_owned()), 950));
    all.extend((0..37).map(|n| by(HiddenSelector::User(format!("later-user-{n}")), 960 + n)));
    all
}

/// `real` and decoys, `MAX_FILTER_VALUES` values in all.
fn capped(real: &[String], decoy: &str, len: usize) -> Vec<String> {
    let mut values = real.to_vec();
    values.extend((values.len()..len).map(|n| format!("{decoy}-{n}")));
    values
}

/// Every condition of the span list and search at its cap, all of which the fixture's spans satisfy.
fn span_filter() -> Filter {
    let users: Vec<String> = (0..10).map(user).collect();
    let sessions: Vec<String> = (0..20).map(session).collect();
    let mut all = vec![
        Filter::Text(
            TextField::Environment,
            capped(&["env-a".to_owned()], "env", MAX_FILTER_VALUES),
        ),
        Filter::Text(
            TextField::Release,
            capped(&["rel-a".to_owned()], "rel", MAX_FILTER_VALUES),
        ),
        Filter::Text(
            TextField::Version,
            capped(&["ver-a".to_owned()], "ver", MAX_FILTER_VALUES),
        ),
        Filter::Text(
            TextField::UserId,
            capped(&users, "nobody", MAX_FILTER_VALUES),
        ),
        Filter::Text(
            TextField::SessionId,
            capped(&sessions, "none", MAX_FILTER_VALUES),
        ),
        Filter::Level(Level::ALL.to_vec()),
        Filter::ObservationType(ObservationType::ALL.to_vec()),
        Filter::Tags(capped(&["tag-a".to_owned()], "tag", MAX_FILTER_VALUES)),
        Filter::Name("checkout".to_owned()),
    ];
    all.extend(metadata().into_iter().map(|entry| {
        let (key, value) = entry.split_once('=').unwrap_or_default();
        Filter::Metadata {
            key: key.to_owned(),
            value: value.to_owned(),
        }
    }));
    Filter::And(all)
}

/// The trace list's filter at its caps: 256 values across its members, the most tags, and observation types.
fn trace_filter() -> Filter {
    let users: Vec<String> = (0..10).map(user).collect();
    Filter::And(vec![
        Filter::Text(
            TextField::Environment,
            capped(&["env-a".to_owned()], "env", 65),
        ),
        Filter::Text(TextField::Release, capped(&["rel-a".to_owned()], "rel", 65)),
        Filter::Text(TextField::Version, capped(&["ver-a".to_owned()], "ver", 65)),
        Filter::Text(TextField::UserId, capped(&users, "nobody", 41)),
        Filter::Level(Level::ALL.to_vec()),
        Filter::ObservationType(ObservationType::ALL.to_vec()),
        Filter::Tags(capped(&["tag-a".to_owned()], "tag", TRACE_LIST_MAX_TAGS)),
        Filter::Name("checkout".to_owned()),
    ])
}

fn live(plan: Result<Option<QueryPlan>, crucible_plan::PlanError>) -> Result<QueryPlan> {
    plan?.context("the builder returned no plan")
}

/// The bytes of output column `at` of each row.
fn column(found: &[Row], at: usize) -> Vec<Vec<u8>> {
    found
        .iter()
        .filter_map(|row| match row.values().get(at) {
            Some(Value::Bytes(bytes)) => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

fn position<C: Column>(all: &[C], name: &str) -> Result<usize> {
    all.iter()
        .position(|c| c.name() == name)
        .with_context(|| format!("no column {name}"))
}

/// The span list, the filtered trace list and a search, with every filter member at its cap, under 200 pending
/// deletions.
pub(crate) async fn lists(ctx: &Ctx) -> Result<String> {
    let scope = load(ctx).await?;
    let window = TimeWindow::new(at(-60), at(3_600)).map_err(|e| anyhow!("{e}"))?;
    let clamp = RetentionClamp::new(RetentionSettings::default(), T0);
    let caller = Caller::Session;
    let as_of = at(3_600);
    let hidden = pending();
    expect(hidden.len() == 200, || {
        format!("{} pending deletions", hidden.len())
    })?;
    let visible: Vec<usize> = (0..TRACES).filter(|t| !HIDDEN.contains(t)).collect();
    // Newest first, as the lists order them.
    let visible_spans: Vec<Vec<u8>> = visible
        .iter()
        .rev()
        .flat_map(|t| [span(2 * t + 1), span(2 * t)])
        .collect();

    let listed = rows(
        ctx,
        live(span_list(
            scope,
            window,
            &clamp,
            caller,
            SpanListRequest {
                filter: span_filter(),
                limit: 200,
                cursor: None,
                as_of,
            },
        ))?
        .hiding(&hidden),
    )
    .await
    .context("the span list")?;
    let at_span = position(SpanCoreCol::ALL, "span_id")?;
    expect(column(&listed, at_span) == visible_spans, || {
        format!(
            "the span list returned {} spans, expected the {} that no carried deletion hides",
            listed.len(),
            visible_spans.len()
        )
    })?;

    let traces = rows(
        ctx,
        live(filtered_trace_list(
            scope,
            window,
            &clamp,
            caller,
            TraceListRequest {
                filter: Pred::all(),
                limit: 200,
                cursor: None,
                as_of,
                since: None,
            },
            &trace_filter(),
        ))?
        .hiding(&hidden),
    )
    .await
    .context("the filtered trace list")?;
    let at_trace = position(TraceHeadsCol::ALL, "trace_id")?;
    let visible_traces: Vec<Vec<u8>> = visible
        .iter()
        .rev()
        .map(|t| trace(*t).to_bytes().to_vec())
        .collect();
    expect(column(&traces, at_trace) == visible_traces, || {
        format!(
            "the filtered trace list returned {} traces, expected the {} that no carried deletion hides",
            traces.len(),
            visible_traces.len()
        )
    })?;

    let request = SearchRequest {
        tokens: vec!["alpha".to_owned()],
        mode: TokenMode::All,
        fields: vec![SearchField::Input],
        filter: span_filter(),
        limit: 200,
        cursor: None,
        as_of,
    };
    let planned =
        search(scope, window, &clamp, caller, &request)?.context("the search returned no plan")?;
    let hits = rows(ctx, planned.hits.clone().hiding(&hidden))
        .await
        .context("the search's hits")?;
    let keys: Vec<SearchKey> = hits
        .iter()
        .filter_map(|row| match row.values() {
            [Value::Time(minute), Value::UInt(hash), Value::Bytes(id), ..] => Some((
                *minute,
                u32::try_from(*hash).ok()?,
                SpanId::from_bytes(id.as_slice().try_into().ok()?),
            )),
            _ => None,
        })
        .collect();
    let mut found = BTreeSet::new();
    let mut lookups = 0;
    for chunk in keys.chunks(SEARCH_SPANS_PER_LOOKUP) {
        lookups += 1;
        let spans = rows(
            ctx,
            live(search_spans(
                scope,
                window,
                &clamp,
                caller,
                chunk,
                &planned.rest,
            ))?
            .hiding(&hidden),
        )
        .await
        .context("the lookup of the search's hits")?;
        found.extend(column(&spans, at_span));
    }
    let expected: BTreeSet<Vec<u8>> = visible_spans.iter().cloned().collect();
    expect(keys.len() == expected.len() && found == expected, || {
        format!(
            "the search found {} hits and {} spans, expected the {} that no carried deletion hides",
            keys.len(),
            found.len(),
            expected.len()
        )
    })?;
    Ok(format!(
        "under {} pending deletions the span list returned {} spans, the filtered trace list {} traces and the \
         search {} spans in {lookups} lookups, the rows that no carried deletion hides, on the server and on the \
         store model",
        hidden.len(),
        listed.len(),
        traces.len(),
        found.len()
    ))
}

/// The servers of the selection: 900 captured by the SDK and 100 by OTLP, all on the server side.
const SERVERS: usize = 1_000;

/// An MCP read whose capture-source selection names 1,000 servers.
pub(crate) async fn mcp_selection(ctx: &Ctx) -> Result<String> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let server = |row: usize| format!("server-{row:04}");
    let source = |row: usize| {
        if row < 900 {
            CaptureSource::Sdk
        } else {
            CaptureSource::Otlp
        }
    };
    let data = |row: usize| row < SERVERS;
    let histogram = |count: u64| {
        let array = |v: u64| Value::Array(vec![Value::UInt(v)]);
        Value::Tuple2(Box::new(array(100)), Box::new(array(count)))
    };
    let empty = || {
        Value::Tuple2(
            Box::new(Value::Array(Vec::new())),
            Box::new(Value::Array(Vec::new())),
        )
    };
    let tools = rollup(TableId::McpToolRollup, project, epoch, 0, SERVERS)
        .all("side", Value::Enum(McpSide::Server.value()))
        .all("minute", Value::Time(T0.floor_minute()))
        .each("server_name", |row| {
            Value::String(if data(row) {
                server(row)
            } else {
                String::new()
            })
        })
        .each("deploy_id", |row| text(if data(row) { "v1" } else { "" }))
        .each("capture_source", |row| Value::Enum(source(row).value()))
        .each("method", |row| {
            text(if data(row) { "tools/call" } else { "" })
        })
        .each("target_name", |row| {
            text(if data(row) { "read" } else { "" })
        })
        .each("client_name", |row| {
            text(if data(row) { "agent" } else { "" })
        })
        .all("error_class", Value::Enum(ErrorClass::None.value()))
        .all("caller_fault", Value::UInt(0))
        .each("count", |row| {
            Value::UInt(if data(row) { row as u64 + 1 } else { 0 })
        })
        .each("dur_hist", |row| {
            if data(row) {
                histogram(row as u64 + 1)
            } else {
                empty()
            }
        })
        .each("semconv_hist", |_| empty())
        .each("last_seen", |row| {
            Value::Time(if data(row) { T0 } else { Timestamp::EPOCH })
        });
    let tools = stamped(tools, epoch, 801);
    insert(
        ctx,
        tools,
        ctx.token(epoch, 801, TableId::McpToolRollup, T0)?,
    )
    .await
    .context("the rollup rows of 1,000 servers")?;
    let scope = scope(project).map_err(|e| anyhow!("{e}"))?;
    let window = TimeWindow::new(
        T0.saturating_add_micros(-HOUR),
        T0.saturating_add_micros(HOUR),
    )
    .map_err(|e| anyhow!("{e}"))?;
    let clamp = RetentionClamp::new(RetentionSettings::default(), T0);
    let caller = Caller::Session;
    let presence = rows(
        ctx,
        live(mcp_presence(
            scope,
            window,
            &clamp,
            caller.class(QueryClass::Analytics),
            McpSide::Server,
            None,
        ))?,
    )
    .await
    .context("the presence of 1,000 servers")?;
    let pairs: Vec<(String, CaptureSource)> = presence
        .iter()
        .filter_map(|row| match (row.values().first(), row.values().get(1)) {
            (Some(Value::String(name)), Some(Value::Enum(source))) => {
                Some((name.clone(), CaptureSource::from_value(*source)?))
            }
            _ => None,
        })
        .collect();
    let selection = McpSelection::select(SourceChoice::Auto, pairs);
    let chosen = selection
        .servers()
        .iter()
        .filter(|s| s.chosen.is_some())
        .count();
    expect(chosen == SERVERS, || {
        format!("the selection reads {chosen} servers")
    })?;
    let read = McpRead {
        side: McpSide::Server,
        selection: &selection,
        include_caller_faults: false,
        server: None,
    };
    let servers = rows(
        ctx,
        live(mcp_servers(scope, window, &clamp, caller, &read))?,
    )
    .await
    .context("the servers of a selection of 1,000")?;
    let calls: u64 = servers
        .iter()
        .filter_map(|row| match row.values().get(4) {
            Some(Value::UInt(calls)) => Some(*calls),
            _ => None,
        })
        .sum();
    let expected = (SERVERS as u64) * (SERVERS as u64 + 1) / 2;
    expect(servers.len() == SERVERS && calls == expected, || {
        format!(
            "the read returned {} servers and {calls} calls, expected {SERVERS} and {expected}",
            servers.len()
        )
    })?;
    Ok(format!(
        "a selection of {SERVERS} servers in two capture classes read {} servers and {calls} calls, on the server \
         and on the store model",
        servers.len()
    ))
}
