//! The export plans on the real server and on the store model (contracts 8.7 and 12.8): the keyset page of
//! `span_core` in key order under the export's `as_of`, the run of tied rows of a span sent twice, the payload,
//! MCP and captured-content lookups of a page, and the facts of the page's traces in every fact table.
//!
//! Every read goes through `differential::rows`, which runs it on both stores and compares the answers, and its
//! rows are also checked against the fixture.

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_plan::{
    ExportKey, ExportSelection, FactTable, RetentionClamp, Row, Value, export_facts,
    export_mcp_content, export_mcp_ops, export_payloads, export_spans, export_ties,
};
use crucible_records::{McpSide, ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{Column, RetentionSettings, SpanCoreCol};

use crate::blocks::Rows;
use crate::differential::{insert, rows, stamped};
use crate::inserts::uuid;
use crate::{Ctx, T0, expect};

const SECOND: i64 = 1_000_000;
const HOUR: i64 = 3_600 * SECOND;

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
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

/// The stored copies of the fixture's spans: span number, start offset in seconds, arrival offset in seconds.
/// Span 2 was sent twice, and span 5 arrived after the export's `as_of`.
const COPIES: [(u8, i64, i64); 6] = [
    (1, 1, 1),
    (2, 2, 2),
    (2, 2, 5),
    (3, 3, 3),
    (4, 4, 4),
    (5, 5, 3_600),
];

/// The arrival bound of the reads: after every copy but the last.
const AS_OF: i64 = 10;

pub(crate) struct Fixture {
    scope: ProjectScope,
    window: TimeWindow,
    trace: TraceId,
}

fn span(n: u8) -> SpanId {
    SpanId::from_bytes([n; 8])
}

pub(crate) async fn load(ctx: &Ctx) -> Result<Fixture> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let trace = TraceId::from_bytes([0xe7; 16]);
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
        .with_context(|| format!("export fixture rows of {}", table.name()))
    };
    let copy = |row: usize| COPIES[row.min(COPIES.len() - 1)];
    let keyed = |table: TableId| {
        Rows::new(table, COPIES.len())
            .all("project_id", uuid(project.uuid()))
            .each("start_minute", move |row| {
                Value::Time(at(copy(row).1 * SECOND).floor_minute())
            })
            .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())))
            .each("start_time", move |row| {
                Value::Time(at(copy(row).1 * SECOND))
            })
            .each("span_id", move |row| {
                Value::Bytes(span(copy(row).0).to_bytes().to_vec())
            })
            .all("trace_id", Value::Bytes(trace.to_bytes().to_vec()))
            .each("received_at", move |row| {
                Value::Time(at(copy(row).2 * SECOND))
            })
            .each("lsn", |row| Value::UInt(row as u64 + 1))
    };
    let core = keyed(TableId::SpanCore)
        .each("end_time", move |row| {
            Value::Time(at(copy(row).1 * SECOND + 5))
        })
        .each("name", move |row| {
            Value::String(format!("span {}", copy(row).0))
        })
        .each("mcp_side", move |row| {
            let side = if copy(row).0 == 3 {
                McpSide::Client
            } else {
                McpSide::None
            };
            Value::Enum(side.value())
        });
    put(core, T0).await?;
    let payload = keyed(TableId::SpanPayload)
        .each("input", |row| Value::String(format!("{{\"copy\":{row}}}")));
    put(payload, T0).await?;
    // The MCP operation of span 3 and its captured content.
    for table in [TableId::McpOps, TableId::McpContent] {
        let rows = Rows::new(table, 1)
            .all("project_id", uuid(project.uuid()))
            .all("start_minute", Value::Time(at(3 * SECOND).floor_minute()))
            .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())))
            .all("start_time", Value::Time(at(3 * SECOND)))
            .all("span_id", Value::Bytes(span(3).to_bytes().to_vec()))
            .all("trace_id", Value::Bytes(trace.to_bytes().to_vec()));
        let rows = if table == TableId::McpOps {
            rows.all("side", Value::Enum(McpSide::Client.value()))
                .all("method", Value::String("tools/call".to_owned()))
        } else {
            rows.all("args", Value::String("{\"q\":1}".to_owned()))
        };
        put(rows, T0).await?;
    }
    // A tag of the trace and one of span 1, and a score of the trace.
    for (table, count) in [(TableId::FactTags, 2), (TableId::FactScores, 1)] {
        let rows = Rows::new(table, count)
            .all("project_id", uuid(project.uuid()))
            .all("target_day", Value::Date(T0.date()))
            .all("target_time", Value::Time(at(SECOND)))
            .each("target_kind", |row| {
                Value::Enum(if row == 0 { 1 } else { 2 })
            })
            .all("trace_id", Value::Bytes(trace.to_bytes().to_vec()))
            .each("span_id", |row| {
                Value::Bytes(span(if row == 0 { 0 } else { 1 }).to_bytes().to_vec())
            })
            .each("key", |row| Value::String(format!("key {row}")))
            .each("version", |row| Value::UInt(row as u64 + 1))
            .each("fact_id", |row| {
                uuid(crucible_records::Uuid::from_bytes([0x70 + row as u8; 16]))
            });
        put(rows, T0).await?;
    }
    Ok(Fixture {
        scope: scope(project).map_err(|e| anyhow!("{e}"))?,
        window: TimeWindow::new(T0, at(2 * HOUR)).map_err(|e| anyhow!("{e}"))?,
        trace,
    })
}

/// The span numbers of `span_core` rows, in order.
fn spans_of(rows: &[Row]) -> Result<Vec<u8>> {
    let at = SpanCoreCol::ALL
        .iter()
        .position(|c| *c == SpanCoreCol::SpanId)
        .ok_or_else(|| anyhow!("span_core has no span_id"))?;
    rows.iter()
        .map(|row| match &row.values()[at] {
            Value::Bytes(bytes) => bytes
                .first()
                .copied()
                .ok_or_else(|| anyhow!("an empty span id")),
            other => Err(anyhow!("a span id that is not bytes: {other:?}")),
        })
        .collect()
}

pub(crate) async fn plans(ctx: &Ctx, f: &Fixture) -> Result<usize> {
    let (scope, window, as_of) = (f.scope, f.window, at(AS_OF * SECOND));
    let all = ExportSelection::default();
    let mut reads = 0;

    // A page of two rows: span 1, then the first copy of span 2 in no defined order between the two copies.
    let first = rows(
        ctx,
        live(export_spans(scope, window, &clamp(), all, as_of, None, 2)?)?,
    )
    .await?;
    reads += 1;
    same("the first page", &spans_of(&first)?, &vec![1, 2])?;

    // After span 2's position: neither copy of it again, and not span 5, which arrived after `as_of`.
    let key: ExportKey = (
        at(2 * SECOND).floor_minute(),
        f.trace.trace_hash(),
        at(2 * SECOND),
        span(2),
    );
    let next = rows(
        ctx,
        live(export_spans(
            scope,
            window,
            &clamp(),
            all,
            as_of,
            Some(key),
            10,
        )?)?,
    )
    .await?;
    reads += 1;
    same("the page after span 2", &spans_of(&next)?, &vec![3, 4])?;

    // The run of tied rows at span 2's position: both copies.
    let ties = rows(
        ctx,
        live(export_ties(scope, window, &clamp(), all, as_of, key)?)?,
    )
    .await?;
    reads += 1;
    same("the tied copies", &spans_of(&ties)?, &vec![2, 2])?;

    // A selection that holds no row of the fixture.
    let other = [TraceId::from_bytes([0x01; 16])];
    let none = ExportSelection {
        trace_ids: &other,
        user_id: Some("nobody"),
        session_id: None,
    };
    let empty = rows(
        ctx,
        live(export_spans(
            scope,
            window,
            &clamp(),
            none,
            as_of,
            None,
            10,
        )?)?,
    )
    .await?;
    reads += 1;
    same("a selection of nothing", &empty.len(), &0)?;

    // The lookups of the page's spans: both payload rows of span 2, the MCP rows of span 3.
    let keys: Vec<(Timestamp, TraceId, SpanId)> = (1..=4)
        .map(|n| (at(i64::from(n) * SECOND), f.trace, span(n)))
        .collect();
    let payloads = rows(ctx, live(export_payloads(scope, &clamp(), &keys)?)?).await?;
    reads += 1;
    same("the payload rows", &payloads.len(), &5)?;
    let ops = rows(ctx, live(export_mcp_ops(scope, &clamp(), &keys)?)?).await?;
    reads += 1;
    same("the MCP operation rows", &ops.len(), &1)?;
    let content = rows(ctx, live(export_mcp_content(scope, &clamp(), &keys)?)?).await?;
    reads += 1;
    same("the captured content rows", &content.len(), &1)?;

    // The facts of the trace and of its spans, in every fact table.
    for (table, expected) in [
        (FactTable::Scores, 1),
        (FactTable::Tags, 2),
        (FactTable::Flags, 0),
        (FactTable::Corrections, 0),
    ] {
        let facts = rows(
            ctx,
            live(export_facts(scope, window, &clamp(), table, &[f.trace])?)?,
        )
        .await?;
        reads += 1;
        same(
            &format!("the facts in {}", table.table().name()),
            &facts.len(),
            &expected,
        )?;
    }
    Ok(reads)
}
