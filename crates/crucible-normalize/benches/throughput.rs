//! Stage A throughput on one thread, as an ignored timing test. Run it with
//! `cargo test --release -p crucible-normalize --bench throughput -- --ignored --nocapture`.
// Timing reads the clock directly, which product code may only do through `Env`; benches may.
#![allow(clippy::disallowed_methods)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use crucible_normalize::{IngestFrame, NormCtx, NormalizedFrame, PriceTable, StageB, normalize};
use crucible_records::{ApiKeyId, Lsn, NanoUsd, PayloadCaps, ProjectId, Timestamp, WalEpoch};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::Event;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message;

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

/// A generation span of an agent run: mapped GenAI keys, residual keys, an event, and an input body that brings
/// the encoded span to `size` bytes. Sixteen spans share a trace.
fn span(i: u64, size: usize) -> Span {
    let mut span = Span {
        trace_id: (i / 16 + 1).to_be_bytes().repeat(2),
        span_id: (i + 1).to_be_bytes().to_vec(),
        parent_span_id: if i.is_multiple_of(16) {
            Vec::new()
        } else {
            (i / 16 * 16 + 1).to_be_bytes().to_vec()
        },
        name: "chat model-large".to_owned(),
        kind: 3,
        start_time_unix_nano: 1_794_907_800_000_000_000 + i * 1_000,
        end_time_unix_nano: 1_794_907_800_000_500_000 + i * 1_000,
        attributes: vec![
            string("gen_ai.operation.name", "chat".to_owned()),
            string("gen_ai.request.model", "model-large-2026-11".to_owned()),
            string("gen_ai.response.model", "model-large-2026-11".to_owned()),
            string("gen_ai.provider.name", "provider".to_owned()),
            int("gen_ai.usage.input_tokens", 1_234),
            int("gen_ai.usage.output_tokens", 567),
            string("session.id", format!("session-{}", i % 97)),
            string("user.id", format!("user-{}", i % 13)),
            string(
                "gen_ai.output.messages",
                r#"[{"role":"assistant"}]"#.to_owned(),
            ),
            int("retry.count", 0),
        ],
        events: vec![Event {
            time_unix_nano: 1_794_907_800_000_100_000 + i * 1_000,
            name: "gen_ai.content.completion".to_owned(),
            attributes: vec![string("event.kind", "first_token".to_owned())],
            dropped_attributes_count: 0,
        }],
        status: Some(Status {
            message: String::new(),
            code: 1,
        }),
        ..Span::default()
    };
    let pad = size.saturating_sub(span.encoded_len() + 32);
    span.attributes
        .push(string("gen_ai.input.messages", "x".repeat(pad)));
    span
}

fn request(spans: u64, size: usize) -> Vec<u8> {
    let resource = Resource {
        attributes: vec![
            string("service.name", "agent".to_owned()),
            string("service.version", "1.4.2".to_owned()),
            string("telemetry.sdk.language", "python".to_owned()),
        ],
        ..Resource::default()
    };
    let scope = InstrumentationScope {
        name: "agent.instrumentation".to_owned(),
        version: "0.9.0".to_owned(),
        ..InstrumentationScope::default()
    };
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(resource),
            scope_spans: vec![ScopeSpans {
                scope: Some(scope),
                spans: (0..spans).map(|i| span(i, size)).collect(),
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
    .encode_to_vec()
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
#[ignore = "timing test; run in release with --ignored --nocapture"]
fn stage_a_throughput() {
    for (label, spans, size) in [("1 KB", 10_000, 1_024), ("10 KB", 2_000, 10 * 1_024)] {
        let body = request(spans, size);
        let frame = IngestFrame {
            wal_epoch: WalEpoch::from_bytes([1; 16]),
            lsn: Lsn::new(1),
            received_at: Timestamp::from_micros(1_794_907_801_000_000),
            project_id: ProjectId::from_bytes([2; 16]),
            api_key_id: ApiKeyId::from_bytes([3; 16]),
            compat_dialect: 0,
            sdk_name: "sdk",
            sdk_version: "1.0.0",
            rejected: &[],
            otlp: &body,
        };
        let ctx = NormCtx::new(PriceTable::bundled().unwrap(), PayloadCaps::DEFAULT);
        let out = normalize(&frame, &ctx).unwrap();
        assert_eq!(out.spans.len() as u64, spans);
        let mean = body.len() as f64 / spans as f64;
        let budget = Duration::from_secs(3);
        let per_s = rate(budget, || {
            normalize(black_box(&frame), &ctx).map_or(0, |f| f.spans.len())
        });
        let mb = body.len() as f64 / 1e6;
        println!(
            "{label} spans (mean {mean:.0} B, {spans} per {mb:.1} MB frame): stage A on one thread: \
             {:.0} spans/s, {:.0} MB/s",
            per_s * spans as f64,
            per_s * mb
        );
    }
}

/// Stage B on one thread over the frames of [`stage_a_throughput`], where every span carries usage and 15 of 16
/// name a parent in the frame: the cost of one admission into fresh state, and what the usage descendant set
/// adds to it. The second figure is the same frame with its usage removed, so that no span enters the set; the
/// copy of the frame that each iteration needs is timed alone and subtracted.
#[test]
#[ignore = "timing test; run in release with --ignored --nocapture"]
fn stage_b_usage_descendant_set() {
    for (label, spans, size) in [("1 KB", 10_000, 1_024), ("10 KB", 2_000, 10 * 1_024)] {
        let body = request(spans, size);
        let frame = IngestFrame {
            wal_epoch: WalEpoch::from_bytes([1; 16]),
            lsn: Lsn::new(1),
            received_at: Timestamp::from_micros(1_794_907_801_000_000),
            project_id: ProjectId::from_bytes([2; 16]),
            api_key_id: ApiKeyId::from_bytes([3; 16]),
            compat_dialect: 0,
            sdk_name: "sdk",
            sdk_version: "1.0.0",
            rejected: &[],
            otlp: &body,
        };
        let ctx = NormCtx::new(PriceTable::bundled().unwrap(), PayloadCaps::DEFAULT);
        let with_usage = normalize(&frame, &ctx).unwrap();
        let mut without_usage = with_usage.clone();
        for span in &mut without_usage.spans {
            span.core.usage.clear();
            span.core.total_cost_nusd = NanoUsd::ZERO;
        }
        let budget = Duration::from_secs(3);
        let admit = |frame: &NormalizedFrame| {
            let mut copy = frame.clone();
            StageB::default().admit(&mut copy).unwrap();
            copy.spans.len()
        };
        // Nanoseconds per span of one iteration.
        let ns = |per_s: f64| 1e9 / (per_s * spans as f64);
        let copy = ns(rate(budget, || black_box(&with_usage).clone().spans.len()));
        let marked = ns(rate(budget, || admit(black_box(&with_usage)))) - copy;
        let copy_plain = ns(rate(budget, || black_box(&without_usage).clone().spans.len()));
        let plain = ns(rate(budget, || admit(black_box(&without_usage)))) - copy_plain;
        let flagged = {
            let mut copy = with_usage.clone();
            StageB::default().admit(&mut copy).unwrap();
            copy.spans.iter().filter(|s| s.core.usage_aggregate).count()
        };
        println!(
            "{label} spans ({spans} per frame, {flagged} flagged): stage B on one thread: {marked:.0} ns/span with the usage descendant set at work, {plain:.0} ns/span without; the set costs {:.0} ns/span",
            marked - plain
        );
    }
}
