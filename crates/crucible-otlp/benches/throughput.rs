//! Scan throughput, as an ignored timing test for the 250k spans/s per node budget. Run it with
//! `cargo test --release -p crucible-otlp --bench throughput -- --ignored --nocapture`.
// Timing reads the clock directly, which product code may only do through `Env`; benches may.
#![allow(clippy::disallowed_methods)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use crucible_otlp::{AnyValueView, Attributes, TraceRequest};
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

/// A span of an agent run: ten small attributes, an event, and one large text attribute that brings
/// the encoded span to `size` bytes.
fn span(i: u64, size: usize) -> Span {
    let mut span = Span {
        trace_id: (i / 16 + 1).to_be_bytes().repeat(2),
        span_id: (i + 1).to_be_bytes().to_vec(),
        parent_span_id: (i + 2).to_be_bytes().to_vec(),
        name: "chat gpt-model".to_owned(),
        kind: 3,
        start_time_unix_nano: 1_794_907_800_000_000_000 + i * 1_000,
        end_time_unix_nano: 1_794_907_800_000_500_000 + i * 1_000,
        attributes: vec![
            string("gen_ai.operation.name", "chat".to_owned()),
            string("gen_ai.request.model", "model-large-2026-11".to_owned()),
            string("gen_ai.response.model", "model-large-2026-11".to_owned()),
            string("gen_ai.system", "provider".to_owned()),
            int("gen_ai.usage.input_tokens", 1_234),
            int("gen_ai.usage.output_tokens", 567),
            string("session.id", format!("session-{}", i % 97)),
            string("user.id", format!("user-{}", i % 13)),
            string("gen_ai.response.finish_reasons", "stop".to_owned()),
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

fn touch(attributes: Attributes<'_>) -> usize {
    attributes
        .map(|(key, value)| {
            key.len()
                + match value {
                    AnyValueView::String(s) => s.len(),
                    AnyValueView::Bytes(b) => b.len(),
                    AnyValueView::Int(i) => i as usize & 1,
                    _ => 1,
                }
        })
        .sum()
}

/// Reads every field a normalizer reads.
fn walk(request: &TraceRequest<'_>) -> usize {
    request
        .spans()
        .map(|span| {
            span.name().len()
                + span.trace_id().to_bytes()[0] as usize
                + span.start_time().micros() as usize % 7
                + span.status().message().len()
                + touch(span.attributes())
                + span
                    .events()
                    .map(|e| e.name().len() + touch(e.attributes()))
                    .sum::<usize>()
                + touch(span.resource().attributes())
                + span.scope().name().len()
        })
        .sum()
}

/// Runs `f` for at least `budget` and returns iterations per second.
fn rate(budget: Duration, mut f: impl FnMut() -> usize) -> f64 {
    for _ in 0..3 {
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
fn scan_throughput() {
    for (label, spans, size) in [("1 KB", 20_000, 1_024), ("10 KB", 4_000, 10 * 1_024)] {
        let body = request(spans, size);
        let scanned = TraceRequest::scan(&body, None).unwrap();
        assert_eq!(u64::from(scanned.span_count()), spans);
        assert!(scanned.partial_success().is_none());
        let mean = body.len() as f64 / spans as f64;
        let budget = Duration::from_secs(3);
        let scan = rate(budget, || {
            TraceRequest::scan(black_box(&body), None)
                .map(|r| r.span_count() as usize)
                .unwrap_or(0)
        });
        let full = rate(budget, || {
            TraceRequest::scan(black_box(&body), None).map_or(0, |r| walk(&r))
        });
        let mb = body.len() as f64 / 1e6;
        for (what, per_s) in [("scan", scan), ("scan + walk", full)] {
            println!(
                "{label} spans (mean {mean:.0} B, {spans} per {mb:.1} MB body): {what}: \
                 {:.0} spans/s, {:.0} MB/s",
                per_s * spans as f64,
                per_s * mb
            );
        }
    }
}
