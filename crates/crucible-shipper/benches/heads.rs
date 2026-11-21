//! Row derivation for a frame whose spans start many traces, as an ignored timing test. Run it with
//! `cargo test --release -p crucible-shipper --bench heads -- --ignored --nocapture`.
//!
//! It derives the rows of two frames of 50,000 spans, the most one request holds: one of 50,000 rootless traces,
//! a span each, so that every span heads its trace provisionally, and one of a single rootless trace, which has one
//! provisional head. The many-trace frame derives one more row per span (its `trace_heads` row); finding the heads
//! must cost no more than a pass over the spans, so the two frames take less than twice as long as each other.
// Timing reads the clock directly, which product code may only do through `Env`; benches may.
#![allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::print_stdout)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use crucible_normalize::{
    Admission, IdentityCtx, IngestFrame, NormCtx, NormalizedFrame, PriceTable, StageB, normalize,
};
use crucible_records::{ApiKeyId, Lsn, PayloadCaps, ProjectId, Timestamp, WalEpoch};
use crucible_schema::Granularity;
use crucible_shipper::tables::derive;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message;

/// The spans of each frame: the most one request holds (contract 4.3).
const SPANS: u64 = 50_000;
/// Timed derivations of each frame, alternating between the two.
const ROUNDS: usize = 7;

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

/// Span `i` of a frame: a generation whose parent no span of the frame carries when `traces` is true, each in a
/// trace of its own; otherwise the spans make one trace whose first span's parent is missing.
fn span(i: u64, traces: bool) -> Span {
    let missing = (SPANS + 1 + i).to_be_bytes().to_vec();
    let (trace_id, parent_span_id) = if traces {
        ((i + 1).to_be_bytes().repeat(2), missing)
    } else if i == 0 {
        ([7u8; 16].to_vec(), missing)
    } else {
        ([7u8; 16].to_vec(), 1u64.to_be_bytes().to_vec())
    };
    Span {
        trace_id,
        span_id: (i + 1).to_be_bytes().to_vec(),
        parent_span_id,
        name: "chat".to_owned(),
        kind: 3,
        start_time_unix_nano: 1_794_907_800_000_000_000 + i * 1_000_000,
        end_time_unix_nano: 1_794_907_800_000_500_000 + i * 1_000_000,
        attributes: vec![
            string("gen_ai.operation.name", "chat".to_owned()),
            string("gen_ai.request.model", "model-large-2026-11".to_owned()),
            int("gen_ai.usage.input_tokens", 1_234),
            int("gen_ai.usage.output_tokens", 567),
        ],
        ..Span::default()
    }
}

/// One frame of [`SPANS`] spans through stage A and stage B.
fn frame(traces: bool) -> (NormalizedFrame, Admission) {
    let body = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string("service.name", "agent".to_owned())],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: (0..SPANS).map(|i| span(i, traces)).collect(),
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

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

#[test]
#[ignore = "a timing run; use --release"]
fn many_rootless_traces_derive_about_as_fast_as_one() {
    let many = frame(true);
    let one = frame(false);
    assert_eq!(many.0.spans.len() as u64, SPANS);
    assert_eq!(one.0.spans.len() as u64, SPANS);
    assert_eq!(many.1.provisional_heads.len() as u64, SPANS);
    assert_eq!(one.1.provisional_heads, vec![0]);
    let time = |(frame, admission): &(NormalizedFrame, Admission)| {
        let start = Instant::now();
        let rows = derive(frame, admission, Granularity::Day).unwrap();
        let took = start.elapsed();
        black_box(rows);
        took
    };
    // One untimed derivation of each, then the rounds, alternating so that drift reaches both alike.
    time(&many);
    time(&one);
    let (mut many_times, mut one_times) = (Vec::new(), Vec::new());
    for _ in 0..ROUNDS {
        many_times.push(time(&many));
        one_times.push(time(&one));
    }
    let (many_median, one_median) = (median(many_times), median(one_times));
    let ratio = many_median.as_secs_f64() / one_median.as_secs_f64();
    println!(
        "derive {SPANS} spans, median of {ROUNDS}: {SPANS} rootless traces {:.1} ms, one rootless trace {:.1} ms, \
         ratio {ratio:.2}",
        many_median.as_secs_f64() * 1e3,
        one_median.as_secs_f64() * 1e3,
    );
    assert!(
        ratio < 2.0,
        "many rootless traces take {ratio:.2} times as long as one"
    );
}
