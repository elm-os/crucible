//! The differential check of the store model for the five MCP tables (contracts 3.5 to 3.7, 3.10 and 6.4):
//! `mcp_ops`, `mcp_content`, `mcp_tool_rollup`, `mcp_deploys` and `mcp_tool_inventory`.
//!
//! Several blocks per table, with keys that repeat from block to block, go to the server and to
//! `crucible_sim::ch_model::ChModel`. Every read then runs on both, before and after the model merges its parts,
//! and the answers must agree and equal what the rows say: the sums, the `sumMap` merge of both histogram
//! columns (the bucket keys of `semconv_hist` are `UInt8`), the minimum and maximum of the seen times, and the
//! bounded set of capture sources. Rows of different capture sources stay apart, because the capture source is a
//! key column (contract 9.5).

use anyhow::{Context as _, Result, anyhow};
use crucible_auth::test_support::scope;
use crucible_plan::test_support::query_plan;
use crucible_plan::{AggOutput, Aggregate, Direction, Measure, Node, Pred, QueryClass, Value};
use crucible_records::{CaptureSource, ErrorClass, McpSide, TableId, TimeWindow, Timestamp};
use crucible_schema::{
    Column, McpContentCol, McpDeploysCol, McpOpsCol, McpToolInventoryCol, McpToolRollupCol,
};

use crate::blocks::Rows;
use crate::differential::{insert, rows, stamped};
use crate::inserts::plain_rows;
use crate::reads::rollup;
use crate::{Ctx, T0, expect};

const HOUR: i64 = 3_600_000_000;
const SALT: u32 = 33;

fn at(micros: i64) -> Timestamp {
    T0.saturating_add_micros(micros)
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn uints(items: &[u64]) -> Value {
    Value::Array(items.iter().map(|item| Value::UInt(*item)).collect())
}

fn histogram(keys: &[u64], counts: &[u64]) -> Value {
    Value::Tuple2(Box::new(uints(keys)), Box::new(uints(counts)))
}

/// The capture source and error class of the data rows of a rollup block: an SDK row, a proxy row with the
/// same other key columns, and an SDK row of a caller fault.
const ROLLUP_ROWS: [(CaptureSource, ErrorClass, u64); 3] = [
    (CaptureSource::Sdk, ErrorClass::None, 0),
    (CaptureSource::ProxyStdio, ErrorClass::None, 0),
    (CaptureSource::Sdk, ErrorClass::Validation, 1),
];

/// Runs the MCP reads on the server and on the model; returns how many reads agreed.
pub(crate) async fn mcp(ctx: &Ctx) -> Result<String> {
    let (project, epoch) = (ctx.project(), ctx.epoch());
    let scope = scope(project).map_err(|e| anyhow!("{e}"))?;
    let window = TimeWindow::new(at(-HOUR), at(2 * HOUR)).map_err(|e| anyhow!("{e}"))?;
    let mut seq = 0;
    let mut put = async |rows: Rows, time: Timestamp| -> Result<()> {
        seq += 1;
        let table = rows.table();
        let rows = stamped(rows, epoch, seq);
        insert(ctx, rows, ctx.token(epoch, seq, table, time)?)
            .await
            .with_context(|| format!("rows of {}", table.name()))
    };

    // Three blocks of the tool rollup with the same three keys: block `b` counts `b + 1` times as much.
    for block in 0..3u64 {
        let scale = block + 1;
        let data = |row: usize| ROLLUP_ROWS.get(row).copied();
        let tool = rollup(TableId::McpToolRollup, project, epoch, 0, 3)
            .all("side", Value::Enum(McpSide::Server.value()))
            .all("minute", Value::Time(T0.floor_minute()))
            .each("server_name", |row| {
                text(if data(row).is_some() { "files" } else { "" })
            })
            .each("deploy_id", |row| {
                text(if data(row).is_some() { "blue" } else { "" })
            })
            .each("capture_source", |row| {
                Value::Enum(data(row).map_or(CaptureSource::Sdk, |d| d.0).value())
            })
            .each("error_class", |row| {
                Value::Enum(data(row).map_or(ErrorClass::None, |d| d.1).value())
            })
            .each("caller_fault", |row| {
                Value::UInt(data(row).map_or(0, |d| d.2))
            })
            .each("count", |row| {
                Value::UInt(data(row).map_or(0, |_| scale * (row as u64 + 1)))
            })
            .each("request_bytes", |row| {
                Value::UInt(data(row).map_or(0, |_| 100 * scale))
            })
            .each("response_bytes", |row| {
                Value::UInt(data(row).map_or(0, |_| 1_000 * scale))
            })
            .each("dur_hist", |row| match data(row) {
                // One bucket that every block has and one of its own.
                Some(_) => histogram(&[7, 100 + block], &[scale, 1]),
                None => histogram(&[], &[]),
            })
            .each("semconv_hist", |row| match data(row) {
                Some(_) => histogram(&[row as u64, 14], &[scale, 2]),
                None => histogram(&[], &[]),
            })
            .each("last_seen", |row| {
                Value::Time(if data(row).is_some() {
                    at(block as i64 * 1_000_000)
                } else {
                    Timestamp::EPOCH
                })
            });
        put(tool, T0).await?;
    }

    // Two blocks of deploys and of inventory: one key seen earlier and later, and a second capture source.
    for block in 0..2i64 {
        let deploys = rollup(TableId::McpDeploys, project, epoch, 0, 2)
            .each("server_name", |row| {
                text(if row < 2 { "files" } else { "" })
            })
            .each("deploy_id", |row| text(if row < 2 { "blue" } else { "" }))
            .each("server_version", |row| {
                text(if row < 2 { "1.0.0" } else { "" })
            })
            .each("capture_source", |row| {
                Value::Enum(
                    if row == 1 {
                        CaptureSource::ProxyStdio
                    } else {
                        CaptureSource::Sdk
                    }
                    .value(),
                )
            })
            .each("first_seen", |row| {
                Value::Time(at((block - 1) * 60_000_000 + row as i64))
            })
            .each("last_seen", |row| {
                Value::Time(at(block * 60_000_000 + row as i64))
            });
        put(deploys, T0).await?;

        let sources = move |row: usize| match (row, block) {
            (0, 0) => uints(&[u64::from(CaptureSource::Otlp.value())]),
            (0, _) => uints(&[
                u64::from(CaptureSource::Sdk.value()),
                u64::from(CaptureSource::Otlp.value()),
            ]),
            (1, _) => uints(&[u64::from(CaptureSource::ProxyHttp.value())]),
            _ => uints(&[]),
        };
        let inventory = rollup(TableId::McpToolInventory, project, epoch, 0, 2)
            .each("server_name", |row| {
                text(if row < 2 { "files" } else { "" })
            })
            .each("deploy_id", |row| text(if row < 2 { "blue" } else { "" }))
            .each("tool_name", |row| {
                text(["read_file", "write_file", ""][row.min(2)])
            })
            .each("schema_hash", |row| {
                Value::UInt(if row < 2 { 0xfeed + row as u64 } else { 0 })
            })
            .each("first_seen", |row| {
                Value::Time(if row < 2 {
                    at((block - 1) * 60_000_000)
                } else {
                    Timestamp::EPOCH
                })
            })
            .each("last_seen", |row| {
                Value::Time(if row < 2 {
                    at(block * 60_000_000)
                } else {
                    Timestamp::EPOCH
                })
            })
            .each("capture_sources", sources);
        put(inventory, T0).await?;
    }

    // Two blocks of operations and of their content: both sides, two capture sources, a caller fault.
    for block in 0..2u64 {
        let seq_now = 100 + block;
        let ops = plain_rows(
            TableId::McpOps,
            project,
            epoch,
            seq_now,
            T0,
            4,
            SALT + block as u32,
        )
        .each("side", |row| {
            Value::Enum(
                if row == 3 {
                    McpSide::Client
                } else {
                    McpSide::Server
                }
                .value(),
            )
        })
        .each("capture_source", |row| {
            Value::Enum(
                if row == 1 {
                    CaptureSource::ProxyStdio
                } else {
                    CaptureSource::Sdk
                }
                .value(),
            )
        })
        .each("caller_fault", |row| Value::UInt(u64::from(row == 2)))
        .each("duration_us", |row| Value::UInt(10 * (row as u64 + 1)))
        .each("content_captured", |row| Value::UInt(u64::from(row < 2)))
        .each("jsonrpc_code", |row| {
            if row == 2 {
                Value::Int(-32602)
            } else {
                Value::Null
            }
        })
        .each("http_status", |row| {
            if row == 1 {
                Value::UInt(200)
            } else {
                Value::Null
            }
        });
        put(ops, T0).await?;
        let content = plain_rows(
            TableId::McpContent,
            project,
            epoch,
            seq_now,
            T0,
            2,
            SALT + block as u32,
        )
        .all("side", Value::Enum(McpSide::Server.value()))
        .each("args", |row| text(&format!("{{\"q\":{row}}}")))
        .each("args_bytes", |row| Value::UInt(7 + row as u64))
        .all("result_bytes", Value::UInt(3));
        put(content, T0).await?;
    }

    let plan = |class: QueryClass, aggregate: Aggregate| {
        query_plan(scope, window, class, Node::Aggregate(aggregate))
    };
    let tool_measures = || {
        vec![
            Measure::Sum(McpToolRollupCol::Count.column_ref()),
            Measure::Sum(McpToolRollupCol::RequestBytes.column_ref()),
            Measure::Sum(McpToolRollupCol::ResponseBytes.column_ref()),
            Measure::HistogramMerge(McpToolRollupCol::DurHist.column_ref()),
            Measure::HistogramMerge(McpToolRollupCol::SemconvHist.column_ref()),
            Measure::Max(McpToolRollupCol::LastSeen.column_ref()),
        ]
    };
    let mut reads = 0;
    for merged in [false, true] {
        if merged {
            ctx.model.merge_all();
        }
        let when = if merged { "after" } else { "before" };

        // The tool rollup per capture source and error class: three keys, never summed across sources.
        let by_key = Aggregate::new(
            TableId::McpToolRollup,
            Pred::all(),
            [
                McpToolRollupCol::CaptureSource,
                McpToolRollupCol::ErrorClass,
            ],
            tool_measures(),
        )?
        .ordered(vec![(AggOutput::Measure(0), Direction::Asc)])?;
        let found = rows(ctx, plan(QueryClass::Analytics, by_key)).await?;
        let counts: Vec<Vec<Value>> = found
            .iter()
            .map(|row| row.values().iter().take(5).cloned().collect())
            .collect();
        // Blocks scale by 1, 2 and 3: six times the first block's values.
        let expected = |source: CaptureSource, class: ErrorClass, count: u64| {
            vec![
                Value::Enum(source.value()),
                Value::Enum(class.value()),
                Value::UInt(6 * count),
                Value::UInt(600),
                Value::UInt(6_000),
            ]
        };
        expect(
            counts
                == vec![
                    expected(CaptureSource::Sdk, ErrorClass::None, 1),
                    expected(CaptureSource::ProxyStdio, ErrorClass::None, 2),
                    expected(CaptureSource::Sdk, ErrorClass::Validation, 3),
                ],
            || format!("the tool rollup by key {when} the merge: {counts:?}"),
        )?;
        let last: Vec<Value> = found
            .iter()
            .filter_map(|row| row.values().last().cloned())
            .collect();
        expect(
            last.iter().all(|seen| *seen == Value::Time(at(2_000_000))),
            || format!("the last seen of the tool rollup {when} the merge: {last:?}"),
        )?;
        // The whole rollup of the minute: every key in one group.
        let total = Aggregate::new(
            TableId::McpToolRollup,
            Pred::all(),
            [McpToolRollupCol::ServerName],
            tool_measures(),
        )?;
        let found = rows(ctx, plan(QueryClass::Analytics, total)).await?;
        expect(
            found.first().and_then(|row| row.values().get(1)) == Some(&Value::UInt(36)),
            || format!("the tool rollup of the server {when} the merge: {found:?}"),
        )?;

        // Deploys: first seen is the minimum and last seen the maximum over the blocks, per capture source.
        let deploys = Aggregate::new(
            TableId::McpDeploys,
            Pred::all(),
            [
                McpDeploysCol::ServerName,
                McpDeploysCol::DeployId,
                McpDeploysCol::ServerVersion,
                McpDeploysCol::CaptureSource,
            ],
            vec![
                Measure::Min(McpDeploysCol::FirstSeen.column_ref()),
                Measure::Max(McpDeploysCol::LastSeen.column_ref()),
            ],
        )?
        .ordered(vec![(AggOutput::Measure(0), Direction::Asc)])?;
        let found = rows(ctx, plan(QueryClass::Analytics, deploys)).await?;
        let seen: Vec<Vec<Value>> = found
            .iter()
            .map(|row| row.values().iter().skip(3).cloned().collect())
            .collect();
        let deploy = |source: CaptureSource, row: i64| {
            vec![
                Value::Enum(source.value()),
                Value::Time(at(-60_000_000 + row)),
                Value::Time(at(60_000_000 + row)),
            ]
        };
        expect(
            seen == vec![
                deploy(CaptureSource::Sdk, 0),
                deploy(CaptureSource::ProxyStdio, 1),
            ],
            || format!("the deploys {when} the merge: {seen:?}"),
        )?;

        // The inventory: the seen times, and the union of the capture sources as a bounded set.
        let inventory = Aggregate::new(
            TableId::McpToolInventory,
            Pred::all(),
            [
                McpToolInventoryCol::ToolName,
                McpToolInventoryCol::SchemaHash,
            ],
            vec![
                Measure::Min(McpToolInventoryCol::FirstSeen.column_ref()),
                Measure::Max(McpToolInventoryCol::LastSeen.column_ref()),
                Measure::SetMerge(McpToolInventoryCol::CaptureSources.column_ref()),
            ],
        )?
        .ordered(vec![(AggOutput::Group(0), Direction::Asc)])?;
        let found = rows(ctx, plan(QueryClass::Analytics, inventory)).await?;
        let tools: Vec<Vec<Value>> = found.iter().map(|row| row.values().to_vec()).collect();
        let tool = |name: &str, hash: u64, sources: &[CaptureSource]| {
            vec![
                text(name),
                Value::UInt(hash),
                Value::Time(at(-60_000_000)),
                Value::Time(at(60_000_000)),
                uints(
                    &sources
                        .iter()
                        .map(|source| u64::from(source.value()))
                        .collect::<Vec<_>>(),
                ),
            ]
        };
        expect(
            tools
                == vec![
                    tool(
                        "read_file",
                        0xfeed,
                        &[CaptureSource::Sdk, CaptureSource::Otlp],
                    ),
                    tool("write_file", 0xfeee, &[CaptureSource::ProxyHttp]),
                ],
            || format!("the tool inventory {when} the merge: {tools:?}"),
        )?;

        // Operations per capture source and caller fault, and their content.
        let ops = Aggregate::new(
            TableId::McpOps,
            Pred::all(),
            [McpOpsCol::CaptureSource, McpOpsCol::CallerFault],
            vec![
                Measure::Count,
                Measure::Sum(McpOpsCol::DurationUs.column_ref()),
                Measure::Sum(McpOpsCol::JsonrpcCode.column_ref()),
                Measure::Max(McpOpsCol::HttpStatus.column_ref()),
            ],
        )?
        .ordered(vec![(AggOutput::Measure(1), Direction::Asc)])?;
        let found = rows(ctx, plan(QueryClass::AnalyticsRaw, ops)).await?;
        let per_source: Vec<Vec<Value>> = found
            .iter()
            .map(|row| row.values().iter().take(4).cloned().collect())
            .collect();
        let group = |source: CaptureSource, fault: u64, count: u64, duration: u64| {
            vec![
                Value::Enum(source.value()),
                Value::UInt(fault),
                Value::UInt(count),
                Value::UInt(duration),
            ]
        };
        expect(
            per_source
                == vec![
                    group(CaptureSource::ProxyStdio, 0, 2, 40),
                    group(CaptureSource::Sdk, 1, 2, 60),
                    group(CaptureSource::Sdk, 0, 4, 100),
                ],
            || format!("the operations per capture source {when} the merge: {per_source:?}"),
        )?;
        let content = Aggregate::new(
            TableId::McpContent,
            Pred::all(),
            [McpContentCol::Side],
            vec![
                Measure::Count,
                Measure::Sum(McpContentCol::ArgsBytes.column_ref()),
                Measure::Sum(McpContentCol::ResultBytes.column_ref()),
            ],
        )?;
        let found = rows(ctx, plan(QueryClass::AnalyticsRaw, content)).await?;
        let stored: Vec<Vec<Value>> = found.iter().map(|row| row.values().to_vec()).collect();
        expect(
            stored
                == vec![vec![
                    Value::Enum(McpSide::Server.value()),
                    Value::UInt(4),
                    Value::UInt(30),
                    Value::UInt(12),
                ]],
            || format!("the content rows {when} the merge: {stored:?}"),
        )?;
        reads += 6;
    }
    let violations = ctx.model.violations();
    expect(violations.is_empty(), || {
        format!("the model recorded {violations:?}")
    })?;
    Ok(format!(
        "{reads} reads of the five MCP tables returned the same rows on the server and on the store model, \
         before and after the model merged its parts: sums, both histogram merges, seen times and the set of \
         capture sources, with rows of different capture sources apart"
    ))
}
