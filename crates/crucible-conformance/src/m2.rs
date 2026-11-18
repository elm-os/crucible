//! The read plans of milestone 2 on the real server and on the store model (contract sections 8.1, 8.7, 8.8 and
//! 8.4): the session and user lists and details, the span list under every filter field, search with snippets,
//! the payload byte range, the metrics query, zero-filled time series, the set merge, a distinct count of a
//! nullable column, and the query-class guardrails.
//!
//! Every read goes through `differential::rows`, which runs it on both stores and compares the answers, and its
//! rows are also checked against the fixture, which lives in a project of its own.

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_env::QueryError;
use crucible_plan::test_support::{query_plan, with_limits};
use crucible_plan::{
    Aggregate, Aggregation, Caller, DimensionId, Fill, Filter, Granularity, Measure, MeasureId,
    MetricsQuery, Node, PayloadField, PayloadRangeRequest, Pred, QueryClass, QueryPlan,
    RetentionClamp, Row, SearchField, SearchRequest, SessionListRequest, Source, SpanListRequest,
    Step, TextField, TimeSeries, TokenMode, UserListRequest, Value, metrics_keys, metrics_other,
    metrics_ranking, metrics_series, metrics_shape, payload_range, search, search_spans,
    session_detail, session_list, span_list, user_detail, user_list, user_session_counts,
};
use crucible_records::{
    Level, ObservationType, ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId, Uuid,
};
use crucible_schema::{
    Column, FactTagsCol, McpOpsCol, RetentionSettings, SessionRollupCol, SpanCoreCol,
    SpanMetricsCol, TraceStatsCol,
};

use crate::blocks::Rows;
use crate::differential::{insert, refused, rows, stamped};
use crate::inserts::{id8, plain_rows, uuid};
use crate::reads::rollup;
use crate::{Ctx, T0, expect};

const SALT: u32 = 33;
const SECOND: i64 = 1_000_000;
const HOUR: i64 = 3_600 * SECOND;
const DAY: i64 = 24 * HOUR;
/// The third payload input: a token far enough in that its snippet does not start at the field's first byte.
const FILLER: usize = 100;

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn texts(values: &[&str]) -> Value {
    Value::Array(values.iter().map(|v| text(v)).collect())
}

fn span_id(row: usize) -> SpanId {
    let Value::Bytes(bytes) = id8(SALT, row) else {
        return SpanId::ZERO;
    };
    SpanId::from_bytes(bytes.try_into().unwrap_or([0; 8]))
}

fn histogram(keys: &[u64], counts: &[u64]) -> Value {
    let array = |v: &[u64]| Value::Array(v.iter().map(|x| Value::UInt(*x)).collect());
    Value::Tuple2(Box::new(array(keys)), Box::new(array(counts)))
}

fn values(rows: &[Row]) -> Vec<Vec<Value>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

fn same<T: PartialEq + std::fmt::Debug>(what: &str, got: &T, expected: &T) -> Result<()> {
    expect(got == expected, || {
        format!("{what}: got {got:?}, expected {expected:?}")
    })
}

fn window(from: i64, to: i64) -> Result<TimeWindow> {
    TimeWindow::new(at(from), at(to)).map_err(|e| anyhow!("{e}"))
}

/// A clamp that expires nothing.
fn clamp() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), T0)
}

fn live<T>(plan: Option<T>) -> Result<T> {
    plan.context("the builder returned no plan: the window has expired")
}

pub(crate) struct Fixture {
    scope: ProjectScope,
    /// Three hours around the fixture instant, unaligned to every step above the minute.
    window: TimeWindow,
    /// The fixture's day and the day before.
    days: TimeWindow,
    trace: TraceId,
}

const INPUTS: [&str; 4] = [
    "Hello WORLD foo",
    "nothing to see",
    " Needle in the hay world",
    "h\u{e9}llo",
];
const OUTPUTS: [&str; 4] = ["the answer is foo", "", "world", ""];

fn input(row: usize) -> String {
    match row {
        2 => format!("{}{}", "x".repeat(FILLER), INPUTS[2]),
        _ => INPUTS[row].to_owned(),
    }
}

pub(crate) async fn load(ctx: &Ctx) -> Result<Fixture> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let trace = TraceId::from_bytes([0xd0; 16]);
    let trace_bytes = Value::Bytes(trace.to_bytes().to_vec());
    let mut seq = 0;
    let mut put = async |rows: Rows, time: Timestamp| -> Result<()> {
        seq += 1;
        let table = rows.table();
        insert(
            ctx,
            stamped(rows, epoch, seq),
            ctx.token(epoch, seq, table, time)?,
        )
        .await
        .with_context(|| format!("milestone 2 fixture rows of {}", table.name()))
    };
    let start = |row: usize| at(row as i64 * SECOND);

    // Six spans, one second apart, that differ in every field a list filter tests.
    let names = [
        "Chat Completion",
        "chat completion",
        "Embed",
        "Tool call",
        "CHAT stream",
        "Rerank",
    ];
    let levels = [
        Level::Default,
        Level::Default,
        Level::Warning,
        Level::Error,
        Level::Error,
        Level::Debug,
    ];
    let types = [
        ObservationType::Generation,
        ObservationType::Generation,
        ObservationType::Embedding,
        ObservationType::Tool,
        ObservationType::Generation,
        ObservationType::Span,
    ];
    let spans = plain_rows(TableId::SpanCore, project, epoch, 1, T0, 6, SALT)
        .each("start_time", |row| Value::Time(start(row)))
        .all("trace_id", trace_bytes.clone())
        .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())))
        .each("name", |row| text(names[row]))
        .each("user_id", |row| text(&format!("u{}", row % 2)))
        .each("session_id", |row| text(&format!("s{}", row % 3)))
        .each("environment", |row| {
            text(if row < 4 { "prod" } else { "dev" })
        })
        .each("release", |row| text(&format!("r{}", row % 2)))
        .each("version", |row| text(if row < 3 { "v1" } else { "v2" }))
        .each("level", |row| Value::Enum(levels[row].value()))
        .each("observation_type", |row| Value::Enum(types[row].value()))
        .each("tags", |row| texts(&[&format!("tag{row}"), "common"]))
        // The column a later schema step added: both sides must read it back as written.
        .each("usage_aggregate", |row| Value::UInt(u64::from(row % 2 == 0)));
    put(spans, T0).await?;

    // The payloads of the first four spans.
    let metadata: [&[&str]; 4] = [&["a=b", "k=v"], &["k=w"], &["k=v"], &[]];
    let payload = plain_rows(TableId::SpanPayload, project, epoch, 1, T0, 4, SALT)
        .each("start_time", |row| Value::Time(start(row)))
        .all("trace_id", trace_bytes.clone())
        .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())))
        .each("user_id", |row| text(&format!("u{}", row % 2)))
        .each("session_id", |row| text(&format!("s{}", row % 3)))
        .each("input", |row| text(&input(row)))
        .each("output", |row| text(OUTPUTS[row]))
        .each("metadata_kv", |row| texts(metadata[row]));
    put(payload, T0).await?;

    // Four trace heads: one user with two sessions and a head without one, and a user with one session.
    let heads = Rows::new(TableId::TraceHeads, 4)
        .all("project_id", uuid(project.uuid()))
        .each("start_time", |row| Value::Time(start(row)))
        .all("received_at", Value::Time(T0))
        .each("trace_id", |row| Value::Bytes(vec![0xd1 + row as u8; 16]))
        .each("trace_hash", |row| {
            Value::UInt(u64::from(
                TraceId::from_bytes([0xd1 + row as u8; 16]).trace_hash(),
            ))
        })
        .each("user_id", |row| text(["u0", "u0", "u0", "u1"][row]))
        .each("session_id", |row| text(["s0", "s1", "", "s0"][row]));
    put(heads, T0).await?;

    // Session partials in three blocks over two days. `s0` has rows in each, `s1` and `s2` tie on their last
    // instant, and `s3` exists on the earlier day only.
    type Partial = (&'static str, i64, i64, u64, &'static [&'static str]);
    let sessions = |day: Timestamp, partials: &'static [Partial]| {
        rollup(TableId::SessionRollup, project, epoch, 0, partials.len())
            .all("day", Value::Date(day.date()))
            .each("session_id", |row| {
                text(partials.get(row).map_or("", |p| p.0))
            })
            .each("first_seen", |row| {
                Value::Time(partials.get(row).map_or(day, |p| at(p.1)))
            })
            .each("last_seen", |row| {
                Value::Time(partials.get(row).map_or(day, |p| at(p.2)))
            })
            .each("trace_count", |row| {
                Value::UInt(u64::from(row < partials.len()))
            })
            .each("span_count", |row| {
                Value::UInt(partials.get(row).map_or(0, |p| p.3))
            })
            .each("total_cost_nusd", |row| {
                Value::Int(partials.get(row).map_or(0, |p| p.3 as i64 * 10))
            })
            .each("user_ids", |row| {
                texts(partials.get(row).map_or(&[], |p| p.4))
            })
    };
    put(
        sessions(
            T0,
            &[
                ("s0", 0, 10 * SECOND, 3, &["a", "b"]),
                ("s1", SECOND, 20 * SECOND, 2, &["c"]),
                ("s2", 2 * SECOND, 20 * SECOND, 1, &[]),
            ],
        ),
        T0,
    )
    .await?;
    put(
        sessions(T0, &[("s0", -5 * SECOND, 5 * SECOND, 2, &["b", "c"])]),
        T0,
    )
    .await?;
    put(
        sessions(
            at(-DAY),
            &[
                ("s0", -DAY, -DAY + SECOND, 1, &["d"]),
                ("s3", -DAY, -DAY + 2 * SECOND, 1, &["e"]),
            ],
        ),
        at(-DAY),
    )
    .await?;

    // User partials on the two days.
    type UserPartial = (&'static str, i64, i64, u64);
    let users = |day: Timestamp, partials: &'static [UserPartial]| {
        rollup(TableId::UserRollup, project, epoch, 0, partials.len())
            .all("day", Value::Date(day.date()))
            .each("user_id", |row| text(partials.get(row).map_or("", |p| p.0)))
            .each("first_seen", |row| {
                Value::Time(partials.get(row).map_or(day, |p| at(p.1)))
            })
            .each("last_seen", |row| {
                Value::Time(partials.get(row).map_or(day, |p| at(p.2)))
            })
            .each("trace_count", |row| {
                Value::UInt(u64::from(row < partials.len()))
            })
            .each("span_count", |row| {
                Value::UInt(partials.get(row).map_or(0, |p| p.3))
            })
    };
    put(
        users(
            T0,
            &[("u0", 0, 10 * SECOND, 3), ("u1", SECOND, 20 * SECOND, 2)],
        ),
        T0,
    )
    .await?;
    put(users(at(-DAY), &[("u0", -DAY, -DAY, 1)]), at(-DAY)).await?;

    // The metrics rollup: three models in two hours; the first block's `m-a` row has a second partial.
    type Metric = (
        i64,
        &'static str,
        &'static str,
        ObservationType,
        Level,
        u64,
        &'static [u64],
        &'static [u64],
    );
    let metrics = |partials: &'static [Metric]| {
        let of = move |row: usize| partials.get(row);
        rollup(TableId::SpanMetrics, project, epoch, 0, partials.len())
            .each("minute", move |row| {
                Value::Time(at(of(row).map_or(0, |p| p.0)).floor_minute())
            })
            .each("model_response", move |row| {
                text(of(row).map_or("", |p| p.1))
            })
            .each("environment", move |row| text(of(row).map_or("", |p| p.2)))
            .each("observation_type", move |row| {
                Value::Enum(of(row).map_or(0, |p| p.3.value()))
            })
            .each("level", move |row| {
                Value::Enum(of(row).map_or(0, |p| p.4.value()))
            })
            .each("count", move |row| Value::UInt(of(row).map_or(0, |p| p.5)))
            .each("total_cost_nusd", move |row| {
                Value::Int(of(row).map_or(0, |p| p.5 as i64 * 10))
            })
            .each("dur_hist", move |row| {
                of(row).map_or_else(|| histogram(&[], &[]), |p| histogram(p.6, p.7))
            })
    };
    const GENERATION: ObservationType = ObservationType::Generation;
    const PLAIN: ObservationType = ObservationType::Span;
    const DEFAULT: Level = Level::Default;
    put(
        metrics(&[
            (0, "m-a", "prod", GENERATION, DEFAULT, 5, &[1, 2], &[2, 3]),
            (0, "m-b", "prod", GENERATION, DEFAULT, 3, &[2], &[3]),
            (0, "m-c", "dev", PLAIN, Level::Error, 1, &[4], &[1]),
            (HOUR, "m-a", "prod", GENERATION, DEFAULT, 2, &[1], &[2]),
            (HOUR, "m-c", "dev", PLAIN, Level::Error, 4, &[4], &[4]),
        ]),
        T0,
    )
    .await?;
    put(
        metrics(&[(0, "m-a", "prod", GENERATION, DEFAULT, 1, &[2], &[1])]),
        T0,
    )
    .await?;

    // One trace's stats with a name pick, and two versions of one tag fact.
    let stats = rollup(TableId::TraceStats, project, epoch, 0, 1)
        .all("trace_id", trace_bytes.clone())
        .all("row_day", Value::Date(T0.date()))
        .all("min_start", Value::Time(T0))
        .all("max_end", Value::Time(at(5 * SECOND)))
        .all("min_head_start", Value::Time(T0))
        .all("span_count", Value::UInt(6))
        .each("trace_name_pick", |row| {
            text(if row == 0 { "Chat Completion" } else { "" })
        });
    put(stats, T0).await?;
    let facts = Rows::new(TableId::FactTags, 2)
        .all("project_id", uuid(project.uuid()))
        .all("key", text("quality"))
        .all("target_day", Value::Date(T0.date()))
        .all("target_time", Value::Time(T0))
        .all("trace_id", trace_bytes.clone())
        .each("version", |row| Value::UInt(row as u64 + 1))
        .each("fact_id", |row| uuid(Uuid::from_bytes([row as u8 + 1; 16])));
    put(facts, T0).await?;

    // Nullable numbers: the operations with `duration_us = 0` have no progress count at all.
    let ops = plain_rows(TableId::McpOps, project, epoch, 1, T0, 4, SALT)
        .each("duration_us", |row| Value::UInt(row as u64 / 2))
        .each("progress_count", |row| {
            [Value::Null, Value::Null, Value::UInt(3), Value::Null][row].clone()
        })
        .each("jsonrpc_code", |row| {
            [Value::Null, Value::Int(-1), Value::Int(2), Value::Null][row].clone()
        });
    put(ops, T0).await?;

    Ok(Fixture {
        scope: scope(project).map_err(|e| anyhow!("{e}"))?,
        window: window(-HOUR, 2 * HOUR)?,
        days: window(-DAY - HOUR, 2 * HOUR)?,
        trace,
    })
}

/// The value of one output column of every row.
fn column(rows: &[Row], at: usize) -> Vec<Value> {
    rows.iter().map(|row| row.values()[at].clone()).collect()
}

fn core_index(column: SpanCoreCol) -> Result<usize> {
    SpanCoreCol::ALL
        .iter()
        .position(|c| *c == column)
        .context("a column of span_core")
}

async fn lists(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let (scope, session) = (f.scope, Caller::Session);
    let mut plans = 0;
    let time = |micros: i64| Value::Time(at(micros));

    // Sessions: the merged partials, last seen first, ties by id; then the page after the cursor.
    let merged = |id: &str, last: i64, first: i64, traces: u64, spans: u64, users: &[&str]| {
        vec![
            text(id),
            time(last),
            time(first),
            Value::UInt(traces),
            Value::UInt(spans),
            Value::UInt(0),
            Value::Int(spans as i64 * 10),
            Value::UInt(0),
            Value::UInt(0),
            Value::UInt(0),
            texts(users),
        ]
    };
    let s0 = merged("s0", 10 * SECOND, -DAY, 3, 6, &["a", "b", "c", "d"]);
    let s1 = merged("s1", 20 * SECOND, SECOND, 1, 2, &["c"]);
    let s2 = merged("s2", 20 * SECOND, 2 * SECOND, 1, 1, &[]);
    let s3 = merged("s3", -DAY + 2 * SECOND, -DAY, 1, 1, &["e"]);
    for caller in [session, Caller::Agent] {
        let request = SessionListRequest {
            limit: 2,
            cursor: None,
        };
        let page = rows(
            ctx,
            live(session_list(scope, f.days, &clamp(), caller, request)?)?,
        )
        .await?;
        same(
            "session list, first page",
            &values(&page),
            &vec![s2.clone(), s1.clone(), s0.clone()],
        )?;
        plans += 1;
    }
    let request = SessionListRequest {
        limit: 2,
        cursor: Some((at(20 * SECOND), "s1".to_owned())),
    };
    let page = rows(
        ctx,
        live(session_list(scope, f.days, &clamp(), session, request)?)?,
    )
    .await?;
    same(
        "session list after the cursor",
        &values(&page),
        &vec![s0, s3],
    )?;
    let detail = rows(
        ctx,
        live(session_detail(scope, f.window, &clamp(), session, "s0")?)?,
    )
    .await?;
    same(
        "session detail over one day",
        &values(&detail),
        &vec![merged(
            "s0",
            10 * SECOND,
            -5 * SECOND,
            2,
            5,
            &["a", "b", "c"],
        )],
    )?;
    let missing = rows(
        ctx,
        live(session_detail(scope, f.days, &clamp(), session, "s9")?)?,
    )
    .await?;
    same("an unknown session", &missing.len(), &0)?;
    plans += 3;

    // Users.
    let merged = |id: &str, last: i64, first: i64, traces: u64, spans: u64| {
        vec![
            text(id),
            time(last),
            time(first),
            Value::UInt(traces),
            Value::UInt(spans),
            Value::UInt(0),
            Value::Int(0),
            Value::UInt(0),
            Value::UInt(0),
            Value::UInt(0),
        ]
    };
    let u0 = merged("u0", 10 * SECOND, -DAY, 2, 4);
    let u1 = merged("u1", 20 * SECOND, SECOND, 1, 2);
    let request = UserListRequest {
        limit: 1,
        cursor: None,
    };
    let page = rows(
        ctx,
        live(user_list(scope, f.days, &clamp(), session, request)?)?,
    )
    .await?;
    same(
        "user list, first page",
        &values(&page),
        &vec![u1, u0.clone()],
    )?;
    let request = UserListRequest {
        limit: 1,
        cursor: Some((at(20 * SECOND), "u1".to_owned())),
    };
    let page = rows(
        ctx,
        live(user_list(scope, f.days, &clamp(), session, request)?)?,
    )
    .await?;
    same(
        "user list after the cursor",
        &values(&page),
        &vec![u0.clone()],
    )?;
    let detail = rows(
        ctx,
        live(user_detail(scope, f.days, &clamp(), session, "u0")?)?,
    )
    .await?;
    same("user detail", &values(&detail), &vec![u0])?;
    let ids = ["u0", "u1", "u9"].map(str::to_owned);
    let mut counts = values(
        &rows(
            ctx,
            live(user_session_counts(
                scope,
                f.window,
                &clamp(),
                session,
                &ids,
            )?)?,
        )
        .await?,
    );
    counts.sort_by_key(|row| format!("{row:?}"));
    same(
        "distinct sessions of users",
        &counts,
        &vec![
            vec![text("u0"), Value::UInt(2)],
            vec![text("u1"), Value::UInt(1)],
        ],
    )?;
    plans += 4;

    // The span list: newest first, one row more than the page, then the page after the cursor.
    let id_at = core_index(SpanCoreCol::SpanId)?;
    let ids = |spans: &[usize]| -> Vec<Value> { spans.iter().map(|row| id8(SALT, *row)).collect() };
    let list = async |filter: Filter, limit: u32, cursor: Option<usize>| -> Result<Vec<Value>> {
        let request = SpanListRequest {
            filter,
            limit,
            cursor: cursor.map(|row| (at(row as i64 * SECOND), span_id(row))),
            as_of: at(HOUR),
        };
        let plan = live(span_list(scope, f.window, &clamp(), session, request)?)?;
        Ok(column(&rows(ctx, plan).await?, id_at))
    };
    same(
        "span list, first page",
        &list(Filter::all(), 3, None).await?,
        &ids(&[5, 4, 3, 2]),
    )?;
    same(
        "span list after the cursor",
        &list(Filter::all(), 3, Some(3)).await?,
        &ids(&[2, 1, 0]),
    )?;
    plans += 2;
    let strings = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let metadata = Filter::Metadata {
        key: "k".to_owned(),
        value: "v".to_owned(),
    };
    let prod = Filter::Text(TextField::Environment, strings(&["prod"]));
    let filters: Vec<(&str, Filter, &[usize])> = vec![
        (
            "environment",
            Filter::Text(TextField::Environment, strings(&["dev"])),
            &[5, 4],
        ),
        (
            "release",
            Filter::Text(TextField::Release, strings(&["r1"])),
            &[5, 3, 1],
        ),
        (
            "version",
            Filter::Text(TextField::Version, strings(&["v1"])),
            &[2, 1, 0],
        ),
        (
            "level",
            Filter::Level(vec![Level::Error, Level::Warning]),
            &[4, 3, 2],
        ),
        (
            "observation type",
            Filter::ObservationType(vec![ObservationType::Generation]),
            &[4, 1, 0],
        ),
        (
            "user",
            Filter::Text(TextField::UserId, strings(&["u0"])),
            &[4, 2, 0],
        ),
        (
            "session",
            Filter::Text(TextField::SessionId, strings(&["s1", "s2"])),
            &[5, 4, 2, 1],
        ),
        ("tags", Filter::Tags(strings(&["tag2", "tag5"])), &[5, 2]),
        ("name", Filter::Name("chat".to_owned()), &[4, 1, 0]),
        ("metadata", metadata.clone(), &[2, 0]),
        (
            "metadata with core conditions",
            Filter::And(vec![
                metadata.clone(),
                prod.clone(),
                Filter::Not(Box::new(Filter::Level(vec![Level::Warning]))),
            ]),
            &[0],
        ),
        (
            "or and not",
            Filter::Or(vec![
                Filter::Tags(strings(&["tag0"])),
                Filter::Not(Box::new(prod)),
            ]),
            &[5, 4, 0],
        ),
    ];
    for (what, filter, expected) in filters {
        same(
            &format!("span list filtered by {what}"),
            &list(filter, 10, None).await?,
            &ids(expected),
        )?;
        plans += 1;
    }
    Ok(plans)
}

async fn search_and_payload(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let (scope, session) = (f.scope, Caller::Session);
    let mut plans = 0;
    let request = |tokens: &[&str], mode, fields: &[SearchField], filter: Filter| SearchRequest {
        tokens: tokens.iter().map(|t| (*t).to_owned()).collect(),
        mode,
        fields: fields.to_vec(),
        filter,
        limit: 10,
    };
    let both = [SearchField::Input, SearchField::Output];
    let hits = async |request: &SearchRequest| -> Result<Vec<Vec<Value>>> {
        let plan = live(search(scope, f.window, &clamp(), session, request)?)?;
        // The key is `(start_minute, trace_hash, span_id)`: order the hits by span.
        let mut found = values(&rows(ctx, plan.hits).await?);
        found.sort_by_key(|row| format!("{:?}", row[2]));
        Ok(found.into_iter().map(|row| row[2..].to_vec()).collect())
    };

    // A token in another case than the text: the hit is by token, the position by substring, both without
    // regard to ASCII case. The second hit's token is past the snippet's lead, so its snippet starts inside.
    let long = input(2);
    let position = FILLER + INPUTS[2].len() - "world".len() + 1;
    let from = position - 80 - 1;
    let found = hits(&request(&["World"], TokenMode::Any, &both, Filter::all())).await?;
    same(
        "search hits with snippets",
        &found,
        &vec![
            vec![
                id8(SALT, 0),
                Value::UInt(7),
                text(INPUTS[0]),
                Value::UInt(0),
                text(OUTPUTS[0]),
            ],
            vec![
                id8(SALT, 2),
                Value::UInt(position as u64),
                text(&long[from..]),
                Value::UInt(1),
                text("world"),
            ],
        ],
    )?;
    let found = hits(&request(
        &["foo", "HELLO"],
        TokenMode::All,
        &[SearchField::Input],
        Filter::all(),
    ))
    .await?;
    same(
        "search for all tokens",
        &found,
        &vec![vec![id8(SALT, 0), Value::UInt(1), text(INPUTS[0])]],
    )?;
    let found = hits(&request(
        &["k=v"],
        TokenMode::Any,
        &[SearchField::Metadata],
        Filter::all(),
    ))
    .await?;
    same(
        "search of metadata entries",
        &found,
        &vec![vec![id8(SALT, 0)], vec![id8(SALT, 2)]],
    )?;
    plans += 3;

    // A filter whose payload conjuncts restrict the hits and whose rest restricts the core rows.
    let strings = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let filtered = request(
        &["world"],
        TokenMode::Any,
        &both,
        Filter::And(vec![
            Filter::Text(TextField::UserId, strings(&["u0"])),
            Filter::Metadata {
                key: "k".to_owned(),
                value: "v".to_owned(),
            },
            Filter::Level(vec![Level::Warning]),
        ]),
    );
    let plan = live(search(scope, f.window, &clamp(), session, &filtered)?)?;
    let found = rows(ctx, plan.hits).await?;
    same("filtered search hits", &found.len(), &2)?;
    let keys: Vec<(Timestamp, u32, SpanId)> = found
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
    same("the keys of the hits", &keys.len(), &2)?;
    let spans = rows(
        ctx,
        live(search_spans(
            scope,
            f.window,
            &clamp(),
            session,
            &keys,
            &plan.rest,
        )?)?,
    )
    .await?;
    same(
        "the core rows of the hits under the rest of the filter",
        &column(&spans, core_index(SpanCoreCol::SpanId)?),
        &vec![id8(SALT, 2)],
    )?;
    plans += 2;

    // The payload viewer: a range inside the field, one that cuts a character, one past the end, and a span
    // without a payload row.
    let range = async |row: usize, offset: u64, len: u64| -> Result<Vec<Vec<Value>>> {
        let request = PayloadRangeRequest {
            trace_id: f.trace,
            span_id: span_id(row),
            start_time: at(row as i64 * SECOND),
            field: PayloadField::Input,
            offset,
            len,
        };
        let plan = live(payload_range(scope, &clamp(), session, request)?)?;
        Ok(values(&rows(ctx, plan).await?))
    };
    same(
        "a payload range",
        &range(2, FILLER as u64 + 1, 6).await?,
        &vec![vec![text("Needle"), Value::UInt(long.len() as u64)]],
    )?;
    same(
        "a payload range that cuts a character",
        &range(3, 0, 2).await?,
        &vec![vec![Value::Bytes(vec![b'h', 0xc3]), Value::UInt(6)]],
    )?;
    same(
        "a payload range past the end",
        &range(0, 1_000, 5).await?,
        &vec![vec![text(""), Value::UInt(INPUTS[0].len() as u64)]],
    )?;
    same(
        "the payload of a span without one",
        &range(5, 0, 5).await?.len(),
        &0,
    )?;
    plans += 4;
    Ok(plans)
}

async fn metrics(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let (scope, session) = (f.scope, Caller::Session);
    let mut plans = 0;
    let hour = |n: i64| Value::Time(Timestamp::from_micros((at(0).micros() / HOUR + n) * HOUR));
    let empty = || histogram(&[], &[]);

    // One dimension, an additive measure and a percentile, zero fill: the ranking, the series of the first two
    // keys and the series of the rest.
    let query = MetricsQuery {
        source: Source::Spans,
        measures: vec![
            (MeasureId::Count, Aggregation::Sum),
            (MeasureId::Duration, Aggregation::P50),
        ],
        dimensions: vec![DimensionId::Model],
        filter: Filter::all(),
        granularity: Granularity::Step(Step::Hour),
        fill: Fill::Zero,
        top_n: Some(2),
    };
    let ranking = rows(
        ctx,
        live(metrics_ranking(scope, f.window, &clamp(), session, &query)?)?,
    )
    .await?;
    same(
        "metrics ranking",
        &values(&ranking),
        &vec![
            vec![text("m-a"), Value::UInt(8)],
            vec![text("m-c"), Value::UInt(5)],
            vec![text("m-b"), Value::UInt(3)],
        ],
    )?;
    let keys = metrics_keys(&query, &ranking[..2]).context("the ranked keys")?;
    let series = rows(
        ctx,
        live(metrics_series(
            scope,
            f.window,
            &clamp(),
            session,
            &query,
            Some(&keys),
        )?)?,
    )
    .await?;
    let point = |n: i64, model: &str, count: u64, hist: Value| {
        vec![hour(n), text(model), Value::UInt(count), hist]
    };
    same(
        "zero-filled metrics series of two keys",
        &values(&series),
        &vec![
            point(-1, "m-a", 0, empty()),
            point(0, "m-a", 6, histogram(&[1, 2], &[2, 4])),
            point(1, "m-a", 2, histogram(&[1], &[2])),
            point(2, "m-a", 0, empty()),
            point(-1, "m-c", 0, empty()),
            point(0, "m-c", 1, histogram(&[4], &[1])),
            point(1, "m-c", 4, histogram(&[4], &[4])),
            point(2, "m-c", 0, empty()),
        ],
    )?;
    let other = rows(
        ctx,
        live(metrics_other(
            scope,
            f.window,
            &clamp(),
            session,
            &query,
            &keys,
        )?)?,
    )
    .await?;
    same(
        "zero-filled series of the other keys",
        &values(&other),
        &vec![
            vec![hour(-1), Value::UInt(0)],
            vec![hour(0), Value::UInt(3)],
            vec![hour(1), Value::UInt(0)],
            vec![hour(2), Value::UInt(0)],
        ],
    )?;
    plans += 3;

    // No dimension, a filter, `auto` granularity: one minute bucket per minute of the window.
    let strings = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    let filtered = MetricsQuery {
        measures: vec![
            (MeasureId::Count, Aggregation::Sum),
            (MeasureId::TotalCost, Aggregation::Sum),
        ],
        dimensions: Vec::new(),
        filter: Filter::And(vec![
            Filter::Level(vec![Level::Error]),
            Filter::Text(TextField::Environment, strings(&["dev"])),
        ]),
        granularity: Granularity::Auto,
        top_n: None,
        ..query.clone()
    };
    let shape = metrics_shape(f.window, &filtered).map_err(|e| anyhow!("{e}"))?;
    same("the granularity auto picks", &shape.step, &Step::Minute)?;
    let minutes = values(
        &rows(
            ctx,
            live(metrics_series(
                scope,
                f.window,
                &clamp(),
                session,
                &filtered,
                None,
            )?)?,
        )
        .await?,
    );
    same("one row per minute of the window", &minutes.len(), &180)?;
    let filled: Vec<&Vec<Value>> = minutes
        .iter()
        .filter(|row| row[1] != Value::UInt(0))
        .collect();
    same(
        "the minutes with spans",
        &filled,
        &vec![
            &vec![Value::Time(T0), Value::UInt(1), Value::Int(10)],
            &vec![Value::Time(at(HOUR)), Value::UInt(4), Value::Int(40)],
        ],
    )?;
    plans += 1;

    // Two dimensions without fill: the keys are pairs.
    let pairs = MetricsQuery {
        measures: vec![(MeasureId::Count, Aggregation::Sum)],
        dimensions: vec![DimensionId::Environment, DimensionId::Model],
        granularity: Granularity::Step(Step::FifteenMinutes),
        fill: Fill::None,
        top_n: Some(1),
        ..query.clone()
    };
    let ranking = rows(
        ctx,
        live(metrics_ranking(scope, f.window, &clamp(), session, &pairs)?)?,
    )
    .await?;
    same(
        "metrics ranking of pairs",
        &values(&ranking),
        &vec![
            vec![text("prod"), text("m-a"), Value::UInt(8)],
            vec![text("dev"), text("m-c"), Value::UInt(5)],
        ],
    )?;
    let keys = metrics_keys(&pairs, &ranking[..1]).context("the ranked keys")?;
    let mut series = values(
        &rows(
            ctx,
            live(metrics_series(
                scope,
                f.window,
                &clamp(),
                session,
                &pairs,
                Some(&keys),
            )?)?,
        )
        .await?,
    );
    series.sort_by_key(|row| format!("{row:?}"));
    same(
        "metrics series of a pair",
        &series,
        &vec![
            vec![Value::Time(T0), text("prod"), text("m-a"), Value::UInt(6)],
            vec![
                Value::Time(at(HOUR)),
                text("prod"),
                text("m-a"),
                Value::UInt(2),
            ],
        ],
    )?;
    let mut other = values(
        &rows(
            ctx,
            live(metrics_other(
                scope,
                f.window,
                &clamp(),
                session,
                &pairs,
                &keys,
            )?)?,
        )
        .await?,
    );
    other.sort_by_key(|row| format!("{row:?}"));
    same(
        "metrics series of the other pairs",
        &other,
        &vec![
            vec![Value::Time(T0), Value::UInt(4)],
            vec![Value::Time(at(HOUR)), Value::UInt(4)],
        ],
    )?;
    plans += 3;
    Ok(plans)
}

async fn fills(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let scope = f.scope;
    let mut plans = 0;
    let plan =
        |window: TimeWindow, root: Node| query_plan(scope, window, QueryClass::Analytics, root);
    let counts = |pred: Pred, group: Vec<SpanMetricsCol>| {
        Aggregate::new(
            TableId::SpanMetrics,
            pred,
            group,
            vec![Measure::Sum(SpanMetricsCol::Count.column_ref())],
        )
    };
    let none = || Pred::eq(SpanMetricsCol::Release, "no such release");

    // Weeks start on Monday, and each series holds every week the window overlaps.
    let weeks = TimeSeries::new(
        counts(Pred::all(), vec![SpanMetricsCol::Environment])?,
        SpanMetricsCol::Minute,
        Step::Week,
        Fill::Zero,
    )?;
    let monday = |day: u8| -> Result<Value> {
        Ok(Value::Time(
            crucible_records::CivilDate::new(2026, 11, day)
                .context("a date")?
                .start(),
        ))
    };
    let found = rows(
        ctx,
        plan(window(-8 * DAY, 8 * DAY)?, Node::TimeSeries(weeks)),
    )
    .await?;
    same(
        "zero-filled weeks of two series",
        &values(&found),
        &vec![
            vec![monday(9)?, text("dev"), Value::UInt(0)],
            vec![monday(16)?, text("dev"), Value::UInt(5)],
            vec![monday(23)?, text("dev"), Value::UInt(0)],
            vec![monday(9)?, text("prod"), Value::UInt(0)],
            vec![monday(16)?, text("prod"), Value::UInt(11)],
            vec![monday(23)?, text("prod"), Value::UInt(0)],
        ],
    )?;
    plans += 1;

    // Without a matching row a grouped series has no series, and an ungrouped one is one series of zeros.
    let grouped = TimeSeries::new(
        counts(none(), vec![SpanMetricsCol::Environment])?,
        SpanMetricsCol::Minute,
        Step::Hour,
        Fill::Zero,
    )?;
    let found = rows(ctx, plan(f.window, Node::TimeSeries(grouped))).await?;
    same("a filled grouped series without rows", &found.len(), &0)?;
    let ungrouped = TimeSeries::new(
        counts(none(), Vec::new())?,
        SpanMetricsCol::Minute,
        Step::Hour,
        Fill::Zero,
    )?;
    let found = rows(ctx, plan(f.window, Node::TimeSeries(ungrouped))).await?;
    same(
        "a filled ungrouped series without rows",
        &column(&found, 1),
        &vec![Value::UInt(0); 4],
    )?;
    plans += 2;

    // Every nullable measure: null in the empty buckets, while a count and a sum are zero there.
    let column_of = |c: McpOpsCol| c.column_ref();
    let ops = Aggregate::new(
        TableId::McpOps,
        Pred::all(),
        [] as [McpOpsCol; 0],
        vec![
            Measure::Count,
            Measure::Sum(column_of(McpOpsCol::ProgressCount)),
            Measure::Uniq(column_of(McpOpsCol::ProgressCount)),
            Measure::Uniq(column_of(McpOpsCol::SpanId)),
            Measure::Min(column_of(McpOpsCol::JsonrpcCode)),
            Measure::Max(column_of(McpOpsCol::DurationUs)),
        ],
    )?;
    let series = TimeSeries::new(ops, McpOpsCol::StartTime, Step::Hour, Fill::Zero)?;
    let found = rows(ctx, plan(f.window, Node::TimeSeries(series))).await?;
    let hour = |n: i64| Value::Time(Timestamp::from_micros((at(0).micros() / HOUR + n) * HOUR));
    let absent = |n: i64| {
        vec![
            hour(n),
            Value::UInt(0),
            Value::UInt(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    };
    same(
        "zero-filled nullable measures",
        &values(&found),
        &vec![
            absent(-1),
            vec![
                hour(0),
                Value::UInt(4),
                Value::UInt(3),
                Value::UInt(1),
                Value::UInt(4),
                Value::Int(-1),
                Value::UInt(1),
            ],
            absent(1),
            absent(2),
        ],
    )?;
    plans += 1;

    // A set merge in a filled series: the empty set in the days without a session.
    let sets = Aggregate::new(
        TableId::SessionRollup,
        Pred::all(),
        [] as [SessionRollupCol; 0],
        vec![
            Measure::SetMerge(SessionRollupCol::UserIds.column_ref()),
            Measure::Max(SessionRollupCol::LastSeen.column_ref()),
        ],
    )?;
    let series = TimeSeries::new(sets, SessionRollupCol::Day, Step::Day, Fill::Zero)?;
    let found = rows(
        ctx,
        plan(window(-2 * DAY - HOUR, 2 * HOUR)?, Node::TimeSeries(series)),
    )
    .await?;
    let day = |n: i64| Value::Time(at(n * DAY).date().start());
    same(
        "a zero-filled set merge",
        &values(&found),
        &vec![
            vec![day(-2), texts(&[]), Value::Null],
            vec![
                day(-1),
                texts(&["d", "e"]),
                Value::Time(at(-DAY + 2 * SECOND)),
            ],
            vec![
                day(0),
                texts(&["a", "b", "c"]),
                Value::Time(at(20 * SECOND)),
            ],
        ],
    )?;
    plans += 1;

    // A pick and a latest value in a filled series: null in the buckets without a row.
    let picks = Aggregate::new(
        TableId::TraceStats,
        Pred::all(),
        [] as [TraceStatsCol; 0],
        vec![
            Measure::PickMerge(TraceStatsCol::TraceNamePick.column_ref()),
            Measure::Sum(TraceStatsCol::SpanCount.column_ref()),
        ],
    )?;
    let series = TimeSeries::new(picks, TraceStatsCol::RowDay, Step::Day, Fill::Zero)?;
    let found = rows(
        ctx,
        plan(window(-DAY - HOUR, 2 * HOUR)?, Node::TimeSeries(series)),
    )
    .await?;
    same(
        "a zero-filled pick",
        &values(&found),
        &vec![
            vec![day(-1), Value::Null, Value::UInt(0)],
            vec![day(0), text("Chat Completion"), Value::UInt(6)],
        ],
    )?;
    let latest = Aggregate::new(
        TableId::FactTags,
        Pred::all(),
        [] as [FactTagsCol; 0],
        vec![Measure::LatestByVersion(FactTagsCol::FactId.column_ref())],
    )?;
    let series = TimeSeries::new(latest, FactTagsCol::TargetTime, Step::Hour, Fill::Zero)?;
    let found = rows(ctx, plan(f.window, Node::TimeSeries(series))).await?;
    same(
        "a zero-filled latest value",
        &values(&found),
        &vec![
            vec![hour(-1), Value::Null],
            vec![hour(0), uuid(Uuid::from_bytes([2; 16]))],
            vec![hour(1), Value::Null],
            vec![hour(2), Value::Null],
        ],
    )?;
    plans += 2;

    // A distinct count of a nullable column is null for the group whose values are all null.
    let distinct = Aggregate::new(
        TableId::McpOps,
        Pred::all(),
        [McpOpsCol::DurationUs],
        vec![Measure::Uniq(column_of(McpOpsCol::ProgressCount))],
    )?;
    let mut found = values(&rows(ctx, plan(f.window, Node::Aggregate(distinct))).await?);
    found.sort_by_key(|row| format!("{row:?}"));
    same(
        "a distinct count of a nullable column",
        &found,
        &vec![
            vec![Value::UInt(0), Value::Null],
            vec![Value::UInt(1), Value::UInt(1)],
        ],
    )?;
    plans += 1;
    Ok(plans)
}

/// The guardrails of contract 8.4 that a server applies to a read this small: the rows and the bytes to read.
/// Both stores must refuse the read as too broad, and both must answer it under the limits of its class. The
/// time and memory limits cannot be reached on a server with these rows; the model's are tested with it.
async fn guardrails(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let list = || -> Result<QueryPlan> {
        let request = SpanListRequest {
            filter: Filter::all(),
            limit: 10,
            cursor: None,
            as_of: at(HOUR),
        };
        live(span_list(
            f.scope,
            f.window,
            &clamp(),
            Caller::Session,
            request,
        )?)
    };
    let limits = list()?.limits();
    let mut rows_limit = limits;
    rows_limit.max_rows_to_read = Some(1);
    refused(ctx, with_limits(list()?, rows_limit), &QueryError::TooBroad)
        .await
        .context("a read above max_rows_to_read")?;
    let mut bytes_limit = limits;
    bytes_limit.max_bytes_to_read = Some(1);
    refused(
        ctx,
        with_limits(list()?, bytes_limit),
        &QueryError::TooBroad,
    )
    .await
    .context("a read above max_bytes_to_read")?;
    let mut wide = limits;
    wide.max_bytes_to_read = Some(1 << 40);
    let found = rows(ctx, with_limits(list()?, wide)).await?;
    same("a read inside its limits", &found.len(), &6)?;
    Ok(3)
}

/// Every read of the milestone 2 plans; the count of plans compared.
pub(crate) async fn plans(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let lists = lists(ctx, f).await.context("lists")?;
    let search = search_and_payload(ctx, f)
        .await
        .context("search and payload")?;
    let metrics = metrics(ctx, f).await.context("metrics")?;
    let fills = fills(ctx, f).await.context("fills and nullable outputs")?;
    let guardrails = guardrails(ctx, f).await.context("guardrails")?;
    Ok(lists + search + metrics + fills + guardrails)
}
