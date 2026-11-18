//! Typed plans on the real server (contract sections 8.1 and 8.3): the plans of the first endpoints and one
//! plan per node kind and predicate form, run through `TelemetryStore::execute` over known rows. The adapter
//! validates every returned block against `QueryPlan::output`, so a plan that runs here is rendered, bound,
//! decoded and typed correctly on that server version.

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_env::{QueryError, TelemetryStore};
use crucible_plan::{
    AggOutput, Aggregate, Caller, CmpOp, Direction, FieldRange, Fill, Having, Literal, Measure,
    Node, PointLookup, Pred, QueryClass, QueryPlan, Row, Scan, SemiJoin, Step, TextSearch,
    TimeSeries, TokenMode, TraceBounds, TraceListRequest, Value, data_bounds, span_children,
    trace_list, trace_spans, trace_time_bounds,
};
use crucible_records::{
    McpSide, ProjectId, ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId, Uuid,
    WalEpoch,
};
use crucible_schema::{
    Column, FactTagsCol, SpanCoreCol, SpanMetricsCol, SpanPayloadCol, TraceHeadsCol,
};

use crate::blocks::Rows;
use crate::inserts::{id8, plain_rows, uuid};
use crate::{Ctx, T0, expect};

const SALT: u32 = 7;
const HOUR: i64 = 3_600_000_000;
/// A string with every character the parameter text form escapes, and a few it must leave alone.
const AWKWARD: &str =
    "tab\there, back\\slash, quote', line\nfeed, \r, nul\0, {p0:String}, ? and \u{e9}";

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn span_id(row: usize) -> SpanId {
    let Value::Bytes(bytes) = id8(SALT, row) else {
        return SpanId::ZERO;
    };
    SpanId::from_bytes(bytes.try_into().unwrap_or([0; 8]))
}

fn trace(n: u8) -> TraceId {
    TraceId::from_bytes([0xa0 + n; 16])
}

fn trace_bytes(n: u8) -> Value {
    Value::Bytes(trace(n).to_bytes().to_vec())
}

/// A block of an aggregating table: `data` data rows, then the marker row.
fn rollup(table: TableId, project: ProjectId, epoch: WalEpoch, seq: u64, data: usize) -> Rows {
    Rows::new(table, data + 1)
        .each("project_id", |row| {
            uuid(if row < data {
                project.uuid()
            } else {
                Uuid::NIL
            })
        })
        .each("marker_epoch", |row| {
            uuid(if row < data { Uuid::NIL } else { epoch.uuid() })
        })
        .each("marker_batch_seq", |row| {
            Value::UInt(if row < data { 0 } else { seq })
        })
}

struct Fixture {
    scope: ProjectScope,
    window: TimeWindow,
}

async fn load(ctx: &Ctx) -> Result<Fixture> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let mut seq = 0;
    let mut insert = async |rows: Rows, time: Timestamp| -> Result<()> {
        seq += 1;
        let table = rows.table();
        ctx.insert(&rows, ctx.token(epoch, seq, table, time)?)
            .await
            .with_context(|| format!("fixture rows of {}", table.name()))
    };

    // Six spans of one trace: a root, three children of it and two grandchildren.
    let parent = |row: usize| match row {
        0 => Value::Bytes(vec![0; 8]),
        1..=3 => id8(SALT, 0),
        _ => id8(SALT, 1),
    };
    let spans = plain_rows(TableId::SpanCore, project, epoch, 1, T0, 6, SALT)
        .each("start_time", |row| Value::Time(at(row as i64 * 1_000)))
        .all("trace_id", trace_bytes(0))
        .all("trace_hash", Value::UInt(u64::from(trace(0).trace_hash())))
        .each("parent_span_id", parent)
        .each("is_root", |row| Value::UInt(u64::from(row == 0)))
        .each("name", |row| text(&format!("Chat Completion {row}")))
        .each("user_id", |row| text(&format!("u{}", row % 2)))
        .each("total_tokens", |row| Value::UInt(row as u64))
        .each("tags", |row| {
            Value::Array(vec![text(&format!("tag{row}")), text("common")])
        })
        .each("status_message", |row| {
            text(if row == 5 { AWKWARD } else { "" })
        })
        .all("environment", text("prod"));
    insert(spans, T0).await?;

    let payload = plain_rows(TableId::SpanPayload, project, epoch, 1, T0, 1, SALT)
        .all("trace_id", trace_bytes(0))
        .all("trace_hash", Value::UInt(u64::from(trace(0).trace_hash())))
        .all("input", text("hello world foo"))
        .all("metadata_kv", Value::Array(vec![text("k=v")]));
    insert(payload, T0).await?;

    // Three trace heads, one second apart.
    let heads = Rows::new(TableId::TraceHeads, 3)
        .all("project_id", uuid(project.uuid()))
        .each("start_time", |row| Value::Time(at(row as i64 * 1_000_000)))
        .each("received_at", |row| Value::Time(at(row as i64 * 1_000_000)))
        .each("trace_id", |row| trace_bytes(row as u8))
        .each("trace_hash", |row| {
            Value::UInt(u64::from(trace(row as u8).trace_hash()))
        })
        .each("name", |row| text(&format!("Trace {row}")))
        .all("wal_epoch", uuid(epoch.uuid()));
    insert(heads, T0).await?;

    // Two partial rows of one trace in two blocks, so the read merges them.
    let day = Value::Date(T0.date());
    for (min_start, max_end, has_root, span_count) in [(0, 5_000, 1, 4), (1_000, 9_000, 0, 2)] {
        let stats = rollup(TableId::TraceStats, project, epoch, 0, 1)
            .all("trace_id", trace_bytes(0))
            .all("row_day", day.clone())
            .all("min_start", Value::Time(at(min_start)))
            .all("max_end", Value::Time(at(max_end)))
            .all("min_head_start", Value::Time(at(min_start)))
            .all("has_root", Value::UInt(has_root))
            .all("span_count", Value::UInt(span_count));
        insert(stats, T0).await?;
    }

    // Rollup rows in three minutes of two hours, two of them with a histogram.
    let histogram = |keys: &[u64], counts: &[u64]| {
        let array = |v: &[u64]| Value::Array(v.iter().map(|x| Value::UInt(*x)).collect());
        Value::Tuple2(Box::new(array(keys)), Box::new(array(counts)))
    };
    let minutes = [0, 60_000_000, HOUR];
    let metrics = rollup(TableId::SpanMetrics, project, epoch, 0, 3)
        .each("minute", |row| {
            Value::Time(at(minutes.get(row).copied().unwrap_or(0)).floor_minute())
        })
        .each("release", |row| {
            text(if row < 3 { ["r0", "r1", "r2"][row] } else { "" })
        })
        .each("count", |row| Value::UInt([2, 3, 5, 0][row]))
        .each("dur_hist", |row| match row {
            0 => histogram(&[1, 2], &[1, 1]),
            1 => histogram(&[2], &[4]),
            _ => histogram(&[], &[]),
        });
    insert(metrics, T0).await?;

    let sides = [
        McpSide::Client.value(),
        McpSide::Server.value(),
        McpSide::None.value(),
    ];
    let mcp = rollup(TableId::McpToolRollup, project, epoch, 0, 2)
        .each("side", |row| Value::Enum(sides[row]))
        .each("minute", |row| {
            Value::Time(at(row as i64 * 120_000_000).floor_minute())
        })
        .each("count", |row| Value::UInt(u64::from(row < 2)));
    insert(mcp, T0).await?;

    // Tag facts on two traces; the first trace has two versions.
    let facts = Rows::new(TableId::FactTags, 3)
        .all("project_id", uuid(project.uuid()))
        .all("key", text("quality"))
        .all("target_day", Value::Date(T0.date()))
        .all("target_time", Value::Time(T0))
        .each("trace_id", |row| trace_bytes(if row < 2 { 0 } else { 1 }))
        .each("version", |row| Value::UInt([1, 2, 1][row]))
        .each("fact_id", |row| uuid(Uuid::from_bytes([row as u8 + 1; 16])))
        .all("wal_epoch", uuid(epoch.uuid()));
    insert(facts, T0).await?;

    Ok(Fixture {
        scope: scope(project).map_err(|e| anyhow!("{e}"))?,
        window: TimeWindow::new(at(-HOUR), at(2 * HOUR)).map_err(|e| anyhow!("{e}"))?,
    })
}

async fn rows(ctx: &Ctx, plan: QueryPlan) -> Result<Vec<Row>> {
    let stream = ctx.store.execute(plan).await?;
    Ok(stream.rows().await?)
}

fn values(rows: &[Row]) -> Vec<Vec<Value>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

fn same<T: PartialEq + std::fmt::Debug>(what: &str, got: &T, expected: &T) -> Result<()> {
    expect(got == expected, || {
        format!("{what}: got {got:?}, expected {expected:?}")
    })
}

async fn endpoint_plans(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let (scope, window) = (f.scope, f.window);
    let session = Caller::Session;

    for caller in [session, Caller::Agent] {
        let bounds = rows(ctx, trace_time_bounds(scope, window, caller, trace(0))?).await?;
        same(
            "trace bounds",
            &values(&bounds),
            &vec![vec![
                trace_bytes(0),
                Value::Time(T0),
                Value::Time(at(9_000)),
                Value::UInt(6),
                Value::UInt(1),
                Value::Time(T0),
            ]],
        )?;
    }
    let missing = rows(ctx, trace_time_bounds(scope, window, session, trace(9))?).await?;
    same("bounds of an unknown trace", &missing.len(), &0)?;

    let spans = rows(ctx, trace_spans(scope, T0, at(9_000), session, trace(0))?).await?;
    same("trace spans", &spans.len(), &6)?;
    let start = SpanCoreCol::ALL
        .iter()
        .position(|c| *c == SpanCoreCol::StartTime)
        .context("start_time is a column")?;
    let starts: Vec<Value> = spans.iter().map(|r| r.values()[start].clone()).collect();
    let ascending: Vec<Value> = (0..6).map(|i| Value::Time(at(i * 1_000))).collect();
    same("trace spans in start order", &starts, &ascending)?;
    let message = SpanCoreCol::ALL
        .iter()
        .position(|c| *c == SpanCoreCol::StatusMessage)
        .context("status_message is a column")?;
    same(
        "a string with escapes",
        &spans[5].values()[message],
        &text(AWKWARD),
    )?;

    let trace_of = |rows: &[Row]| -> Vec<Value> {
        let at = TraceHeadsCol::ALL
            .iter()
            .position(|c| *c == TraceHeadsCol::TraceId)
            .unwrap_or_default();
        rows.iter().map(|r| r.values()[at].clone()).collect()
    };
    let list = |cursor, as_of: Timestamp, since| {
        trace_list(
            scope,
            window,
            session,
            TraceListRequest {
                filter: Pred::all(),
                limit: 2,
                cursor,
                as_of,
                since,
            },
        )
    };
    let newest_first = rows(ctx, list(None, at(10_000_000), None)?).await?;
    same(
        "trace list, newest first",
        &trace_of(&newest_first),
        &vec![trace_bytes(2), trace_bytes(1), trace_bytes(0)],
    )?;
    let cursor = Some((at(2_000_000), trace(2).trace_hash(), trace(2)));
    let page = rows(ctx, list(cursor, at(10_000_000), None)?).await?;
    same(
        "trace list after the cursor",
        &trace_of(&page),
        &vec![trace_bytes(1), trace_bytes(0)],
    )?;
    let early = rows(ctx, list(None, at(500_000), None)?).await?;
    same(
        "trace list as of an earlier instant",
        &trace_of(&early),
        &vec![trace_bytes(0)],
    )?;
    let tail = rows(ctx, list(None, at(10_000_000), Some(at(1_000_000)))?).await?;
    same(
        "trace list since a head cursor",
        &trace_of(&tail),
        &vec![trace_bytes(2)],
    )?;

    let bounds = TraceBounds {
        trace_id: trace(0),
        min_start: T0,
        max_end: at(9_000),
    };
    let children = rows(
        ctx,
        span_children(scope, bounds, session, span_id(0), None, None)?,
    )
    .await?;
    same("children of the root", &children.len(), &3)?;
    let after = Some((at(1_000), span_id(1)));
    let more = rows(
        ctx,
        span_children(scope, bounds, session, span_id(0), after, Some(1))?,
    )
    .await?;
    same(
        "children after the cursor, one more than the page",
        &more.len(),
        &2,
    )?;

    let plans = data_bounds(scope, window, session)?;
    let span_bounds = rows(ctx, plans.spans).await?;
    same(
        "span data bounds",
        &values(&span_bounds),
        &vec![vec![
            Value::Time(T0.floor_minute()),
            Value::Time(at(HOUR).floor_minute()),
        ]],
    )?;
    let mut mcp_bounds = values(&rows(ctx, plans.mcp).await?);
    mcp_bounds.sort_by_key(|row| format!("{row:?}"));
    same(
        "mcp data bounds per side",
        &mcp_bounds,
        &vec![
            vec![
                Value::Enum(McpSide::Client.value()),
                Value::Time(T0.floor_minute()),
                Value::Time(T0.floor_minute()),
            ],
            vec![
                Value::Enum(McpSide::Server.value()),
                Value::Time(at(120_000_000).floor_minute()),
                Value::Time(at(120_000_000).floor_minute()),
            ],
        ],
    )?;
    // An aggregate without group dimensions returns no row when nothing matches.
    let other = crucible_auth::test_support::scope(ctx.project()).map_err(|e| anyhow!("{e}"))?;
    let empty = data_bounds(other, window, session)?;
    same(
        "bounds of a project without data",
        &rows(ctx, empty.spans).await?.len(),
        &0,
    )?;
    same(
        "mcp bounds of a project without data",
        &rows(ctx, empty.mcp).await?.len(),
        &0,
    )?;
    Ok(14)
}

async fn node_plans(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let (scope, window) = (f.scope, f.window);
    let plan = |class: QueryClass, root: Node| QueryPlan::new(scope, window, class, root);
    let names = async |pred: Pred| -> Result<Vec<Value>> {
        let scan = Scan::new(TableId::SpanCore, [SpanCoreCol::Name], pred)?.ordered(
            crucible_plan::Order::by([SpanCoreCol::StartTime], Direction::Asc),
        )?;
        let found = rows(ctx, plan(QueryClass::List, Node::Scan(scan))).await?;
        Ok(found.into_iter().flat_map(Row::into_values).collect())
    };
    let named = |rows: &[usize]| -> Vec<Value> {
        rows.iter()
            .map(|row| text(&format!("Chat Completion {row}")))
            .collect()
    };
    let mut plans = 0;

    // Predicate forms.
    let cases: Vec<(&str, Pred, Vec<usize>)> = vec![
        (
            "In",
            Pred::is_in(
                SpanCoreCol::SpanId,
                vec![span_id(1).into(), span_id(3).into(), span_id(4).into()],
            ),
            vec![1, 3, 4],
        ),
        (
            "StartsWith",
            Pred::StartsWith(SpanCoreCol::UserId.column_ref(), "u1".to_owned()),
            vec![1, 3, 5],
        ),
        (
            "HasAny",
            Pred::HasAny(
                SpanCoreCol::Tags.column_ref(),
                vec!["tag4".into(), "absent".into(), "tag2".into()],
            ),
            vec![2, 4],
        ),
        (
            "TextMatch",
            Pred::TextMatch("COMPLETION 5".to_owned()),
            vec![5],
        ),
        (
            "TextMatch below one gram",
            Pred::TextMatch("4".to_owned()),
            vec![4],
        ),
        (
            "Not and Or",
            Pred::Not(Box::new(Pred::Or(vec![
                Pred::eq(SpanCoreCol::UserId, "u0"),
                Pred::cmp(SpanCoreCol::TotalTokens, CmpOp::Ge, 5u64),
            ]))),
            vec![1, 3],
        ),
        (
            "enum equality",
            Pred::And(vec![
                Pred::eq(SpanCoreCol::Level, Literal::Enum(0)),
                Pred::cmp(SpanCoreCol::TotalTokens, CmpOp::Lt, 2u64),
            ]),
            vec![0, 1],
        ),
        (
            "a string with escapes",
            Pred::eq(SpanCoreCol::StatusMessage, AWKWARD),
            vec![5],
        ),
        (
            "a time comparison",
            Pred::cmp(SpanCoreCol::StartTime, CmpOp::Gt, at(3_000)),
            vec![4, 5],
        ),
    ];
    for (name, pred, expected) in cases {
        same(name, &names(pred).await?, &named(&expected))?;
        plans += 1;
    }

    // PointLookup: two keys, one of them absent.
    let key = |row: usize| -> Vec<Literal> {
        vec![
            at(row as i64 * 1_000).floor_minute().into(),
            trace(0).trace_hash().into(),
            at(row as i64 * 1_000).into(),
            span_id(row).into(),
        ]
    };
    let lookup = PointLookup::new(
        TableId::SpanCore,
        [
            SpanCoreCol::StartMinute,
            SpanCoreCol::TraceHash,
            SpanCoreCol::StartTime,
            SpanCoreCol::SpanId,
        ],
        vec![key(2), key(40), key(4)],
        [SpanCoreCol::Name, SpanCoreCol::TotalTokens],
    )?;
    let mut found = values(&rows(ctx, plan(QueryClass::Point, Node::PointLookup(lookup))).await?);
    found.sort_by_key(|row| format!("{row:?}"));
    same(
        "point lookup",
        &found,
        &vec![
            vec![text("Chat Completion 2"), Value::UInt(2)],
            vec![text("Chat Completion 4"), Value::UInt(4)],
        ],
    )?;

    // Aggregate over a plain table: every scalar measure, a condition, an order and a limit.
    let by_user = Aggregate::new(
        TableId::SpanCore,
        Pred::all(),
        [SpanCoreCol::UserId],
        vec![
            Measure::Count,
            Measure::Sum(SpanCoreCol::TotalTokens.column_ref()),
            Measure::Uniq(SpanCoreCol::SpanId.column_ref()),
            Measure::Max(SpanCoreCol::StartTime.column_ref()),
            Measure::Min(SpanCoreCol::Environment.column_ref()),
        ],
    )?
    .having(vec![Having {
        measure: 1,
        op: CmpOp::Ge,
        value: Literal::UInt(6),
    }])?
    .ordered(vec![(AggOutput::Measure(1), Direction::Desc)])?
    .limited(1)?;
    same(
        "aggregate over spans",
        &values(
            &rows(
                ctx,
                plan(QueryClass::AnalyticsRaw, Node::Aggregate(by_user)),
            )
            .await?,
        ),
        &vec![vec![
            text("u1"),
            Value::UInt(3),
            Value::UInt(9),
            Value::UInt(3),
            Value::Time(at(5_000)),
            text("prod"),
        ]],
    )?;

    // Aggregate over a rollup: partial rows merge, histograms add by bucket.
    let merged = Aggregate::new(
        TableId::SpanMetrics,
        Pred::cmp(SpanMetricsCol::Release, CmpOp::Ne, "r2"),
        [SpanMetricsCol::Environment],
        vec![
            Measure::Sum(SpanMetricsCol::Count.column_ref()),
            Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
        ],
    )?;
    let uints = |v: &[u64]| Value::Array(v.iter().map(|x| Value::UInt(*x)).collect());
    same(
        "histogram merge",
        &values(&rows(ctx, plan(QueryClass::Analytics, Node::Aggregate(merged))).await?),
        &vec![vec![
            text(""),
            Value::UInt(5),
            Value::Tuple2(Box::new(uints(&[1, 2])), Box::new(uints(&[1, 5]))),
        ]],
    )?;

    // TimeSeries: hourly buckets of the rollup.
    let series = |fill| -> Result<QueryPlan> {
        let counts = Aggregate::new(
            TableId::SpanMetrics,
            Pred::all(),
            [] as [SpanMetricsCol; 0],
            vec![Measure::Sum(SpanMetricsCol::Count.column_ref())],
        )?
        .ordered(vec![(AggOutput::Measure(0), Direction::Asc)])?;
        let series = TimeSeries::new(counts, SpanMetricsCol::Minute, Step::Hour, fill)?;
        Ok(plan(QueryClass::Analytics, Node::TimeSeries(series)))
    };
    let mut buckets = values(&rows(ctx, series(Fill::None)?).await?);
    buckets.sort_by_key(|row| format!("{row:?}"));
    let hour = |micros: i64| Value::Time(Timestamp::from_micros(at(micros).micros() / HOUR * HOUR));
    same(
        "hourly series",
        &buckets,
        &vec![
            vec![hour(0), Value::UInt(5)],
            vec![hour(HOUR), Value::UInt(5)],
        ],
    )?;
    let refused = ctx.store.execute(series(Fill::Zero)?).await;
    expect(matches!(refused, Err(QueryError::Internal(_))), || {
        "a zero-filled series was not refused".to_owned()
    })?;

    // TextSearch over the payload's text index.
    let search = async |mode: TokenMode, tokens: &[&str]| -> Result<usize> {
        let search = TextSearch::new(
            TableId::SpanPayload,
            [SpanPayloadCol::Input, SpanPayloadCol::Output],
            mode,
            tokens.iter().map(|t| (*t).to_owned()).collect(),
            Pred::all(),
            10,
        )?;
        Ok(
            rows(ctx, plan(QueryClass::Search, Node::TextSearch(search)))
                .await?
                .len(),
        )
    };
    same(
        "all tokens",
        &search(TokenMode::All, &["hello", "world", "foo"]).await?,
        &1,
    )?;
    same(
        "all tokens, one absent",
        &search(TokenMode::All, &["hello", "absent"]).await?,
        &0,
    )?;
    same(
        "any token",
        &search(TokenMode::Any, &["absent", "foo"]).await?,
        &1,
    )?;
    same(
        "phrase",
        &search(TokenMode::Phrase, &["hello", "world"]).await?,
        &1,
    )?;

    // FieldRange: bytes 6..11 of the input.
    let payload_key = [
        SpanPayloadCol::StartMinute.column_ref(),
        SpanPayloadCol::TraceHash.column_ref(),
        SpanPayloadCol::SpanId.column_ref(),
    ];
    let range = FieldRange::new(
        TableId::SpanPayload,
        payload_key,
        vec![
            T0.floor_minute().into(),
            trace(0).trace_hash().into(),
            span_id(0).into(),
        ],
        SpanPayloadCol::Input,
        6,
        5,
    )?;
    same(
        "field range",
        &values(&rows(ctx, plan(QueryClass::Point, Node::FieldRange(range))).await?),
        &vec![vec![text("world")]],
    )?;

    let by_metadata = Scan::new(
        TableId::SpanPayload,
        [SpanPayloadCol::Input],
        Pred::MetadataEq {
            key: "k".to_owned(),
            value: "v".to_owned(),
        },
    )?;
    same(
        "metadata equality",
        &rows(ctx, plan(QueryClass::List, Node::Scan(by_metadata)))
            .await?
            .len(),
        &1,
    )?;

    // SemiJoin: heads of the traces that carry a tag fact, with the latest version per trace.
    let tagged = |max_ids: u32| -> Result<QueryPlan> {
        let facts = Aggregate::new(
            TableId::FactTags,
            Pred::eq(FactTagsCol::Key, "quality"),
            [FactTagsCol::TraceId],
            vec![Measure::LatestByVersion(FactTagsCol::Version.column_ref())],
        )?;
        let heads = Scan::new(TableId::TraceHeads, [TraceHeadsCol::Name], Pred::all())?.ordered(
            crucible_plan::Order::by([TraceHeadsCol::StartTime], Direction::Asc),
        )?;
        let join = SemiJoin::new(
            heads,
            TraceHeadsCol::TraceId,
            Node::Aggregate(facts),
            max_ids,
        )?;
        Ok(plan(QueryClass::List, Node::SemiJoin(join)))
    };
    same(
        "semi-join",
        &values(&rows(ctx, tagged(10)?).await?),
        &vec![vec![text("Trace 0")], vec![text("Trace 1")]],
    )?;
    let too_many = rows(ctx, tagged(1)?).await;
    let too_broad = too_many
        .as_ref()
        .err()
        .and_then(|e| e.downcast_ref::<QueryError>())
        == Some(&QueryError::TooBroad);
    expect(too_broad, || {
        format!("a semi-join over its id cap gave {too_many:?}, not query_too_broad")
    })?;

    let latest = Aggregate::new(
        TableId::FactTags,
        Pred::all(),
        [FactTagsCol::TraceId],
        vec![Measure::LatestByVersion(FactTagsCol::FactId.column_ref())],
    )?
    .ordered(vec![(AggOutput::Group(0), Direction::Asc)])?;
    same(
        "latest by version",
        &values(&rows(ctx, plan(QueryClass::List, Node::Aggregate(latest))).await?),
        &vec![
            vec![trace_bytes(0), uuid(Uuid::from_bytes([2; 16]))],
            vec![trace_bytes(1), uuid(Uuid::from_bytes([3; 16]))],
        ],
    )?;
    Ok(plans + 14)
}

pub(crate) async fn reads(ctx: &Ctx) -> Result<String> {
    let fixture = load(ctx).await?;
    let endpoints = endpoint_plans(ctx, &fixture)
        .await
        .context("endpoint plans")?;
    let nodes = node_plans(ctx, &fixture).await.context("node plans")?;
    Ok(format!(
        "{endpoints} reads of the endpoint builders (trace bounds, trace spans, trace list, span children, \
         data bounds) and {nodes} reads over every node kind and predicate form returned the expected rows, \
         each block valid under the plan's output schema"
    ))
}
