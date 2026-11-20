//! What preparing a request's WAL frame costs by body size and encoding, against what the hop to a blocking thread
//! and back costs, as an ignored timing test. It chose `INLINE_PREPARE_MAX_BYTES` in `src/service.rs`. Run it with
//! `cargo test --release -p crucible-ingest --bench prepare -- --ignored --nocapture`.
// Timing reads the clock directly, which product code may only do through `Env`; benches may.
#![allow(
    clippy::disallowed_methods,
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::expect_used
)]

use std::borrow::Cow;
use std::hint::black_box;
use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

use crucible_env_prod::{ProdConfig, build};
use crucible_otlp::{TraceRequest, transcode_json};
use crucible_records::{ApiKeyId, ProjectId, Timestamp};
use crucible_wal_codec::{
    Compression, ContentOrigin, IngestRequest, IngestTransport, PreparedFrame, Record, RecordType,
};
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use prost::Message as _;

/// The load generator's word list, so that message text is words rather than random bytes. These bodies compress
/// more than the generator's (the table's wire bytes); the gzip rows show the cost of the hop's inflate, not the
/// threshold, since a gzip body always takes the hop.
const WORDS: [&str; 32] = [
    "agent", "tool", "model", "prompt", "token", "query", "result", "context", "server", "client",
    "search", "answer", "the", "a", "of", "to", "and", "with", "from", "request", "response",
    "document", "summary", "plan", "step", "memory", "user", "system", "message", "function",
    "value", "error",
];

/// One step of splitmix64: the deterministic choice of words.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn string(key: &str, value: String) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value)),
        }),
        key_strindex: 0,
    }
}

/// A span of an agent run, padded with message text to about `size` encoded bytes.
fn span(i: u64, size: usize) -> Span {
    let mut span = Span {
        trace_id: (i / 16 + 1).to_be_bytes().repeat(2),
        span_id: (i + 1).to_be_bytes().to_vec(),
        name: "chat model".to_owned(),
        kind: 3,
        start_time_unix_nano: 1_794_907_800_000_000_000 + i * 1_000,
        end_time_unix_nano: 1_794_907_800_000_500_000 + i * 1_000,
        attributes: vec![
            string("gen_ai.operation.name", "chat".to_owned()),
            string("gen_ai.request.model", "model-large".to_owned()),
            string("session.id", format!("session-{}", i % 97)),
        ],
        status: Some(Status {
            message: String::new(),
            code: 1,
        }),
        ..Span::default()
    };
    let mut text = String::new();
    let mut state = i;
    while text.len() + span.encoded_len() + 32 < size {
        text.push_str(WORDS[(next(&mut state) % WORDS.len() as u64) as usize]);
        text.push(' ');
    }
    span.attributes.push(string("gen_ai.input.messages", text));
    span
}

/// A request of about `bytes` protobuf bytes: spans of about 1 KiB, or one smaller span.
fn request(bytes: usize) -> ExportTraceServiceRequest {
    let spans = (bytes / 1_024).max(1) as u64;
    let size = bytes / spans as usize;
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![string("service.name", "agent".to_owned())],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: "agent.instrumentation".to_owned(),
                    ..InstrumentationScope::default()
                }),
                spans: (0..spans).map(|i| span(i, size)).collect(),
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

/// The work of `Inner::prepare` on a body as received: inflate a gzip body, transcode a JSON one, scan, then build
/// the `IngestRequest` payload and its LZ4 frame with the payload checksum.
fn prepare(wire: &[u8], gzip: bool, json: bool) -> PreparedFrame {
    let decoded: Cow<'_, [u8]> = if gzip {
        let mut plain = Vec::new();
        MultiGzDecoder::new(wire).read_to_end(&mut plain).unwrap();
        Cow::Owned(plain)
    } else {
        Cow::Borrowed(wire)
    };
    let canonical = if json {
        Cow::Owned(transcode_json(&decoded).unwrap())
    } else {
        decoded
    };
    let request = TraceRequest::scan(&canonical, Some(Timestamp::from_micros(0))).unwrap();
    let envelope = IngestRequest {
        project_id: ProjectId::from_bytes([7; 16]),
        api_key_id: ApiKeyId::from_bytes([9; 16]),
        compat_dialect: 0,
        transport: IngestTransport::Http,
        content_origin: if json {
            ContentOrigin::Json
        } else {
            ContentOrigin::Protobuf
        },
        admin_import: false,
        sdk_name: "opentelemetry",
        sdk_version: "1.0.0",
        idempotency_key: &[],
        span_count: request.span_count(),
        rejected: &[],
        otlp: &canonical,
    };
    let payload = Record::IngestRequest(envelope).encode().unwrap();
    PreparedFrame::new(RecordType::IngestRequest, payload, Compression::Lz4).unwrap()
}

/// The median and the 99th percentile of `samples`, in microseconds.
fn percentiles(mut samples: Vec<Duration>) -> (f64, f64) {
    samples.sort_unstable();
    let at = |q: f64| {
        let index = ((samples.len() as f64 * q).ceil() as usize).clamp(1, samples.len()) - 1;
        samples[index].as_secs_f64() * 1e6
    };
    (at(0.5), at(0.99))
}

/// Times `f` once per call, for at least 300 ms and 300 calls after a warm-up.
fn time(mut f: impl FnMut()) -> (f64, f64) {
    for _ in 0..50 {
        f();
    }
    let started = Instant::now();
    let mut samples = Vec::new();
    while samples.len() < 300 || started.elapsed() < Duration::from_millis(300) {
        let at = Instant::now();
        f();
        samples.push(at.elapsed());
    }
    percentiles(samples)
}

#[test]
#[ignore = "a timing measurement; run it with --release"]
fn prepare_cost_against_the_blocking_hop() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let prod = build(runtime.handle().clone(), &ProdConfig::default()).unwrap();
    let env = prod.env.clone();
    let hop = runtime.block_on(async move {
        for _ in 0..1_000 {
            env.spawn_blocking("hop", || ()).join().await.unwrap();
        }
        let mut samples = Vec::new();
        for _ in 0..20_000 {
            let at = Instant::now();
            env.spawn_blocking("hop", || ()).join().await.unwrap();
            samples.push(at.elapsed());
        }
        percentiles(samples)
    });
    println!(
        "hop to a blocking thread and back (Env::spawn_blocking, 2 workers): p50 {:.1} us, p99 {:.1} us",
        hop.0, hop.1
    );
    println!("| body | protobuf bytes | wire bytes | prepare p50 us | prepare p99 us |");
    println!("|---|---|---|---|---|");
    for kib in [1, 2, 4, 8, 16, 32, 64, 128] {
        let request = request(kib << 10);
        let protobuf = request.encode_to_vec();
        let json = serde_json::to_vec(&request).unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&protobuf).unwrap();
        let gzip = encoder.finish().unwrap();
        for (name, wire, gzipped, is_json) in [
            ("protobuf", &protobuf, false, false),
            ("json", &json, false, true),
            ("protobuf, gzip", &gzip, true, false),
        ] {
            let (p50, p99) = time(|| {
                black_box(prepare(black_box(wire), gzipped, is_json));
            });
            println!(
                "| {name} | {} | {} | {p50:.1} | {p99:.1} |",
                protobuf.len(),
                wire.len()
            );
        }
    }
    drop(prod);
}
