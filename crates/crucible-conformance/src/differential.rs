//! The differential check of the simulation's store model against the real server (contract 2.6: the model
//! may be stricter than ClickHouse and never more permissive).
//!
//! `crucible_sim::ch_model::ChModel` is given the same blocks as the server, through the same
//! `TelemetryStore::insert_block`, and then answers the same typed plans and maintenance calls. Every read of the
//! suite goes through [`rows`], which compares the two answers: row for row where the plan defines the order,
//! as multisets where it does not (rows that tie on the order, and plans without one). [`extra`] adds the reads
//! where a model is most likely to drift from a server (null handling, the `UUID` order, text tokens, ties under
//! a limit, window edges, the semi-join cap), before and after the model merges its parts. [`maintenance`]
//! mirrors deletes, mask application, partition drops and the marker purge.
//!
//! A mismatch is a defect of the model, and is fixed there.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use crucible_auth::test_support::scope;
use crucible_env::{InsertSettings, MutationId, MutationStatus, QueryError, TelemetryStore};
use crucible_plan::test_support::{batch_probe, query_plan};
use crucible_plan::{
    AggOutput, Aggregate, CmpOp, DeletePlan, Direction, FieldRange, Fill, Filter, FilterTarget,
    Literal, Measure, Node, Order, Pred, QueryClass, QueryPlan, Row, Scan, SemiJoin, Step,
    TextSearch, TimeSeries, TokenMode, Value,
};
use crucible_records::{
    BatchToken, FactTargetKind, HeadKind, Lsn, ProjectId, ProjectScope, TableId, TagOp, TimeWindow,
    Timestamp, TraceId, Uuid, WalEpoch,
};
use crucible_schema::{
    Column, FactTagsCol, Granularity, McpOpsCol, SpanCoreCol, SpanMetricsCol, SpanPayloadCol,
    TableKind, TraceHeadsCol, route,
};
use crucible_sim::ch_model::Answer;

use crate::blocks::Rows;
use crate::inserts::{id8, plain_rows, rollup_rows, uuid};
use crate::{Ctx, T0, expect};

const HOUR: i64 = 3_600_000_000;
const DAY: i64 = 24 * HOUR;
const SALT: u32 = 21;

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

/// Inserts one block, in `ORDER BY` order, into the server and into the model. Both must accept it.
pub(crate) async fn insert(ctx: &Ctx, rows: Rows, token: BatchToken) -> Result<()> {
    let rows = rows.sorted();
    ctx.insert(&rows, token).await?;
    ctx.model
        .insert_block(rows.encode()?, InsertSettings::new(token, ctx.flavour))
        .await
        .map_err(|error| {
            anyhow!(
                "the model refused a block the server accepted: {error:?}; {:?}",
                ctx.model.violations()
            )
        })
}

/// The rows with the identity of batch `seq` of `epoch`: the ingestion columns of a plain table, or the marker
/// columns of the last row, which is the marker row of every fixture block of an aggregating table.
pub(crate) fn stamped(rows: Rows, epoch: WalEpoch, seq: u64) -> Rows {
    let marker = rows.len().saturating_sub(1);
    match crucible_schema::table(rows.table()).kind {
        TableKind::Plain => rows
            .all("wal_epoch", uuid(epoch.uuid()))
            .all("batch_seq", Value::UInt(seq))
            .all("lsn", Value::UInt(seq)),
        TableKind::Aggregating => rows
            .each("marker_epoch", |row| {
                uuid(if row == marker {
                    epoch.uuid()
                } else {
                    Uuid::NIL
                })
            })
            .each("marker_batch_seq", |row| {
                Value::UInt(if row == marker { seq } else { 0 })
            }),
    }
}

fn sorted(rows: &[Row]) -> Vec<String> {
    let mut images: Vec<String> = rows
        .iter()
        .map(|row| format!("{:?}", row.values()))
        .collect();
    images.sort_unstable();
    images
}

/// Compares the server's rows with the model's answer: run by run, each run as a multiset. When a limit cut
/// the result, the last run may hold other rows of the tie, so only its length is compared.
fn compare(real: &[Row], model: &Answer) -> Result<()> {
    let total: usize = model.runs.iter().map(Vec::len).sum();
    if real.len() != total {
        bail!(
            "the server returned {} rows, the model {total}: {real:?} against {:?}",
            real.len(),
            model.runs
        );
    }
    let mut from = 0;
    for (index, run) in model.runs.iter().enumerate() {
        let theirs = &real[from..from + run.len()];
        from += run.len();
        if model.cut && index + 1 == model.runs.len() {
            continue;
        }
        if sorted(theirs) != sorted(run) {
            bail!("run {index} differs: the server returned {theirs:?}, the model {run:?}");
        }
    }
    Ok(())
}

/// Runs a plan on the server and on the model and returns the server's rows. The answers must agree; an error
/// must be of the same class on both.
pub(crate) async fn rows(ctx: &Ctx, plan: QueryPlan) -> Result<Vec<Row>> {
    let real = match ctx.store.execute(plan.clone()).await {
        Ok(stream) => stream.rows().await,
        Err(error) => Err(error),
    };
    let model = ctx.model.read(&plan);
    ctx.compared.fetch_add(1, Ordering::Relaxed);
    match (real, model) {
        (Ok(real), Ok(model)) => {
            compare(&real, &model).with_context(|| format!("model differential of {plan:?}"))?;
            // The stream of the trait method carries the same rows as the direct read.
            let streamed = ctx.model.execute(plan).await?.rows().await?;
            expect(streamed == model.into_rows(), || {
                "the model's stream differs from its own read".to_owned()
            })?;
            Ok(real)
        }
        (Err(real), Err(model)) => {
            expect(
                std::mem::discriminant(&real) == std::mem::discriminant(&model),
                || format!("the server failed with {real:?}, the model with {model:?}"),
            )?;
            Err(real.into())
        }
        (Ok(real), Err(model)) => {
            bail!("the server returned {real:?}, the model failed with {model:?} for {plan:?}")
        }
        (Err(real), Ok(model)) => bail!(
            "the server failed with {real:?}, the model returned {:?} for {plan:?}",
            model.runs
        ),
    }
}

/// A plan both stores must refuse with an error of one class.
pub(crate) async fn refused(ctx: &Ctx, plan: QueryPlan, class: &QueryError) -> Result<()> {
    match rows(ctx, plan).await {
        Ok(found) => bail!("a plan that must fail with {class:?} returned {found:?}"),
        Err(error) => match error.downcast_ref::<QueryError>() {
            Some(found) if std::mem::discriminant(found) == std::mem::discriminant(class) => Ok(()),
            _ => Err(error),
        },
    }
}

struct Extra {
    scope: ProjectScope,
    window: TimeWindow,
    trace: TraceId,
}

const NAMES: [&str; 10] = [
    "Alpha one",
    "\u{c9}COLE normale",
    "\u{e9}cole primaire",
    "ab",
    "x",
    "Chat GPT",
    "chat gpt",
    "Zed",
    "first instant",
    "past the end",
];

async fn load(ctx: &Ctx) -> Result<Extra> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let trace = TraceId::from_bytes([0xb1; 16]);
    let trace_bytes = Value::Bytes(trace.to_bytes().to_vec());
    let window = TimeWindow::new(at(-HOUR), at(2 * HOUR)).map_err(|e| anyhow!("{e}"))?;
    let mut seq = 0u64;
    let mut next = || {
        seq += 1;
        seq
    };
    let stamp = |rows: Rows, seq: u64| stamped(rows, epoch, seq);

    // Ten spans: two that tie on their start, one at the first instant of the window and one at its end.
    let start = |row: usize| match row {
        3 => at(2_000),
        8 => window.from(),
        9 => window.to(),
        _ => at(row as i64 * 1_000),
    };
    let n = next();
    let spans = plain_rows(TableId::SpanCore, project, epoch, n, T0, 10, SALT)
        .each("start_time", |row| Value::Time(start(row)))
        .each("start_minute", |row| Value::Time(start(row).floor_minute()))
        .all("trace_id", trace_bytes.clone())
        .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())))
        .each("name", |row| text(NAMES[row]))
        .each("user_id", |row| text(&format!("u{}", row % 3)))
        .each("total_tokens", |row| Value::UInt(row as u64))
        .each("parent_span_id", |row| id8(SALT, row / 2))
        .each("completion_start_time", |row| {
            if row % 2 == 1 && row < 8 {
                Value::Time(at(row as i64))
            } else {
                Value::Null
            }
        });
    insert(
        ctx,
        stamp(spans, n),
        ctx.token(epoch, n, TableId::SpanCore, T0)?,
    )
    .await?;

    let n = next();
    let inputs = [
        "Hello WORLD foo-bar",
        "h\u{e9}llo w\u{f6}rld",
        "one two",
        "H\u{c9}LLO snake_case99",
    ];
    let payload = plain_rows(TableId::SpanPayload, project, epoch, n, T0, 4, SALT)
        .all("trace_id", trace_bytes.clone())
        .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())))
        .each("input", |row| text(inputs[row]));
    insert(
        ctx,
        stamp(payload, n),
        ctx.token(epoch, n, TableId::SpanPayload, T0)?,
    )
    .await?;

    // Nullable numbers: one group whose values are all null.
    let n = next();
    let ops = plain_rows(TableId::McpOps, project, epoch, n, T0, 4, SALT)
        .each("duration_us", |row| Value::UInt(row as u64 / 2))
        .each("progress_count", |row| {
            [Value::Null, Value::Null, Value::UInt(3), Value::Null][row].clone()
        })
        .each("jsonrpc_code", |row| {
            [Value::Null, Value::Int(-1), Value::Int(2), Value::Null][row].clone()
        });
    insert(
        ctx,
        stamp(ops, n),
        ctx.token(epoch, n, TableId::McpOps, T0)?,
    )
    .await?;

    // Two facts of one version whose ids order differently by their first and by their second half, and two
    // versions on another trace.
    let n = next();
    let low_second_half = Uuid::from_bytes([0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let high_second_half = Uuid::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    let facts = Rows::new(TableId::FactTags, 4)
        .all("project_id", uuid(project.uuid()))
        .all("key", text("quality"))
        .all("target_day", Value::Date(T0.date()))
        .all("target_time", Value::Time(T0))
        .each("trace_id", |row| {
            Value::Bytes(vec![0xc0 + (row / 2) as u8; 16])
        })
        .each("version", |row| Value::UInt([5, 5, 1, 2][row]))
        .each("fact_id", |row| {
            uuid(
                [
                    low_second_half,
                    high_second_half,
                    Uuid::from_bytes([7; 16]),
                    Uuid::from_bytes([3; 16]),
                ][row],
            )
        });
    insert(
        ctx,
        stamp(facts, n),
        ctx.token(epoch, n, TableId::FactTags, T0)?,
    )
    .await?;

    // The trace list's tags over tag edits (contract 3.8). Four heads: two sent with `beta`. Trace 0xd0 was
    // tagged `reviewed` and untagged `beta`; 0xd1 lost `beta` and got it back; 0xd2's latest `reviewed` row is a
    // retraction; 0xd3 has an addition and a removal of `x` of one version whose ids order differently by their
    // first and by their second half, so the server's `UUID` order picks the latest.
    let n = next();
    let sent: [&[&str]; 4] = [&["beta"], &["beta"], &[], &[]];
    let heads = Rows::new(TableId::TraceHeads, 4)
        .all("project_id", uuid(project.uuid()))
        .all("received_at", Value::Time(T0))
        .each("span_id", |row| id8(SALT, row))
        .each("start_time", |row| Value::Time(at(row as i64 * 1_000)))
        .each("trace_id", |row| Value::Bytes(vec![0xd0 + row as u8; 16]))
        .each("trace_hash", |row| Value::UInt(row as u64))
        .all("head_kind", Value::Enum(HeadKind::Root.value()))
        .each("tags", |row| {
            Value::Array(sent[row].iter().map(|tag| text(tag)).collect())
        });
    insert(
        ctx,
        stamp(heads, n),
        ctx.token(epoch, n, TableId::TraceHeads, T0)?,
    )
    .await?;
    let n = next();
    let edits = [
        (
            0,
            "reviewed",
            TagOp::Add,
            1,
            false,
            Uuid::from_bytes([0x11; 16]),
        ),
        (
            0,
            "beta",
            TagOp::Remove,
            2,
            false,
            Uuid::from_bytes([0x12; 16]),
        ),
        (
            1,
            "beta",
            TagOp::Remove,
            3,
            false,
            Uuid::from_bytes([0x13; 16]),
        ),
        (
            1,
            "beta",
            TagOp::Add,
            4,
            false,
            Uuid::from_bytes([0x14; 16]),
        ),
        (
            2,
            "reviewed",
            TagOp::Add,
            5,
            false,
            Uuid::from_bytes([0x15; 16]),
        ),
        (
            2,
            "reviewed",
            TagOp::Add,
            6,
            true,
            Uuid::from_bytes([0x16; 16]),
        ),
        (3, "x", TagOp::Remove, 7, false, low_second_half),
        (3, "x", TagOp::Add, 7, false, high_second_half),
    ];
    let tags = Rows::new(TableId::FactTags, edits.len())
        .all("project_id", uuid(project.uuid()))
        .all("target_day", Value::Date(T0.date()))
        .all("target_time", Value::Time(T0))
        .all("target_kind", Value::Enum(FactTargetKind::Trace.value()))
        .each("trace_id", |row| {
            Value::Bytes(vec![0xd0 + edits[row].0; 16])
        })
        .each("key", |row| text(edits[row].1))
        .each("op", |row| Value::Enum(edits[row].2.value()))
        .each("version", |row| Value::UInt(edits[row].3))
        .each("retracted", |row| Value::UInt(u64::from(edits[row].4)))
        .each("fact_id", |row| uuid(edits[row].5));
    insert(
        ctx,
        stamp(tags, n),
        ctx.token(epoch, n, TableId::FactTags, T0)?,
    )
    .await?;

    // Two partial rows of one rollup key in two blocks; their histograms cancel to zero in two buckets.
    let histogram = |keys: &[u64], counts: &[u64]| {
        let array = |v: &[u64]| Value::Array(v.iter().map(|x| Value::UInt(*x)).collect());
        Value::Tuple2(Box::new(array(keys)), Box::new(array(counts)))
    };
    for (keys, counts) in [([1, 2], [0, 3]), ([2, 5], [1, 0])] {
        let n = next();
        let metrics = rollup_rows(project, epoch, n, T0, 1)
            .each("release", |_| text(""))
            .each("dur_hist", |row| {
                if row == 0 {
                    histogram(&keys, &counts)
                } else {
                    histogram(&[], &[])
                }
            });
        insert(ctx, metrics, ctx.token(epoch, n, TableId::SpanMetrics, T0)?).await?;
    }

    Ok(Extra {
        scope: scope(project).map_err(|e| anyhow!("{e}"))?,
        window,
        trace,
    })
}

/// The extra reads, each with the fewest rows it must return so that no comparison passes on two empty answers.
fn plans(f: &Extra) -> Result<Vec<(&'static str, QueryPlan, usize)>> {
    let plan = |class: QueryClass, root: Node| query_plan(f.scope, f.window, class, root);
    let by_start = |direction| Order::by([SpanCoreCol::StartTime], direction);
    let names = |pred: Pred| Scan::new(TableId::SpanCore, [SpanCoreCol::Name], pred);
    let scan = |scan: Scan| plan(QueryClass::List, Node::Scan(scan));
    let completed = SpanCoreCol::CompletionStartTime;
    let mut out = Vec::new();

    // The trace list's tags filter: the traces whose latest tag edits are resolved over `fact_tags` first, then
    // semi-joined into the heads, negated and combined with the heads' own tags.
    let tagged = |tags: &[&str]| Filter::Tags(tags.iter().map(|tag| (*tag).to_owned()).collect());
    for (name, filter, least) in [
        (
            "the effective tags: an addition, and a removal of a sent tag",
            tagged(&["reviewed"]),
            1,
        ),
        (
            "the effective tags: a removal undone, and an addition retracted",
            tagged(&["beta", "reviewed"]),
            2,
        ),
        (
            "the effective tags: two edits of one version, by the order of ids",
            tagged(&["x"]),
            1,
        ),
        (
            "the effective tags, negated",
            Filter::Not(Box::new(tagged(&["reviewed"]))),
            3,
        ),
    ] {
        let heads = Scan::new(
            TableId::TraceHeads,
            [TraceHeadsCol::TraceId, TraceHeadsCol::StartTime],
            filter.pred(FilterTarget::TraceHeads)?,
        )?;
        out.push((name, plan(QueryClass::List, Node::Scan(heads)), least));
    }

    out.push((
        "the window keeps its first instant and not its end",
        scan(names(Pred::all())?.ordered(by_start(Direction::Asc))?),
        9,
    ));
    out.push((
        "a limit inside a tie",
        scan(
            names(Pred::all())?
                .ordered(by_start(Direction::Asc))?
                .limited(4)?,
        ),
        4,
    ));
    out.push((
        "a descending scan after a cursor",
        scan(
            Scan::new(
                TableId::SpanCore,
                [SpanCoreCol::Name, SpanCoreCol::StartTime],
                Pred::all(),
            )?
            .ordered(by_start(Direction::Desc))?
            .after(vec![at(5_000).into()])?,
        ),
        6,
    ));
    for (name, needle, least) in [
        ("a text match on a lowered accent", "\u{e9}cole", 1),
        ("a text match below one gram", "ab", 1),
        ("a text match in another case", "CHAT G", 2),
        ("a text match on an accent in another case", "\u{c9}cole", 0),
    ] {
        out.push((
            name,
            scan(names(Pred::TextMatch(needle.to_owned()))?.ordered(by_start(Direction::Asc))?),
            least,
        ));
    }
    out.push((
        "a comparison with a nullable column",
        scan(names(Pred::cmp(completed, CmpOp::Gt, at(2)))?),
        3,
    ));
    out.push((
        "the negation of a comparison with a nullable column",
        scan(names(Pred::Not(Box::new(Pred::cmp(
            completed,
            CmpOp::Gt,
            at(2),
        ))))?),
        1,
    ));
    out.push((
        "a prefix and an array test",
        scan(names(Pred::And(vec![
            Pred::StartsWith(SpanCoreCol::Name.column_ref(), "Chat".to_owned()),
            Pred::Not(Box::new(Pred::HasAny(
                SpanCoreCol::Tags.column_ref(),
                vec!["absent".into()],
            ))),
        ]))?),
        1,
    ));

    let by_user = Aggregate::new(
        TableId::SpanCore,
        Pred::all(),
        [SpanCoreCol::UserId],
        vec![
            Measure::Count,
            Measure::Min(completed.column_ref()),
            Measure::Max(completed.column_ref()),
            Measure::Uniq(SpanCoreCol::Name.column_ref()),
            Measure::Sum(SpanCoreCol::TotalTokens.column_ref()),
        ],
    )?
    .ordered(vec![(AggOutput::Measure(1), Direction::Desc)])?;
    out.push((
        "measures over a nullable column, nulls last",
        plan(QueryClass::AnalyticsRaw, Node::Aggregate(by_user)),
        3,
    ));
    let total = Aggregate::new(
        TableId::SpanCore,
        Pred::all(),
        [] as [SpanCoreCol; 0],
        vec![Measure::Count],
    )?;
    out.push((
        "the count of the window",
        plan(QueryClass::AnalyticsRaw, Node::Aggregate(total)),
        1,
    ));
    let nullable_sums = Aggregate::new(
        TableId::McpOps,
        Pred::all(),
        [McpOpsCol::DurationUs],
        vec![
            Measure::Sum(McpOpsCol::ProgressCount.column_ref()),
            Measure::Sum(McpOpsCol::JsonrpcCode.column_ref()),
            Measure::Max(McpOpsCol::JsonrpcCode.column_ref()),
        ],
    )?
    .ordered(vec![(AggOutput::Measure(0), Direction::Asc)])?;
    out.push((
        "sums of nullable numbers",
        plan(QueryClass::AnalyticsRaw, Node::Aggregate(nullable_sums)),
        2,
    ));
    let minutes = Aggregate::new(
        TableId::SpanCore,
        Pred::all(),
        [SpanCoreCol::UserId],
        vec![Measure::Count],
    )?;
    out.push((
        "minute buckets of a microsecond column",
        plan(
            QueryClass::AnalyticsRaw,
            Node::TimeSeries(TimeSeries::new(
                minutes,
                SpanCoreCol::StartTime,
                Step::Minute,
                Fill::None,
            )?),
        ),
        4,
    ));

    for (name, mode, tokens, least) in [
        ("a token in another case", TokenMode::All, vec!["HELLO"], 0),
        ("two tokens", TokenMode::All, vec!["hello", "world"], 1),
        (
            "a token beside a hyphen",
            TokenMode::All,
            vec!["foo", "bar"],
            1,
        ),
        (
            "a token with a separator in it",
            TokenMode::All,
            vec!["foo-bar"],
            0,
        ),
        (
            "a token with an accent",
            TokenMode::Any,
            vec!["h\u{e9}llo", "absent"],
            1,
        ),
        (
            "an accent in another case",
            TokenMode::Any,
            vec!["H\u{c9}LLO"],
            0,
        ),
        ("any of two tokens", TokenMode::Any, vec!["two", "world"], 2),
        (
            "a token whose accent is in upper case in the column",
            TokenMode::All,
            vec!["h\u{c9}llo"],
            1,
        ),
        (
            "tokens beside an underscore, with digits",
            TokenMode::All,
            vec!["snake", "case99"],
            1,
        ),
    ] {
        let search = TextSearch::new(
            TableId::SpanPayload,
            [SpanPayloadCol::Input, SpanPayloadCol::Output],
            mode,
            tokens.into_iter().map(str::to_owned).collect(),
            Pred::all(),
            10,
        )?;
        out.push((
            name,
            plan(QueryClass::Search, Node::TextSearch(search)),
            least,
        ));
    }

    let payload_key = [
        SpanPayloadCol::StartMinute.column_ref(),
        SpanPayloadCol::TraceHash.column_ref(),
        SpanPayloadCol::SpanId.column_ref(),
    ];
    let span = |row: usize| -> Literal {
        match id8(SALT, row) {
            Value::Bytes(bytes) => Literal::Bytes(bytes),
            _ => Literal::Bytes(Vec::new()),
        }
    };
    for (name, offset, len) in [
        ("a byte range that cuts a character", 1, 1),
        ("a byte range past the end", 100, 5),
        ("a byte range over the end", 8, 100),
    ] {
        let range = FieldRange::new(
            TableId::SpanPayload,
            payload_key,
            vec![
                T0.floor_minute().into(),
                f.trace.trace_hash().into(),
                span(1),
            ],
            SpanPayloadCol::Input,
            offset,
            len,
        )?;
        out.push((name, plan(QueryClass::Point, Node::FieldRange(range)), 1));
    }

    // The right side yields ten ids of which five are distinct: the cap counts the distinct ones.
    let parents = Scan::new(TableId::SpanCore, [SpanCoreCol::ParentSpanId], Pred::all())?;
    let join = SemiJoin::new(
        names(Pred::all())?.ordered(by_start(Direction::Asc))?,
        SpanCoreCol::SpanId,
        Node::Scan(parents),
        5,
    )?;
    out.push((
        "a semi-join at its cap of distinct ids",
        plan(QueryClass::List, Node::SemiJoin(join)),
        5,
    ));

    let latest = Aggregate::new(
        TableId::FactTags,
        Pred::all(),
        [FactTagsCol::TraceId],
        vec![
            Measure::LatestByVersion(FactTagsCol::FactId.column_ref()),
            Measure::LatestByVersion(FactTagsCol::Version.column_ref()),
            Measure::Min(FactTagsCol::FactId.column_ref()),
            Measure::Max(FactTagsCol::FactId.column_ref()),
        ],
    )?
    .ordered(vec![(AggOutput::Measure(0), Direction::Asc)])?;
    out.push((
        "the latest fact of one version, by the order of ids",
        plan(QueryClass::List, Node::Aggregate(latest)),
        2,
    ));

    let merged = Aggregate::new(
        TableId::SpanMetrics,
        Pred::all(),
        [SpanMetricsCol::Release],
        vec![
            Measure::Sum(SpanMetricsCol::Count.column_ref()),
            Measure::HistogramMerge(SpanMetricsCol::DurHist.column_ref()),
        ],
    )?;
    out.push((
        "a histogram whose buckets sum to zero",
        plan(QueryClass::Analytics, Node::Aggregate(merged)),
        1,
    ));
    Ok(out)
}

/// The extra reads, before and after the model merges every partition down to one part.
pub(crate) async fn extra(ctx: &Ctx) -> Result<usize> {
    let fixture = load(ctx).await?;
    let mut done = 0;
    let mut failures: Vec<String> = Vec::new();
    for merged in [false, true] {
        if merged {
            ctx.model.merge_all();
        }
        for (name, plan, least) in plans(&fixture)? {
            // Every mismatch is reported, not only the first.
            match rows(ctx, plan).await {
                Ok(found) if found.len() >= least => done += 1,
                Ok(found) => failures.push(format!(
                    "{name}: {} rows, expected at least {least}",
                    found.len()
                )),
                Err(error) => failures.push(format!("{name} (merged: {merged}): {error:#}")),
            }
        }
        let parents = Scan::new(TableId::SpanCore, [SpanCoreCol::ParentSpanId], Pred::all())?;
        let names = Scan::new(TableId::SpanCore, [SpanCoreCol::Name], Pred::all())?;
        let join = SemiJoin::new(names, SpanCoreCol::SpanId, Node::Scan(parents), 4)?;
        let over = query_plan(
            fixture.scope,
            fixture.window,
            QueryClass::List,
            Node::SemiJoin(join),
        );
        refused(ctx, over, &QueryError::TooBroad)
            .await
            .context("a semi-join over its cap of distinct ids")?;
        // The server's distinct count of a nullable column is nullable, and so is the plan's output: four
        // spans of the fixture carry a completion time, each its own.
        let distinct = Aggregate::new(
            TableId::SpanCore,
            Pred::all(),
            [] as [SpanCoreCol; 0],
            vec![Measure::Uniq(SpanCoreCol::CompletionStartTime.column_ref())],
        )?;
        let distinct = query_plan(
            fixture.scope,
            fixture.window,
            QueryClass::AnalyticsRaw,
            Node::Aggregate(distinct),
        );
        let counted = rows(ctx, distinct)
            .await
            .context("a distinct count of a nullable column")?;
        expect(
            counted.first().map(Row::values) == Some(&[Value::UInt(4)][..]),
            || format!("a distinct count of a nullable column returned {counted:?}"),
        )?;
        done += 2;
    }
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    Ok(done)
}

pub(crate) async fn finished(ctx: &Ctx, store: &dyn TelemetryStore, id: &MutationId) -> Result<()> {
    for _ in 0..120 {
        match store.mutation_status(id).await? {
            MutationStatus::Done => return Ok(()),
            MutationStatus::Pending => ctx.env.clock.sleep(Duration::from_millis(250)).await,
            MutationStatus::Failed { code, reason } => bail!("mutation failed ({code}): {reason}"),
        }
    }
    bail!("mutation {} did not finish", id.name())
}

pub(crate) fn same<T: PartialEq + std::fmt::Debug>(what: &str, real: &T, model: &T) -> Result<()> {
    expect(real == model, || {
        format!("{what}: the server gives {real:?}, the model {model:?}")
    })
}

/// The maintenance calls of contract 2.6, on the server and on the model, with every observable answer
/// compared: reads after each step, partition projects, masked parts and probes.
pub(crate) async fn maintenance(ctx: &Ctx) -> Result<String> {
    let table = TableId::McpOps;
    let (first, second, epoch) = (ctx.project(), ctx.project(), ctx.epoch());
    let partition = route(table, Granularity::Day, T0)?;
    let both: [&dyn TelemetryStore; 2] = [&ctx.store, &ctx.model];
    let block = |project: ProjectId, seq: u64, time: Timestamp, count: usize| {
        plain_rows(table, project, epoch, seq, time, count, SALT)
            .each("duration_us", |row| Value::UInt(row as u64))
            .all("lsn", Value::UInt(seq))
    };
    for (seq, project) in [(1, first), (2, second)] {
        insert(
            ctx,
            block(project, seq, T0, 10),
            ctx.token(epoch, seq, table, T0)?,
        )
        .await?;
    }
    let window = TimeWindow::new(at(-HOUR), at(40 * DAY)).map_err(|e| anyhow!("{e}"))?;
    let mut checks = 0usize;
    let mut count = async |project: ProjectId, what: &str| -> Result<u64> {
        let counted = Aggregate::new(
            table,
            Pred::all(),
            [] as [McpOpsCol; 0],
            vec![
                Measure::Count,
                Measure::Sum(McpOpsCol::DurationUs.column_ref()),
            ],
        )?;
        let plan = query_plan(
            scope(project).map_err(|e| anyhow!("{e}"))?,
            window,
            QueryClass::AnalyticsRaw,
            Node::Aggregate(counted),
        );
        checks += 1;
        let found = rows(ctx, plan).await.with_context(|| what.to_owned())?;
        Ok(match found.first().map(Row::values) {
            Some([Value::UInt(count), _]) => *count,
            _ => 0,
        })
    };
    let mine = |projects: Vec<ProjectId>| -> Vec<ProjectId> {
        projects
            .into_iter()
            .filter(|p| *p == first || *p == second)
            .collect()
    };
    let projects = async |table: TableId, partition, what: &str| -> Result<Vec<ProjectId>> {
        let real = mine(ctx.store.partition_projects(table, partition).await?);
        let model = mine(ctx.model.partition_projects(table, partition).await?);
        same(what, &real, &model)?;
        Ok(real)
    };
    let probed =
        async |table: TableId, partition, epoch: WalEpoch, seq: u64, what: &str| -> Result<bool> {
            let probe = batch_probe(table, partition, epoch, Lsn::new(seq))?;
            let real = ctx.store.probe(probe).await?;
            same(what, &real, &ctx.model.probe(probe).await?)?;
            Ok(real)
        };

    same(
        "the projects of the partition",
        &projects(table, partition, "partition projects")
            .await?
            .len(),
        &2,
    )?;

    // A typed delete hides exactly the matching rows of one project, at once, on both.
    let some = DeletePlan::project(
        scope(first).map_err(|e| anyhow!("{e}"))?,
        table,
        partition,
        Pred::cmp(McpOpsCol::DurationUs, CmpOp::Ge, 4u64),
    )?;
    let mut deletes = Vec::new();
    for store in both {
        deletes.push(store.delete(some.clone()).await.context("delete")?);
    }
    let left = (
        count(first, "after the delete").await?,
        count(second, "after the delete").await?,
    );
    expect(left == (4, 10), || {
        format!("after the delete the projects hold {left:?} rows, not (4, 10)")
    })?;
    for (store, id) in both.iter().zip(&deletes) {
        finished(ctx, *store, id).await?;
        let masked = store.masked_parts(table, partition).await?;
        expect(masked >= 1, || "no part carries a delete mask".to_owned())?;
    }
    for store in both {
        let mask = store
            .apply_deleted_mask(table, partition)
            .await
            .context("apply mask")?;
        finished(ctx, store, &mask).await?;
        let masked = store.masked_parts(table, partition).await?;
        expect(masked == 0, || format!("{masked} parts still carry a mask"))?;
        let unlisted = MutationId::new(table, "mutation_999999999.txt".to_owned());
        expect(
            store.mutation_status(&unlisted).await? == MutationStatus::Done,
            || "a mutation the store does not list is not done".to_owned(),
        )?;
    }
    count(first, "after the mask application").await?;

    // Deleting the rest of a project removes it from the partition's projects and from the probe's scan.
    let rest = DeletePlan::project(
        scope(first).map_err(|e| anyhow!("{e}"))?,
        table,
        partition,
        Pred::all(),
    )?;
    for store in both {
        store.delete(rest.clone()).await?;
    }
    same(
        "the projects after a project's last row is deleted",
        &projects(table, partition, "partition projects after the delete").await?,
        &vec![second],
    )?;
    let gone = probed(table, partition, epoch, 1, "the probe of a deleted block").await?;
    let kept = probed(table, partition, epoch, 2, "the probe of a kept block").await?;
    expect(!gone && kept, || {
        format!("the probes read {gone} for the deleted block and {kept} for the kept one")
    })?;

    // Dropping a partition removes its rows; dropping it again succeeds. A block sent again with its token
    // after the drop is compared too: whether the server still deduplicates it is the server's to say.
    let later = at(30 * DAY);
    let other = route(table, Granularity::Day, later)?;
    let token = ctx.token(epoch, 3, table, later)?;
    insert(ctx, block(second, 3, later, 5), token).await?;
    same(
        "rows before the drop",
        &count(second, "before the drop").await?,
        &15,
    )?;
    for store in both {
        for _ in 0..2 {
            store
                .drop_partition(table, other)
                .await
                .context("drop partition")?;
        }
    }
    same(
        "rows after the drop",
        &count(second, "after the drop").await?,
        &10,
    )?;
    probed(table, other, epoch, 3, "the probe of a dropped block").await?;
    insert(ctx, block(second, 3, later, 5), token).await?;
    let again = count(second, "after the block is sent again").await?;
    probed(
        table,
        other,
        epoch,
        3,
        "the probe after the block is sent again",
    )
    .await?;

    // Markers: invisible to partition projects, and purged by the marker delete.
    let rollup = TableId::SpanMetrics;
    let month = route(rollup, Granularity::Day, T0)?;
    insert(
        ctx,
        rollup_rows(first, epoch, 9, T0, 3),
        ctx.token(epoch, 9, rollup, T0)?,
    )
    .await?;
    expect(
        probed(rollup, month, epoch, 9, "the marker probe").await?,
        || "the marker is not there".to_owned(),
    )?;
    same(
        "rollup partition projects",
        &projects(rollup, month, "rollup partition projects").await?,
        &vec![first],
    )?;
    for (days, expected) in [(-1, true), (1, false)] {
        let purge = DeletePlan::marker_purge(rollup, month, at(days * DAY), Pred::all())?;
        for store in both {
            store.delete(purge.clone()).await?;
        }
        let found = probed(rollup, month, epoch, 9, "the marker probe after a purge").await?;
        expect(found == expected, || {
            format!("after the purge of markers older than {days} days the probe reads {found}")
        })?;
    }
    let data = Aggregate::new(
        rollup,
        Pred::all(),
        [SpanMetricsCol::Release],
        vec![Measure::Sum(SpanMetricsCol::Count.column_ref())],
    )?;
    let data = query_plan(
        scope(first).map_err(|e| anyhow!("{e}"))?,
        window,
        QueryClass::Analytics,
        Node::Aggregate(data),
    );
    same(
        "data rows after the marker purge",
        &rows(ctx, data).await?.len(),
        &3,
    )?;

    Ok(format!(
        "{} reads, the partition projects, the masked parts and the probes agree between the model and the \
         server through a typed delete, the mask application, a full delete, a partition drop and the marker \
         purge; a block sent again with its token after its partition was dropped leaves {again} rows on both",
        checks + 1
    ))
}
