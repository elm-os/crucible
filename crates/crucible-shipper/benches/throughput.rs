//! Row derivation and block building on one thread, as an ignored timing test. Run it with
//! `cargo test --release -p crucible-shipper --bench throughput -- --ignored --nocapture`.
//!
//! It times what the shipper does with a frame after stage B: derive the rows of every table, then sort,
//! combine, encode and compress the block of every (table, partition) pair. One lane has agent spans only; the
//! other has MCP operations, which also fill the five MCP tables.
// Timing reads the clock directly, which product code may only do through `Env`; benches may.
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::print_stdout)]

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::{Duration, Instant};

use crucible_chfmt::Compression;
use crucible_normalize::{
    Admission, IdentityCtx, IngestFrame, NormCtx, NormalizedFrame, PriceTable, StageB, normalize,
};
use crucible_records::{
    ApiKeyId, Lsn, PartitionId, PayloadCaps, ProjectId, TableId, Timestamp, WalEpoch,
};
use crucible_schema::Granularity;
use crucible_shipper::tables::{RoutedRow, derive};
use crucible_shipper::value::Row;
use crucible_shipper::{BatchId, pair_block};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

const SPANS: u64 = 2_000;

fn string(key: &str, value: String) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value)),
        }),
        key_strindex: 0,
    }
}

fn int(key: &str, value: i64) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::IntValue(value)),
        }),
        key_strindex: 0,
    }
}

/// A span of about 1 KiB: a generation of an agent run, or with `mcp` a server-side tool call.
fn span(i: u64, mcp: bool) -> Span {
    let mut attributes = if mcp {
        vec![
            string("mcp.method.name", "tools/call".to_owned()),
            string("gen_ai.tool.name", format!("tool-{}", i % 7)),
            string("crucible.mcp.capture_source", "sdk".to_owned()),
            string("crucible.mcp.era", "modern".to_owned()),
            string("crucible.mcp.transport", "streamable_http".to_owned()),
            string("mcp.protocol.version", "2026-07-28".to_owned()),
            string("jsonrpc.request.id", i.to_string()),
            string("crucible.mcp.client.name", "inspector".to_owned()),
            string("crucible.mcp.result_type", "complete".to_owned()),
            int("crucible.mcp.request.bytes", 512),
            int("http.response.status_code", 200),
        ]
    } else {
        vec![
            string("gen_ai.operation.name", "chat".to_owned()),
            string("gen_ai.request.model", "model-large-2026-11".to_owned()),
            string("gen_ai.response.model", "model-large-2026-11".to_owned()),
            int("gen_ai.usage.input_tokens", 1_234),
            int("gen_ai.usage.output_tokens", 567),
            string("session.id", format!("session-{}", i % 97)),
            string("user.id", format!("user-{}", i % 13)),
        ]
    };
    attributes.push(string("app.note", "x".repeat(700)));
    Span {
        trace_id: (i / 16 + 1).to_be_bytes().repeat(2),
        span_id: (i + 1).to_be_bytes().to_vec(),
        parent_span_id: if i.is_multiple_of(16) {
            Vec::new()
        } else {
            (i / 16 * 16 + 1).to_be_bytes().to_vec()
        },
        name: "operation".to_owned(),
        kind: if mcp { 2 } else { 3 },
        start_time_unix_nano: 1_794_907_800_000_000_000 + i * 1_000_000,
        end_time_unix_nano: 1_794_907_800_000_500_000 + i * 1_000_000,
        attributes,
        ..Span::default()
    }
}

/// One frame of [`SPANS`] spans through stage A and stage B.
fn frame(mcp: bool) -> (NormalizedFrame, Admission) {
    let body = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![
                    string("service.name", "agent".to_owned()),
                    string("service.version", "1.4.2".to_owned()),
                ],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: (0..SPANS).map(|i| span(i, mcp)).collect(),
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    }
    .encode_to_vec();
    let frame = IngestFrame {
        wal_epoch: WalEpoch::from_bytes([1; 16]),
        lsn: Lsn::new(10),
        received_at: Timestamp::from_micros(1_794_907_801_000_000),
        project_id: ProjectId::from_bytes([2; 16]),
        api_key_id: ApiKeyId::from_bytes([3; 16]),
        compat_dialect: 0,
        sdk_name: "",
        sdk_version: "",
        rejected: &[],
        otlp: &body,
    };
    let ctx = NormCtx::new(PriceTable::bundled().unwrap(), PayloadCaps::DEFAULT);
    let mut normalized = normalize(&frame, &ctx).unwrap();
    let admission = StageB::default()
        .admit(&mut normalized, &IdentityCtx::Suppressed)
        .unwrap();
    normalized.stamp_batch_seq(Lsn::new(10));
    (normalized, admission)
}

/// Runs `f` for at least `budget` and returns iterations per second.
fn rate(budget: Duration, mut f: impl FnMut() -> usize) -> f64 {
    for _ in 0..2 {
        black_box(f());
    }
    let start = Instant::now();
    let mut iterations = 0u32;
    while start.elapsed() < budget {
        black_box(f());
        iterations += 1;
    }
    f64::from(iterations) / start.elapsed().as_secs_f64()
}

#[test]
#[ignore = "a timing run; use --release"]
fn derive_and_block_throughput() {
    let batch = BatchId {
        epoch: WalEpoch::from_bytes([1; 16]),
        first_lsn: Lsn::new(10),
        last_lsn: Lsn::new(10),
    };
    let marker_at = Timestamp::from_micros(1_794_907_801_000_000);
    for (lane, mcp) in [("agent spans", false), ("mcp operations", true)] {
        let (normalized, admission) = frame(mcp);
        let derive_rate = rate(Duration::from_secs(3), || {
            derive(&normalized, &admission, Granularity::Day)
                .unwrap()
                .len()
        });
        let rows = derive(&normalized, &admission, Granularity::Day).unwrap();
        let tables = rows
            .iter()
            .map(|row| row.table)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let block_rate = rate(Duration::from_secs(3), || {
            let mut pairs: BTreeMap<(TableId, PartitionId), Vec<Row>> = BTreeMap::new();
            for RoutedRow {
                table,
                partition,
                row,
            } in rows.iter().cloned()
            {
                pairs.entry((table, partition)).or_default().push(row);
            }
            pairs
                .into_iter()
                .map(|(pair, rows)| {
                    pair_block(&batch, pair, rows, marker_at, Compression::Lz4)
                        .unwrap()
                        .rows()
                })
                .sum()
        });
        println!(
            "{lane}: derive {:.0} spans/s, blocks {:.0} spans/s ({} rows in {tables} tables per {SPANS} spans)",
            derive_rate * SPANS as f64,
            block_rate * SPANS as f64,
            rows.len(),
        );
    }
}
