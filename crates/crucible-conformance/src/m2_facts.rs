//! The read plans of the facts and metrics slice on the real server and on the store model (contract 3.8, 8.7
//! and 8.8): the candidate facts of a target and of a page of traces in every fact table, the filter-values
//! lookup over the span rollup, and the trace count of the `traces` metrics source.
//!
//! Every read goes through `differential::rows`, which runs it on both stores and compares the answers, and its
//! rows are also checked against the fixture, which lives in a project of its own. No plan picks a fact's
//! winner: the rows are folded here with `crucible_records::fold_facts`, as the query service folds them.

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_plan::{
    Aggregation, Caller, FactTable, FactsOf, Fill, Filter, FilterField, FilterValuesRequest,
    Granularity, MeasureId, MetricsQuery, RetentionClamp, Row, Source, Step, TextField, Value,
    fact_page, fact_rows, filter_values, metrics_series,
};
use crucible_records::{
    FactId, FactTargetKind, FactVersion, HeadKind, Level, ProjectScope, SpanId, TableId,
    TimeWindow, Timestamp, TraceId, Uuid, fold_facts,
};
use crucible_schema::RetentionSettings;

use crate::blocks::Rows;
use crate::differential::{insert, rows, stamped};
use crate::inserts::uuid;
use crate::reads::rollup;
use crate::{Ctx, T0, expect};

const SECOND: i64 = 1_000_000;
const HOUR: i64 = 3_600 * SECOND;

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn same<T: PartialEq + std::fmt::Debug>(what: &str, got: &T, expected: &T) -> Result<()> {
    expect(got == expected, || {
        format!("{what}: got {got:?}, expected {expected:?}")
    })
}

fn clamp() -> RetentionClamp {
    RetentionClamp::new(RetentionSettings::default(), T0)
}

fn live<T>(plan: Option<T>) -> Result<T> {
    plan.context("the builder returned no plan: the window has expired")
}

pub(crate) struct Fixture {
    scope: ProjectScope,
    /// Two hours from the fixture instant.
    window: TimeWindow,
    trace: TraceId,
    other: TraceId,
    span: SpanId,
}

/// One fact row: target kind, span, session, key, version, id byte, and whether it retracts.
type Fact = (
    FactTargetKind,
    u8,
    &'static str,
    &'static str,
    u64,
    u8,
    bool,
);

/// The tag facts: two versions of one tag of the trace, where the later one wins although its id is smaller in
/// the second half (the half ClickHouse compares first); a retracted tag; a tag of a span and one of a
/// session.
const TAGS: [Fact; 6] = [
    (FactTargetKind::Trace, 0, "", "quality", 1, 0x01, false),
    (FactTargetKind::Trace, 0, "", "quality", 2, 0x02, false),
    (FactTargetKind::Trace, 0, "", "slow", 3, 0x03, false),
    (FactTargetKind::Trace, 0, "", "slow", 4, 0x04, true),
    (FactTargetKind::Span, 7, "", "pinned", 5, 0x05, false),
    (
        FactTargetKind::Session,
        0,
        "session-1",
        "reviewed",
        6,
        0x06,
        false,
    ),
];

/// A UUIDv7 whose first half grows with `n` and whose second half shrinks with it.
fn fact_id(n: u8) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&[0x01, 0x93, 0, 0, 0, n]);
    bytes[6] = 0x70;
    bytes[8] = 0x80 | (0x3f - n.min(0x3f));
    bytes[15] = 0xff - n;
    Uuid::from_bytes(bytes)
}

pub(crate) async fn load(ctx: &Ctx) -> Result<Fixture> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let trace = TraceId::from_bytes([0xf1; 16]);
    let other = TraceId::from_bytes([0xf2; 16]);
    let span = SpanId::from_bytes([7; 8]);
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
        .with_context(|| format!("facts fixture rows of {}", table.name()))
    };

    // The same facts in every table, so every body column is read; the other trace has one fact each.
    for table in FactTable::ALL {
        let id = table.table();
        let count = TAGS.len() + 1;
        let row = |row: usize| TAGS.get(row).copied();
        let mut rows = Rows::new(id, count)
            .all("project_id", uuid(project.uuid()))
            .all("target_day", Value::Date(T0.date()))
            .each("target_time", |row| Value::Time(at(row as i64 * SECOND)))
            .each("target_kind", move |r| {
                Value::Enum(row(r).map_or(FactTargetKind::Trace, |f| f.0).value())
            })
            .each("trace_id", move |r| {
                let target = match row(r) {
                    Some((FactTargetKind::Session, ..)) => TraceId::ZERO,
                    Some(_) => trace,
                    None => other,
                };
                Value::Bytes(target.to_bytes().to_vec())
            })
            .each("span_id", move |r| {
                Value::Bytes(vec![row(r).map_or(0, |f| f.1); 8])
            })
            .each("session_id", move |r| text(row(r).map_or("", |f| f.2)))
            .each("key", move |r| text(row(r).map_or("quality", |f| f.3)))
            .each("version", move |r| Value::UInt(row(r).map_or(9, |f| f.4)))
            .each("fact_id", move |r| {
                uuid(fact_id(row(r).map_or(0x09, |f| f.5)))
            })
            .each("retracted", move |r| {
                Value::UInt(u64::from(row(r).is_some_and(|f| f.6)))
            });
        rows = match table {
            FactTable::Scores => rows
                .all("data_type", Value::Enum(1))
                .each("value_num", |row| Value::Float(row as f64 / 4.0))
                .all("comment", text("good"))
                .all("source", Value::Enum(1))
                .all("metadata", text("{\"k\":1}")),
            FactTable::Tags => rows.each("op", |row| Value::Enum(if row == 1 { 2 } else { 1 })),
            FactTable::Flags => rows.each("value", |row| Value::UInt(row as u64 % 2)),
            FactTable::Corrections => rows
                .all("corrected", text("\"fixed\""))
                .all("note", text("typo")),
        };
        put(rows, T0).await?;
    }

    // Span rollup rows in three releases and two levels.
    let partials: [(&str, Level, u64); 4] = [
        ("v1.2", Level::Default, 5),
        ("v1.10", Level::Error, 5),
        ("v2.0", Level::Default, 9),
        ("v1.2", Level::Error, 1),
    ];
    let of = move |row: usize| partials.get(row).copied();
    let metrics = rollup(TableId::SpanMetrics, project, epoch, 0, partials.len())
        .all("minute", Value::Time(T0.floor_minute()))
        .each("release", move |row| text(of(row).map_or("", |p| p.0)))
        .each("level", move |row| {
            Value::Enum(of(row).map_or(Level::Default, |p| p.1).value())
        })
        .each("count", move |row| Value::UInt(of(row).map_or(0, |p| p.2)));
    put(metrics, T0).await?;

    // Three root heads and a provisional one: the traces source counts the roots.
    let heads = Rows::new(TableId::TraceHeads, 4)
        .all("project_id", uuid(project.uuid()))
        .each("start_time", |row| {
            Value::Time(at([0, 61, 62, 3_700][row] * SECOND))
        })
        .each("trace_id", |row| Value::Bytes(vec![0xe0 + row as u8; 16]))
        .each("head_kind", |row| {
            Value::Enum(
                if row == 2 {
                    HeadKind::Provisional
                } else {
                    HeadKind::Root
                }
                .value(),
            )
        })
        .each("received_at", |row| Value::Time(at(row as i64 * SECOND)));
    put(heads, T0).await?;

    Ok(Fixture {
        scope: scope(project).map_err(|e| anyhow!("{e}"))?,
        window: TimeWindow::new(T0, at(2 * HOUR)).map_err(|e| anyhow!("{e}"))?,
        trace,
        other,
        span,
    })
}

/// The winning fact ids of fact rows, per (target kind, trace, span, session, key), by the reference fold.
fn winners(rows: &[Row]) -> Result<Vec<Uuid>> {
    let mut candidates = Vec::with_capacity(rows.len());
    for row in rows {
        let v = row.values();
        let (Value::UInt(version), Value::Uuid(id), Value::UInt(retracted)) = (&v[5], &v[6], &v[7])
        else {
            return Err(anyhow!("a fact row without its version: {v:?}"));
        };
        let version = FactVersion {
            version: *version,
            fact_id: FactId::from_uuid(*id),
            retracted: *retracted == 1,
        };
        candidates.push((format!("{:?}", &v[..5]), version, *id));
    }
    Ok(fold_facts(candidates).into_values().collect())
}

pub(crate) async fn plans(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let (scope, window, session) = (f.scope, f.window, Caller::Session);
    let mut reads = 0;
    let traces = [f.trace, f.other];
    for table in FactTable::ALL {
        let name = table.table().name();
        // The trace's facts: both versions of `quality` and both rows of `slow`; the fold keeps the newer
        // `quality` and drops the retracted `slow`.
        let found = rows(
            ctx,
            live(fact_rows(
                scope,
                window,
                &clamp(),
                session,
                table,
                FactsOf::Traces(&traces[..1]),
            )?)?,
        )
        .await?;
        reads += 1;
        same(&format!("{name}: facts of the trace"), &found.len(), &4)?;
        same(
            &format!("{name}: winners of the trace"),
            &winners(&found)?,
            &vec![fact_id(0x02)],
        )?;

        // A page of two traces, without the large body columns.
        let page = rows(
            ctx,
            live(fact_page(scope, window, &clamp(), session, table, &traces)?)?,
        )
        .await?;
        reads += 1;
        same(&format!("{name}: facts of the page"), &page.len(), &5)?;

        let span = rows(
            ctx,
            live(fact_rows(
                scope,
                window,
                &clamp(),
                session,
                table,
                FactsOf::Span(f.trace, f.span),
            )?)?,
        )
        .await?;
        let session_rows = rows(
            ctx,
            live(fact_rows(
                scope,
                window,
                &clamp(),
                session,
                table,
                FactsOf::Session("session-1"),
            )?)?,
        )
        .await?;
        reads += 2;
        same(&format!("{name}: facts of the span"), &span.len(), &1)?;
        same(
            &format!("{name}: facts of the session"),
            &session_rows.len(),
            &1,
        )?;
        same(
            &format!("{name}: the session's key"),
            &session_rows[0].values()[4],
            &text("reviewed"),
        )?;
    }

    // Filter values: by count, then value; a prefix on a string field; the plan reads one more than asked.
    let request = |field, prefix: Option<&str>, limit| FilterValuesRequest {
        source: Source::Spans,
        field,
        prefix: prefix.map(str::to_owned),
        limit,
    };
    let release = FilterField::Text(TextField::Release);
    let values =
        |rows: &[Row]| -> Vec<Vec<Value>> { rows.iter().map(|r| r.values().to_vec()).collect() };
    let all = rows(
        ctx,
        live(filter_values(
            scope,
            window,
            &clamp(),
            session,
            &request(release, None, 2),
        )?)?,
    )
    .await?;
    same(
        "release values",
        &values(&all),
        &vec![
            vec![text("v2.0"), Value::UInt(9)],
            vec![text("v1.2"), Value::UInt(6)],
            vec![text("v1.10"), Value::UInt(5)],
        ],
    )?;
    let prefixed = rows(
        ctx,
        live(filter_values(
            scope,
            window,
            &clamp(),
            session,
            &request(release, Some("v1."), 10),
        )?)?,
    )
    .await?;
    same(
        "release values with a prefix",
        &values(&prefixed),
        &vec![
            vec![text("v1.2"), Value::UInt(6)],
            vec![text("v1.10"), Value::UInt(5)],
        ],
    )?;
    let levels = rows(
        ctx,
        live(filter_values(
            scope,
            window,
            &clamp(),
            session,
            &request(FilterField::Level, Some("err"), 10),
        )?)?,
    )
    .await?;
    same(
        "level values",
        &values(&levels),
        &vec![
            vec![Value::Enum(Level::Default.value()), Value::UInt(14)],
            vec![Value::Enum(Level::Error.value()), Value::UInt(6)],
        ],
    )?;
    reads += 3;

    // The traces source: root heads per hour bucket, zero-filled.
    let step = Step::Hour;
    let query = MetricsQuery {
        source: Source::Traces,
        measures: vec![(MeasureId::Traces, Aggregation::Sum)],
        dimensions: Vec::new(),
        filter: Filter::all(),
        granularity: Granularity::Step(step),
        fill: Fill::Zero,
        top_n: None,
    };
    let series = rows(
        ctx,
        live(metrics_series(
            scope,
            window,
            &clamp(),
            session,
            &query,
            None,
        )?)?,
    )
    .await?;
    reads += 1;
    let roots = [at(0), at(61 * SECOND), at(3_700 * SECOND)];
    let (first, buckets) = step.buckets(window);
    let expected: Vec<Vec<Value>> = (0..buckets as i64)
        .map(|i| {
            let bucket = first.saturating_add_micros(i * HOUR);
            let count = roots.iter().filter(|t| step.bucket(**t) == bucket).count();
            vec![Value::Time(bucket), Value::UInt(count as u64)]
        })
        .collect();
    same("trace counts", &values(&series), &expected)?;
    Ok(reads)
}
