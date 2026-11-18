//! Handler tests over the real WAL on crucible-sim's `SimDisk`, in the deterministic runtime with its paused
//! virtual clock: durability ordering over HTTP and gRPC, protobuf and JSON equivalence, gzip, partial success,
//! admission, authentication with Bearer and Basic, compat routes and routing headers, media types, the
//! request cap, gRPC status codes, metrics, and a round-trip property over random requests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use common::*;
use crucible_admission::{THROTTLED_MAX_S, THROTTLED_MIN_S, UNAVAILABLE_MAX_S, UNAVAILABLE_MIN_S};
use crucible_env::metrics::registry;
use crucible_ingest::{BucketConfig, IngestConfig};
use crucible_otlp::{MAX_BODY_BYTES, MAX_SPANS, transcode_json};
use crucible_records::Lsn;
use crucible_sim::{DiskFaults, Host, SIM_EPOCH_MICROS, Sim, SimDisk, run};
use crucible_wal::{Mutant, Wal, WalConfig};
use crucible_wal_codec::{ContentOrigin, IngestTransport, Record};
use futures::StreamExt as _;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message as _;
use tonic::Code;
use tonic::codec::CompressionEncoding;
use tonic::metadata::MetadataValue;

/// The simulated wall clock at the start of every run, in nanoseconds.
const NOW_NANOS: u64 = SIM_EPOCH_MICROS as u64 * 1_000;

/// The refusal for a key pair whose halves belong to different keys (lead decision U19).
const PAIR_MISMATCH: &str = "the public key does not belong to the same project as the secret key; use the \
                             key pair shown together in Settings";

struct Node {
    host: Host,
    wal: Wal,
    /// The HTTP routes.
    router: Router,
    /// The gRPC routes.
    grpc: Router,
}

async fn start(sim: &Sim, disk: &SimDisk, config: &WalConfig, settings: IngestConfig) -> Node {
    let host = sim.host("node/boot-1", disk, 0);
    let (wal, _report) = Wal::open(&host.env, config.clone()).await.unwrap();
    let service = ingest(&host.env, &wal, config, settings);
    Node {
        host,
        wal,
        router: service.router(),
        grpc: service.grpc_router(),
    }
}

/// Runs `body` against a fresh node on a fault-free disk.
fn with_node<F, Fut>(seed: u64, config: WalConfig, ingest: IngestConfig, body: F)
where
    F: FnOnce(Node, SimDisk) -> Fut,
    Fut: Future<Output = ()>,
{
    run(seed, |sim| async move {
        let disk = sim.disk("disk");
        let node = start(&sim, &disk, &config, ingest).await;
        body(node, disk).await;
    });
}

fn requests(node: &Node, outcome: &str) -> u64 {
    node.host
        .recorder
        .counter_value(&registry::INGEST_REQUESTS, &[outcome])
}

fn inflight(node: &Node) -> Option<i64> {
    node.host
        .recorder
        .gauge_value(&registry::INGEST_INFLIGHT_BYTES, &[])
}

/// The virtual time every sync takes in the durability test.
const SYNC: Duration = Duration::from_millis(200);

/// How a test request travels.
#[derive(Clone, Copy, Debug)]
enum Transport {
    /// `POST /v1/traces`.
    Http,
    /// `TraceService/Export` through the generated client.
    Grpc,
}

/// What a client saw of one request sent while each sync takes [`SYNC`].
#[derive(Debug)]
struct Observed {
    /// The request was acknowledged.
    acked: bool,
    /// No answer had arrived halfway through the sync.
    pending_mid_sync: bool,
    /// When the answer arrived, the WAL's durable LSN covered the request's frame.
    durable_when_answered: bool,
    /// Virtual time from sending to the answer.
    waited: Duration,
}

fn observe(mutant: Option<Mutant>, transport: Transport) -> Observed {
    run(11, |sim| async move {
        let disk = sim.disk("disk");
        let config = WalConfig {
            mutant,
            ..wal_config()
        };
        let node = start(&sim, &disk, &config, IngestConfig::default()).await;
        let micros = SYNC.as_micros() as u64;
        disk.set_faults(DiskFaults {
            sync_latency_micros: (micros, micros),
            ..DiskFaults::default()
        });
        let clock = &node.host.env.clock;
        let started = clock.now();
        let request = export(vec![good_span(1, NOW_NANOS)]);
        let acked = async {
            match transport {
                Transport::Http => {
                    send(&node.router, Req::protobuf(&request)).await.status == StatusCode::OK
                }
                Transport::Grpc => grpc_export(&node.grpc, request.clone()).await.is_ok(),
            }
        };
        let mut acked = pin!(acked);
        let mid_sync = tokio::select! {
            biased;
            acked = &mut acked => Some(acked),
            () = clock.sleep(SYNC / 2) => None,
        };
        let pending_mid_sync = mid_sync.is_none();
        if pending_mid_sync {
            assert_eq!(node.wal.durable_lsn(), Lsn::ZERO);
        }
        // The runtime has one thread, so nothing runs between the answer and this read.
        let (acked, durable_when_answered) = match mid_sync {
            Some(acked) => (acked, node.wal.durable_lsn() >= Lsn::new(1)),
            None => {
                let acked = acked.await;
                (acked, node.wal.durable_lsn() >= Lsn::new(1))
            }
        };
        Observed {
            acked,
            pending_mid_sync,
            durable_when_answered,
            waited: clock.now().saturating_duration_since(started),
        }
    })
}

#[test]
fn the_ack_waits_until_the_frame_is_durable() {
    for transport in [Transport::Http, Transport::Grpc] {
        let observed = observe(None, transport);
        assert!(observed.acked, "{transport:?}: {observed:?}");
        assert!(observed.pending_mid_sync, "{transport:?}: {observed:?}");
        assert!(
            observed.durable_when_answered,
            "{transport:?}: {observed:?}"
        );
        assert!(observed.waited >= SYNC, "{transport:?}: {observed:?}");
    }
}

#[test]
fn the_durability_test_catches_an_ack_before_the_sync() {
    for transport in [Transport::Http, Transport::Grpc] {
        let observed = observe(Some(Mutant::AckBeforeSync), transport);
        assert!(observed.acked, "{transport:?}: {observed:?}");
        assert!(!observed.pending_mid_sync, "{transport:?}: {observed:?}");
        assert!(
            !observed.durable_when_answered,
            "{transport:?}: {observed:?}"
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Two spans with every kind of field the identity test covers, and the same request as OTLP/JSON written
/// with only non-default fields in field-number order, which transcodes to prost's bytes exactly.
fn identity_request() -> (ExportTraceServiceRequest, String) {
    let start = NOW_NANOS - 3_600_000_000_000;
    let mut child = good_span(2, start + 1_000);
    child.parent_span_id = vec![1; 8];
    child.status = Some(Status {
        message: "tool failed".to_owned(),
        code: 2,
    });
    let request = export(vec![good_span(1, start), child]);
    let span = |id: u8, start: u64, extra_ids: &str, extra_tail: &str| {
        format!(
            concat!(
                r#"{{"traceId":"{t}","spanId":"{s}"{extra_ids},"name":"call","kind":3,"#,
                r#""startTimeUnixNano":"{start}","endTimeUnixNano":"{end}","#,
                r#""attributes":[{{"key":"gen_ai.request.model","value":{{"stringValue":"model-a"}}}}]{extra_tail}}}"#
            ),
            t = hex(&[id; 16]),
            s = hex(&[id; 8]),
            extra_ids = extra_ids,
            start = start,
            end = start + 1_500_000,
            extra_tail = extra_tail,
        )
    };
    let parent = format!(r#","parentSpanId":"{}""#, hex(&[1; 8]));
    let json = format!(
        concat!(
            r#"{{"resourceSpans":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"checkout"}}}}]}},"#,
            r#""scopeSpans":[{{"scope":{{"name":"agent","version":"1.0"}},"spans":[{},{}]}}]}}]}}"#
        ),
        span(1, start, "", ""),
        span(
            3,
            start + 1_000,
            &parent,
            r#","status":{"message":"tool failed","code":2}"#
        ),
    );
    (request, json)
}

#[test]
fn protobuf_and_json_bodies_write_the_same_payload() {
    with_node(
        1,
        wal_config(),
        IngestConfig::default(),
        |node, _| async move {
            let (request, json) = identity_request();
            let protobuf = request.encode_to_vec();
            let sent = [
                Req::new(protobuf.clone(), PROTOBUF),
                Req::json(json.clone()),
                Req::new(protobuf.clone(), PROTOBUF).gzip(),
                Req::json(json.clone()).gzip(),
            ];
            for req in sent {
                let reply = send(&node.router, req).await;
                assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
            }
            let payloads = payloads(&node.wal).await;
            assert_eq!(payloads.len(), 4);
            let reference = envelope(&payloads[0]);
            assert_common_envelope(&reference);
            assert_eq!(reference.otlp, protobuf.as_slice());
            assert_eq!(reference.span_count, 2);
            assert_eq!(reference.rejected, b"");
            let origins = [
                ContentOrigin::Protobuf,
                ContentOrigin::Json,
                ContentOrigin::Protobuf,
                ContentOrigin::Json,
            ];
            for (payload, origin) in payloads.iter().zip(origins) {
                let mut written = envelope(payload);
                assert_eq!(written.content_origin, origin);
                // The payloads differ in the content origin and nothing else.
                written.content_origin = ContentOrigin::Protobuf;
                assert_eq!(written, reference);
                assert_eq!(
                    &Record::IngestRequest(written).encode().unwrap(),
                    &payloads[0]
                );
            }
        },
    );
}

#[test]
fn gzip_bodies_are_inflated_and_bad_ones_refused() {
    with_node(
        2,
        wal_config(),
        IngestConfig::default(),
        |node, _| async move {
            let protobuf =
                export(vec![good_span(1, NOW_NANOS), good_span(2, NOW_NANOS)]).encode_to_vec();
            let reply = send(&node.router, Req::new(protobuf.clone(), PROTOBUF).gzip()).await;
            assert_eq!(reply.status, StatusCode::OK);
            // Two gzip members back to back inflate to their concatenation.
            let (head, tail) = protobuf.split_at(protobuf.len() / 2);
            let members = [gzip(head), gzip(tail)].concat();
            let req = Req::new(members, PROTOBUF).with("content-encoding", "x-gzip");
            assert_eq!(send(&node.router, req).await.status, StatusCode::OK);
            let req = Req::new(protobuf.clone(), PROTOBUF).with("content-encoding", "identity");
            assert_eq!(send(&node.router, req).await.status, StatusCode::OK);
            let written = payloads(&node.wal).await;
            assert_eq!(written.len(), 3);
            for payload in &written {
                assert_eq!(envelope(payload).otlp, protobuf.as_slice());
            }

            let bad =
                Req::new(b"not gzip at all".to_vec(), PROTOBUF).with("content-encoding", "gzip");
            let reply = send(&node.router, bad).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST);
            assert!(reply.rpc_status().message.contains("gzip"), "{reply:?}");
            let truncated = gzip(&protobuf);
            let truncated = Req::new(truncated[..truncated.len() - 6].to_vec(), PROTOBUF)
                .with("content-encoding", "gzip");
            assert_eq!(
                send(&node.router, truncated).await.status,
                StatusCode::BAD_REQUEST
            );
            let brotli = Req::new(protobuf.clone(), PROTOBUF).with("content-encoding", "br");
            let reply = send(&node.router, brotli).await;
            assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(reply.rpc_status().code, 3);
            assert_eq!(payloads(&node.wal).await.len(), 3);
            assert_eq!(inflight(&node), Some(0));
        },
    );
}

/// Ten spans across two resources and three scopes; ordinals 1, 3, 4 and 8 are rejected.
fn partial_request() -> ExportTraceServiceRequest {
    let old = NOW_NANOS - 401 * DAY_NANOS;
    let recent = NOW_NANOS - 399 * DAY_NANOS;
    let scope = |spans: Vec<Span>| ScopeSpans {
        scope: Some(InstrumentationScope::default()),
        spans,
        ..ScopeSpans::default()
    };
    let resource = |scopes| ResourceSpans {
        resource: Some(Resource::default()),
        scope_spans: scopes,
        ..ResourceSpans::default()
    };
    ExportTraceServiceRequest {
        resource_spans: vec![
            resource(vec![
                scope(vec![
                    good_span(0, NOW_NANOS),
                    span(vec![0; 16], vec![7; 8], NOW_NANOS, "zero trace id"),
                    good_span(2, recent),
                ]),
                scope(vec![
                    span(vec![7; 16], Vec::new(), NOW_NANOS, "no span id"),
                    span(vec![7; 16], vec![9; 8], old, "too old"),
                ]),
            ]),
            resource(vec![scope(vec![
                good_span(5, NOW_NANOS),
                good_span(6, NOW_NANOS),
                good_span(7, NOW_NANOS),
                span(vec![7; 16], vec![0; 8], NOW_NANOS, "zero span id"),
                good_span(9, NOW_NANOS),
            ])]),
        ],
    }
}

const PARTIAL_MESSAGE: &str = "rejected 4 of 10 spans: 1 with a missing or all-zero trace_id, \
                               2 with a missing or all-zero span_id, 1 older than the accepted event age";

#[test]
fn partial_success_marks_rejected_spans_by_ordinal() {
    with_node(
        3,
        wal_config(),
        IngestConfig::default(),
        |node, _| async move {
            let request = partial_request();
            let reply = send(&node.router, Req::protobuf(&request)).await;
            let partial = reply.export_response().partial_success.unwrap();
            assert_eq!(partial.rejected_spans, 4);
            assert_eq!(partial.error_message, PARTIAL_MESSAGE);

            let reply = send(
                &node.router,
                Req::json(serde_json::to_vec(&request).unwrap()),
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK);
            assert_eq!(
                reply.json(),
                serde_json::json!({
                    "partialSuccess": { "rejectedSpans": "4", "errorMessage": PARTIAL_MESSAGE }
                })
            );

            // A request without rejections: no partial success and an empty bitmap.
            let clean = Req::protobuf(&export(vec![good_span(1, NOW_NANOS)]));
            let reply = send(&node.router, clean.clone()).await;
            assert!(reply.body.is_empty());
            assert_eq!(reply.export_response().partial_success, None);
            let reply = send(
                &node.router,
                clean.with("content-type", JSON).without("content-length"),
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "protobuf bytes are not JSON"
            );
            let json = serde_json::to_vec(&export(vec![good_span(1, NOW_NANOS)])).unwrap();
            let reply = send(&node.router, Req::json(json)).await;
            assert_eq!(reply.json(), serde_json::json!({}));

            let written = payloads(&node.wal).await;
            assert_eq!(written.len(), 4);
            for payload in &written[..2] {
                let written = envelope(payload);
                assert_eq!(written.span_count, 10);
                // Least-significant bit first: ordinals 1, 3 and 4 in byte 0, ordinal 8 in byte 1.
                assert_eq!(written.rejected, [0b0001_1010, 0b0000_0001]);
                assert_eq!(rejected_ordinals(written.rejected), [1, 3, 4, 8]);
                for ordinal in 0..10 {
                    assert_eq!(
                        written.is_rejected(ordinal),
                        [1, 3, 4, 8].contains(&ordinal)
                    );
                }
            }
            for payload in &written[2..] {
                assert_eq!(envelope(payload).rejected, b"");
                assert_eq!(envelope(payload).span_count, 1);
            }
            assert_eq!(requests(&node, "partial"), 2);
            assert_eq!(requests(&node, "accepted"), 2);
            let spans = |outcome| {
                node.host
                    .recorder
                    .counter_value(&registry::INGEST_SPANS, &[outcome])
            };
            assert_eq!(
                [
                    spans("accepted"),
                    spans("missing_trace_id"),
                    spans("missing_span_id"),
                    spans("too_old")
                ],
                [14, 2, 4, 2]
            );
        },
    );
}

#[test]
fn the_event_age_cutoff_follows_the_setting() {
    let config = IngestConfig {
        max_event_age: Duration::from_secs(86_400),
        ..IngestConfig::default()
    };
    with_node(4, wal_config(), config, |node, _| async move {
        let spans = vec![
            good_span(1, NOW_NANOS - DAY_NANOS + 1_000_000),
            good_span(2, NOW_NANOS - DAY_NANOS - 1_000_000),
        ];
        let reply = send(&node.router, Req::protobuf(&export(spans))).await;
        assert_eq!(reply.rejected_spans(), 1);
        let written = payloads(&node.wal).await;
        assert_eq!(rejected_ordinals(envelope(&written[0]).rejected), [1]);
    });
}

#[test]
fn an_exhausted_inflight_budget_answers_429_with_retry_after() {
    let config = IngestConfig {
        inflight_bytes: 64,
        ..IngestConfig::default()
    };
    with_node(5, wal_config(), config, |node, _| async move {
        let body = export(vec![good_span(1, NOW_NANOS)]).encode_to_vec();
        assert!(body.len() > 64);
        let mut waits = Vec::new();
        for _ in 0..20 {
            let reply = send(&node.router, Req::new(body.clone(), PROTOBUF)).await;
            assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(reply.rpc_status().code, 8);
            waits.push(reply.retry_after().unwrap());
        }
        assert!(
            waits
                .iter()
                .all(|s| (THROTTLED_MIN_S..=THROTTLED_MAX_S).contains(s)),
            "{waits:?}"
        );
        waits.sort_unstable();
        waits.dedup();
        assert!(waits.len() > 1, "the jitter spreads retries: {waits:?}");
        // Without `Content-Length` the request is admitted and refused as its body arrives.
        let reply = send_request(&node.router, Req::new(body, PROTOBUF).build_streamed()).await;
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(reply.retry_after().is_some());
        assert_eq!(requests(&node, "rate_limited"), 21);
        assert_eq!(inflight(&node), Some(0));
        assert!(payloads(&node.wal).await.is_empty());
    });
}

#[test]
fn a_full_wal_or_disk_answers_503_with_retry_after() {
    // One segment fills a WAL that may hold one segment: above the 90% high-water mark.
    let full = WalConfig {
        max_bytes: wal_config().segment_bytes,
        ..wal_config()
    };
    let low_disk = WalConfig {
        min_free_bytes: u64::MAX,
        ..wal_config()
    };
    for (seed, config) in [(6, full), (7, low_disk)] {
        with_node(
            seed,
            config,
            IngestConfig::default(),
            |node, _| async move {
                let req = Req::protobuf(&export(vec![good_span(1, NOW_NANOS)]));
                let reply = send(&node.router, req.clone()).await;
                assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(reply.rpc_status().code, 14);
                // No shipper drains the WAL yet, so the wait is the longest.
                assert_eq!(reply.retry_after(), Some(UNAVAILABLE_MAX_S));
                let reply = send(&node.router, req.with("content-type", JSON)).await;
                assert_eq!(reply.json()["code"], 14);
                assert!(payloads(&node.wal).await.is_empty());
                assert_eq!(requests(&node, "unavailable"), 2);
                assert_eq!(inflight(&node), Some(0));
            },
        );
    }
}

#[test]
fn a_failed_group_answers_503_and_a_failed_wal_keeps_answering_it() {
    with_node(
        8,
        wal_config(),
        IngestConfig::default(),
        |node, disk| async move {
            let req = |n| Req::protobuf(&export(vec![good_span(n, NOW_NANOS)]));
            // The group's sync fails; the committer rotates and the client may retry.
            disk.fail_next_syncs(1);
            let reply = send(&node.router, req(1)).await;
            assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(reply.retry_after(), Some(UNAVAILABLE_MIN_S));
            let reply = send(&node.router, req(2)).await;
            assert_eq!(reply.status, StatusCode::OK);
            let written = payloads(&node.wal).await;
            assert_eq!(written.len(), 1);
            assert_eq!(
                envelope(&written[0]).otlp,
                export(vec![good_span(2, NOW_NANOS)]).encode_to_vec()
            );

            // The group's sync fails and so does the rotation: the WAL fails until restart.
            disk.fail_next_syncs(2);
            let reply = send(&node.router, req(3)).await;
            assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(node.wal.admission().failed);
            for _ in 0..2 {
                let reply = send(&node.router, req(4)).await;
                assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(reply.retry_after(), Some(UNAVAILABLE_MAX_S));
            }
            assert_eq!(requests(&node, "unavailable"), 4);
            assert_eq!(inflight(&node), Some(0));
        },
    );
}

#[test]
fn requests_without_a_valid_secret_key_get_401() {
    with_node(
        9,
        wal_config(),
        IngestConfig::default(),
        |node, _| async move {
            let req = Req::protobuf(&export(vec![good_span(1, NOW_NANOS)]));
            let short = format!("Bearer {}", &secret()[..20]);
            let unknown = format!("Bearer {}", unknown_secret());
            let public = format!("Bearer {}", secret().replace("csk_", "cpk_"));
            let basic = "Basic Y3BrX3g6Y3NrX3k=";
            let refused = [
                req.clone().without("authorization"),
                req.clone().with("authorization", &unknown),
                req.clone().with("authorization", &short),
                req.clone().with("authorization", &public),
                req.clone().with("authorization", basic),
                req.clone().with("authorization", "Bearer"),
                req.clone().with("authorization", &secret()),
                // Authentication comes before every other check.
                req.clone()
                    .without("authorization")
                    .with("content-type", "text/plain"),
                req.clone()
                    .without("authorization")
                    .with("content-encoding", "br"),
            ];
            for (i, req) in refused.into_iter().enumerate() {
                let reply = send(&node.router, req).await;
                assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "case {i}");
                assert_eq!(
                    reply.header("www-authenticate"),
                    Some("Bearer, Basic realm=\"crucible\"")
                );
                assert_eq!(reply.rpc_status().code, 16);
            }
            let json = Req::json(b"{}".to_vec()).without("authorization");
            let reply = send(&node.router, json).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
            assert_eq!(reply.json()["code"], 16);
            assert!(payloads(&node.wal).await.is_empty());
            assert_eq!(requests(&node, "unauthenticated"), 10);

            let lower = req.with("authorization", &format!("bearer  {}", secret()));
            assert_eq!(send(&node.router, lower).await.status, StatusCode::OK);
            assert_eq!(payloads(&node.wal).await.len(), 1);
        },
    );
}

/// The SDK identity headers of the invented compat route.
fn with_sdk(req: Req) -> Req {
    req.with(SDK_NAME_HEADER, "sdk-a")
        .with(SDK_VERSION_HEADER, "2.1.0")
}

#[test]
fn compat_routes_take_every_encoding_and_carry_their_hint() {
    with_node(20, wal_config(), compat_config(), |node, _| async move {
        let (request, json) = identity_request();
        let protobuf = request.encode_to_vec();
        let agent = ("OTel-OTLP-Exporter-Rust", "0.31.0");
        let sdk = ("sdk-a", "2.1.0");
        let (pb, js) = (ContentOrigin::Protobuf, ContentOrigin::Json);
        let sent = [
            (
                with_sdk(Req::new(protobuf.clone(), PROTOBUF)).to(COMPAT_PATH),
                COMPAT_DIALECT,
                sdk,
                pb,
            ),
            (
                with_sdk(Req::json(json.clone())).to(COMPAT_PATH),
                COMPAT_DIALECT,
                sdk,
                js,
            ),
            (
                with_sdk(Req::new(protobuf.clone(), PROTOBUF).gzip()).to(COMPAT_PATH),
                COMPAT_DIALECT,
                sdk,
                pb,
            ),
            (
                with_sdk(Req::json(json.clone()).gzip()).to(COMPAT_PATH),
                COMPAT_DIALECT,
                sdk,
                js,
            ),
            // Without the SDK headers the SDK comes from `User-Agent`; with one of them, from the headers.
            (
                Req::new(protobuf.clone(), PROTOBUF).to(COMPAT_PATH),
                COMPAT_DIALECT,
                agent,
                pb,
            ),
            (
                Req::new(protobuf.clone(), PROTOBUF)
                    .to(COMPAT_PATH)
                    .with(SDK_NAME_HEADER, "sdk-a"),
                COMPAT_DIALECT,
                ("sdk-a", ""),
                pb,
            ),
            // A route that declares no SDK headers ignores them.
            (
                Req::new(protobuf.clone(), PROTOBUF).to(PLAIN_COMPAT_PATH),
                PLAIN_COMPAT_DIALECT,
                agent,
                pb,
            ),
            (
                with_sdk(Req::json(json.clone()).gzip()).to(PLAIN_COMPAT_PATH),
                PLAIN_COMPAT_DIALECT,
                agent,
                js,
            ),
        ];
        for (req, ..) in &sent {
            let reply = send(&node.router, req.clone()).await;
            assert_eq!(reply.status, StatusCode::OK, "{req:?}: {reply:?}");
            let empty: &[u8] = if req.headers.iter().any(|(_, v)| v == JSON) {
                b"{}"
            } else {
                b""
            };
            assert_eq!(reply.body.as_ref(), empty, "the response of /v1/traces");
        }
        let written = payloads(&node.wal).await;
        assert_eq!(written.len(), sent.len());
        for (payload, (req, dialect, (name, version), origin)) in written.iter().zip(&sent) {
            let frame = envelope(payload);
            assert_eq!(frame.compat_dialect, *dialect, "{req:?}");
            assert_eq!(
                (frame.sdk_name, frame.sdk_version),
                (*name, *version),
                "{req:?}"
            );
            assert_eq!(frame.content_origin, *origin, "{req:?}");
            assert_eq!(frame.otlp, protobuf.as_slice());
            assert_eq!(frame.transport, IngestTransport::Http);
            assert_eq!((frame.project_id, frame.api_key_id), (PROJECT, KEY_ID));
        }

        // Partial success and refusals answer as on `/v1/traces`.
        let reply = send(
            &node.router,
            Req::protobuf(&partial_request()).to(COMPAT_PATH),
        )
        .await;
        let partial = reply.export_response().partial_success.unwrap();
        assert_eq!(
            (partial.rejected_spans, partial.error_message.as_str()),
            (4, PARTIAL_MESSAGE)
        );
        let reply = send(
            &node.router,
            Req::new(protobuf.clone(), "text/plain").to(PLAIN_COMPAT_PATH),
        )
        .await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(reply.rpc_status().code, 3);
        let declared = Req::new(b"tiny".to_vec(), PROTOBUF)
            .to(COMPAT_PATH)
            .with("content-length", &(MAX_BODY_BYTES + 1).to_string())
            .build_streamed();
        assert_eq!(
            send_request(&node.router, declared).await.status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let reply = send(
            &node.router,
            Req::json(b"{\"resourceSpans\": [".to_vec()).to(COMPAT_PATH),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["code"], 3);

        // An empty export answers 200 in either encoding.
        let empty = [
            Req::new(Vec::new(), PROTOBUF),
            Req::json(b"{}".to_vec()),
            Req::json(br#"{"resourceSpans":[]}"#.to_vec()),
        ];
        for req in empty {
            let reply = send(&node.router, req.to(PLAIN_COMPAT_PATH)).await;
            assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        }
        assert_eq!(requests(&node, "accepted"), 11);
        assert_eq!(requests(&node, "partial"), 1);
        assert_eq!(inflight(&node), Some(0));
    });
}

#[test]
fn routing_headers_set_the_hint_on_v1_traces_only() {
    with_node(21, wal_config(), compat_config(), |node, _| async move {
        let request = export(vec![good_span(1, NOW_NANOS)]);
        let routed = |req: Req, value: &str| req.with(ROUTING_HEADER, value);
        let sent = [
            (
                routed(
                    Req::protobuf(&request),
                    "project_id:0193a1b2-0000-7000-8000-000000000000",
                ),
                ROUTING_DIALECT,
            ),
            (
                routed(Req::json(serde_json::to_vec(&request).unwrap()).gzip(), ""),
                ROUTING_DIALECT,
            ),
            (Req::protobuf(&request), 0),
            // A compat route's own hint wins.
            (
                routed(Req::protobuf(&request), "x").to(PLAIN_COMPAT_PATH),
                PLAIN_COMPAT_DIALECT,
            ),
        ];
        for (req, _) in &sent {
            assert_eq!(send(&node.router, req.clone()).await.status, StatusCode::OK);
        }
        // A gRPC export carries no hint, routing header or not.
        let mut grpc = grpc_request(request.clone(), Some(&format!("Bearer {}", secret())));
        grpc.metadata_mut()
            .insert(ROUTING_HEADER, MetadataValue::from_static("x"));
        grpc_client(&node.grpc).export(grpc).await.unwrap();
        let written = payloads(&node.wal).await;
        let dialects: Vec<u8> = written.iter().map(|p| envelope(p).compat_dialect).collect();
        let mut expected: Vec<u8> = sent.iter().map(|(_, dialect)| *dialect).collect();
        expected.push(0);
        assert_eq!(dialects, expected);
        // The project always comes from the key.
        assert!(written.iter().all(|p| envelope(p).project_id == PROJECT));
    });
}

#[test]
fn basic_auth_takes_a_key_pair_and_says_what_is_wrong() {
    with_node(22, wal_config(), compat_config(), |node, _| async move {
        let req = Req::protobuf(&export(vec![good_span(1, NOW_NANOS)]));
        let bearer = format!("Bearer {}", secret());
        let pair = basic(&public(), &secret());
        let accepted = [
            ("/v1/traces", &pair),
            (COMPAT_PATH, &pair),
            (PLAIN_COMPAT_PATH, &pair),
            (COMPAT_PATH, &bearer),
            (PLAIN_COMPAT_PATH, &bearer),
        ];
        for (path, authorization) in accepted {
            let reply = send(
                &node.router,
                req.clone().to(path).with("authorization", authorization),
            )
            .await;
            assert_eq!(
                reply.status,
                StatusCode::OK,
                "{path} {authorization}: {reply:?}"
            );
        }
        // The pair of the other key works for its own project.
        let other = req
            .clone()
            .with("authorization", &basic(&other_public(), &other_secret()));
        assert_eq!(send(&node.router, other).await.status, StatusCode::OK);
        let written = payloads(&node.wal).await;
        assert_eq!(written.len(), accepted.len() + 1);
        for payload in &written[..accepted.len()] {
            assert_eq!(
                (envelope(payload).project_id, envelope(payload).api_key_id),
                (PROJECT, KEY_ID)
            );
        }
        assert_eq!(envelope(&written[accepted.len()]).project_id, OTHER_PROJECT);

        let refused = [
            // A mismatched pair, either way round.
            (basic(&other_public(), &secret()), PAIR_MISMATCH),
            (basic(&public(), &other_secret()), PAIR_MISMATCH),
            // A public key alone.
            (basic(&public(), ""), "a public key alone"),
            (format!("Bearer {}", public()), "a public key alone"),
            // Empty parts.
            (basic("", &secret()), "username is empty"),
            (basic("", ""), "username is empty"),
            // Malformed credentials.
            ("Basic !!!".to_owned(), "base64"),
            (format!("Basic {}", BASE64.encode(secret())), "base64"),
            (basic(&secret(), &public()), "not a Crucible secret key"),
            (
                basic(&public(), &unknown_secret()),
                "does not accept the secret key",
            ),
            ("Digest realm".to_owned(), "must be `Bearer <secret key>`"),
        ];
        let keys = [
            secret(),
            public(),
            other_secret(),
            other_public(),
            unknown_secret(),
        ];
        for path in ["/v1/traces", COMPAT_PATH] {
            for (authorization, expected) in &refused {
                let reply = send(
                    &node.router,
                    req.clone().to(path).with("authorization", authorization),
                )
                .await;
                assert_eq!(
                    reply.status,
                    StatusCode::UNAUTHORIZED,
                    "{path} {authorization}"
                );
                assert_eq!(
                    reply.header("www-authenticate"),
                    Some("Bearer, Basic realm=\"crucible\"")
                );
                let status = reply.rpc_status();
                assert_eq!(status.code, 16);
                assert!(
                    status.message.contains(expected),
                    "{authorization}: {}",
                    status.message
                );
                let body = String::from_utf8_lossy(&reply.body);
                assert!(keys.iter().all(|key| !body.contains(&key[4..])), "{body}");
            }
        }
        let json =
            Req::json(b"{}".to_vec()).with("authorization", &basic(&other_public(), &secret()));
        let reply = send(&node.router, json).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            reply.json(),
            serde_json::json!({ "code": 16, "message": PAIR_MISMATCH })
        );
        assert_eq!(payloads(&node.wal).await.len(), accepted.len() + 1);
        assert_eq!(
            requests(&node, "unauthenticated"),
            2 * refused.len() as u64 + 1
        );
    });
}

#[test]
fn grpc_exports_round_trip_with_partial_success() {
    with_node(
        23,
        wal_config(),
        IngestConfig::default(),
        |node, _| async move {
            let response = grpc_export(&node.grpc, partial_request()).await.unwrap();
            let partial = response.partial_success.unwrap();
            assert_eq!(
                (partial.rejected_spans, partial.error_message.as_str()),
                (4, PARTIAL_MESSAGE)
            );
            let (request, _) = identity_request();
            let response = grpc_export(&node.grpc, request.clone()).await.unwrap();
            assert_eq!(response.partial_success, None);
            // gzip, with the key pair as Basic.
            let mut client = grpc_client(&node.grpc).send_compressed(CompressionEncoding::Gzip);
            let pair = basic(&public(), &secret());
            let response = client
                .export(grpc_request(request.clone(), Some(&pair)))
                .await
                .unwrap();
            assert_eq!(response.into_inner().partial_success, None);

            let written = payloads(&node.wal).await;
            assert_eq!(written.len(), 3);
            let partial = envelope(&written[0]);
            assert_eq!(
                (partial.span_count, partial.transport),
                (10, IngestTransport::Grpc)
            );
            assert_eq!(rejected_ordinals(partial.rejected), [1, 3, 4, 8]);
            assert_eq!(partial.otlp, partial_request().encode_to_vec().as_slice());
            for payload in &written[1..] {
                let mut frame = envelope(payload);
                assert_eq!(frame.transport, IngestTransport::Grpc);
                assert_eq!(frame.otlp, request.encode_to_vec().as_slice());
                assert_eq!(frame.span_count, 2);
                frame.transport = IngestTransport::Http;
                assert_common_envelope(&frame);
            }
            assert_eq!(
                (requests(&node, "partial"), requests(&node, "accepted")),
                (1, 2)
            );
            assert_eq!(inflight(&node), Some(0));
        },
    );
}

#[test]
fn grpc_refusals_carry_the_contract_codes() {
    let config = WalConfig {
        segment_bytes: 4 << 20,
        ..wal_config()
    };
    with_node(24, config, IngestConfig::default(), |node, _| async move {
        let request = export(vec![good_span(1, NOW_NANOS)]);
        // Unauthenticated, before the body is read, with what to fix.
        let status = grpc_client(&node.grpc)
            .export(grpc_request(request.clone(), None))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        assert!(
            status.message().contains("Authorization: Basic"),
            "{status:?}"
        );
        assert_eq!(retry_after(&status), None);
        let mixed = basic(&other_public(), &secret());
        let status = grpc_client(&node.grpc)
            .export(grpc_request(request.clone(), Some(&mixed)))
            .await
            .unwrap_err();
        assert_eq!(
            (status.code(), status.message()),
            (Code::Unauthenticated, PAIR_MISMATCH)
        );
        // Malformed, and over the cap as sent and once inflated: INVALID_ARGUMENT, never a retryable code.
        let status = grpc_raw(&node.grpc, EXPORT, vec![0xff, 0xff, 0xff], false)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
        for compress in [false, true] {
            let over = vec![0; MAX_BODY_BYTES + 1];
            let status = grpc_raw(&node.grpc, EXPORT, over, compress)
                .await
                .unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
            assert!(status.message().contains("above the cap"), "{status:?}");
        }
        // A message at the cap is accepted, gzip-compressed or not.
        let at_cap = request_of_len(MAX_BODY_BYTES);
        for compress in [true, false] {
            let response = grpc_raw(&node.grpc, EXPORT, at_cap.clone(), compress)
                .await
                .unwrap();
            assert!(response.is_empty());
        }
        // Another method.
        let logs = "/opentelemetry.proto.collector.logs.v1.LogsService/Export";
        let status = grpc_raw(&node.grpc, logs, Vec::new(), false)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unimplemented);
        // OTLP over HTTP sent to the gRPC routes gets 415 and where to send it: a gRPC status on HTTP 200 would
        // read as success to an HTTP exporter.
        for path in [EXPORT, "/v1/traces"] {
            let reply = send(&node.grpc, Req::protobuf(&request).to(path)).await;
            assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{path}");
            assert_eq!(reply.header("grpc-status"), None);
            assert!(String::from_utf8_lossy(&reply.body).contains("POST /v1/traces"));
        }

        let written = payloads(&node.wal).await;
        assert_eq!(written.len(), 2);
        assert!(
            written
                .iter()
                .all(|p| envelope(p).otlp.len() == MAX_BODY_BYTES)
        );
        assert_eq!(requests(&node, "unauthenticated"), 2);
        assert_eq!(requests(&node, "malformed"), 1);
        assert_eq!(requests(&node, "payload_too_large"), 2);
        assert_eq!(requests(&node, "unsupported_media_type"), 1);
        assert_eq!(inflight(&node), Some(0));
    });
}

/// A gRPC body holds the in-flight budget from its first byte: a message that never finishes arriving is
/// refused as soon as the bytes received pass the budget, without waiting for the rest.
#[test]
fn a_grpc_body_is_charged_as_it_arrives() {
    let config = IngestConfig {
        inflight_bytes: 1_000,
        ..IngestConfig::default()
    };
    with_node(30, wal_config(), config, |node, _| async move {
        // The prefix announces a 4,000-byte message; 2,000 bytes arrive and then nothing more.
        let mut first = vec![0, 0, 0, 0x0f, 0xa0];
        first.extend_from_slice(&[0; 2_000]);
        let chunks = futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(first))])
            .chain(futures::stream::pending());
        let request = axum::http::Request::post(EXPORT)
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .header("authorization", format!("Bearer {}", secret()))
            .body(axum::body::Body::from_stream(chunks))
            .unwrap();
        let clock = &node.host.env.clock;
        let answered = tokio::select! {
            biased;
            reply = send_request(&node.grpc, request) => Some(reply),
            () = clock.sleep(Duration::from_secs(60)) => None,
        };
        let reply = answered.expect("the refusal does not wait for the rest of the body");
        assert_eq!(reply.header("grpc-status"), Some("8"), "{reply:?}");
        let status = tonic::Status::from_header_map(&reply.headers).unwrap();
        assert!((1..=5).contains(&retry_after(&status).unwrap()));
        assert_eq!(requests(&node, "rate_limited"), 1);
        assert_eq!(inflight(&node), Some(0));
    });
}

#[test]
fn grpc_backpressure_answers_with_retry_info() {
    let request = export(vec![good_span(1, NOW_NANOS)]);
    // The in-flight budget, which the body exceeds as it arrives, and the project's byte rate.
    let budget = IngestConfig {
        inflight_bytes: 64,
        ..IngestConfig::default()
    };
    let rate = IngestConfig {
        project_rate: Some(BucketConfig {
            rate_per_s: 1,
            burst: 64,
        }),
        ..IngestConfig::default()
    };
    for (seed, config) in [(25, budget), (26, rate)] {
        let request = request.clone();
        with_node(seed, wal_config(), config, |node, _| async move {
            let mut waits = Vec::new();
            for _ in 0..10 {
                let status = grpc_export(&node.grpc, request.clone()).await.unwrap_err();
                assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
                waits.push(retry_after(&status).unwrap());
            }
            let range = i64::from(THROTTLED_MIN_S)..=i64::from(THROTTLED_MAX_S);
            assert!(waits.iter().all(|s| range.contains(s)), "{waits:?}");
            assert_eq!(requests(&node, "rate_limited"), 10);
            assert!(payloads(&node.wal).await.is_empty());
            assert_eq!(inflight(&node), Some(0));
        });
    }
    // A full WAL, a disk below its floor, and a failed group.
    let full = WalConfig {
        max_bytes: wal_config().segment_bytes,
        ..wal_config()
    };
    let low_disk = WalConfig {
        min_free_bytes: u64::MAX,
        ..wal_config()
    };
    for (seed, config) in [(27, full), (28, low_disk)] {
        let request = request.clone();
        with_node(
            seed,
            config,
            IngestConfig::default(),
            |node, _| async move {
                let status = grpc_export(&node.grpc, request).await.unwrap_err();
                assert_eq!(status.code(), Code::Unavailable);
                assert_eq!(retry_after(&status), Some(i64::from(UNAVAILABLE_MAX_S)));
                assert!(payloads(&node.wal).await.is_empty());
            },
        );
    }
    with_node(
        29,
        wal_config(),
        IngestConfig::default(),
        |node, disk| async move {
            disk.fail_next_syncs(1);
            let status = grpc_export(&node.grpc, request.clone()).await.unwrap_err();
            assert_eq!(status.code(), Code::Unavailable);
            assert_eq!(retry_after(&status), Some(i64::from(UNAVAILABLE_MIN_S)));
            assert!(grpc_export(&node.grpc, request).await.is_ok());
            assert_eq!(requests(&node, "unavailable"), 1);
            assert_eq!(inflight(&node), Some(0));
        },
    );
}

#[test]
fn only_otlp_media_types_are_accepted() {
    with_node(
        10,
        wal_config(),
        IngestConfig::default(),
        |node, _| async move {
            let protobuf = export(vec![good_span(1, NOW_NANOS)]).encode_to_vec();
            let json = serde_json::to_vec(&export(vec![good_span(1, NOW_NANOS)])).unwrap();
            for content_type in ["text/plain", "application/protobuf", "application/grpc", ""] {
                let reply = send(&node.router, Req::new(protobuf.clone(), content_type)).await;
                assert_eq!(
                    reply.status,
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "{content_type}"
                );
                assert_eq!(reply.rpc_status().code, 3);
            }
            let missing = Req::new(protobuf.clone(), PROTOBUF).without("content-type");
            assert_eq!(
                send(&node.router, missing).await.status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE
            );
            let accepted = [
                Req::new(protobuf, "Application/X-Protobuf"),
                Req::new(json, "application/json; charset=utf-8"),
            ];
            for req in accepted {
                let reply = send(&node.router, req).await;
                assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
            }
            assert_eq!(payloads(&node.wal).await.len(), 2);
            assert_eq!(requests(&node, "unsupported_media_type"), 5);
        },
    );
}

/// A valid request whose protobuf encoding is exactly `len` bytes: one span with a long attribute.
fn request_of_len(len: usize) -> Vec<u8> {
    let mut filler = len;
    loop {
        let mut span = good_span(1, NOW_NANOS);
        span.attributes = vec![string("filler", &"x".repeat(filler))];
        let bytes = export(vec![span]).encode_to_vec();
        match bytes.len().cmp(&len) {
            std::cmp::Ordering::Equal => return bytes,
            std::cmp::Ordering::Greater => filler -= bytes.len() - len,
            std::cmp::Ordering::Less => filler += len - bytes.len(),
        }
    }
}

#[test]
fn the_canonical_body_cap_holds_for_every_encoding() {
    let config = WalConfig {
        segment_bytes: 4 << 20,
        ..wal_config()
    };
    with_node(12, config, IngestConfig::default(), |node, _| async move {
        // A declared length above the cap is refused before the body is read.
        let declared = Req::new(b"tiny".to_vec(), PROTOBUF)
            .with("content-length", &(MAX_BODY_BYTES + 1).to_string())
            .build_streamed();
        let reply = send_request(&node.router, declared).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(reply.rpc_status().code, 3);
        // A streamed body above the cap is refused as it arrives.
        let over = Req::new(vec![0; MAX_BODY_BYTES + 1], PROTOBUF).build_streamed();
        assert_eq!(
            send_request(&node.router, over).await.status,
            StatusCode::PAYLOAD_TOO_LARGE
        );

        // gzip is inflated under the cap: a body at the cap is accepted, one byte more is refused.
        let at_cap = request_of_len(MAX_BODY_BYTES);
        let reply = send(&node.router, Req::new(at_cap.clone(), PROTOBUF).gzip()).await;
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        let mut over = at_cap;
        over.push(0);
        let reply = send(&node.router, Req::new(over, PROTOBUF).gzip()).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(reply.rpc_status().message.contains("inflated"), "{reply:?}");
        let json = Req::json(vec![b' '; MAX_BODY_BYTES + 1]).gzip();
        assert_eq!(
            send(&node.router, json).await.status,
            StatusCode::PAYLOAD_TOO_LARGE
        );

        // The span cap.
        let spans: Vec<Span> = (0..=MAX_SPANS).map(|_| good_span(1, NOW_NANOS)).collect();
        let reply = send(&node.router, Req::protobuf(&export(spans))).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(reply.rpc_status().message.contains("spans"), "{reply:?}");

        let written = payloads(&node.wal).await;
        assert_eq!(written.len(), 1);
        assert_eq!(envelope(&written[0]).otlp.len(), MAX_BODY_BYTES);
        assert_eq!(requests(&node, "payload_too_large"), 5);
        assert_eq!(inflight(&node), Some(0));
    });
}

#[test]
fn malformed_bodies_get_400() {
    with_node(
        13,
        wal_config(),
        IngestConfig::default(),
        |node, _| async move {
            let bad_id = export(vec![span(
                vec![1; 15],
                vec![1; 8],
                NOW_NANOS,
                "short trace id",
            )]);
            let refused = [
                Req::new(vec![0xff, 0xff, 0xff], PROTOBUF),
                Req::protobuf(&bad_id),
                Req::json(b"{\"resourceSpans\": [".to_vec()),
                Req::json(
                    b"{\"resourceSpans\":[{\"scopeSpans\":[{\"spans\":[{\"traceId\":\"xyz\"}]}]}]}"
                        .to_vec(),
                ),
            ];
            for (i, req) in refused.into_iter().enumerate() {
                let reply = send(&node.router, req).await;
                assert_eq!(reply.status, StatusCode::BAD_REQUEST, "case {i}: {reply:?}");
            }
            assert!(payloads(&node.wal).await.is_empty());
            assert_eq!(requests(&node, "malformed"), 4);
            assert_eq!(inflight(&node), Some(0));
        },
    );
}

fn id(len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => Just(Vec::new()),
        1 => Just(vec![0; len]),
        6 => vec(any::<u8>(), len),
    ]
}

/// Start times well inside or well outside the 400-day age cap, never near its boundary.
fn start_nanos() -> impl Strategy<Value = u64> {
    prop_oneof![
        3 => (0..30 * DAY_NANOS).prop_map(|ago| NOW_NANOS - ago),
        1 => (401 * DAY_NANOS..800 * DAY_NANOS).prop_map(|ago| NOW_NANOS - ago),
    ]
}

fn arb_span() -> impl Strategy<Value = Span> {
    let attributes = vec(("[a-z.]{1,8}", "\\PC{0,8}"), 0..3);
    (
        id(16),
        id(8),
        "[a-z ]{0,12}",
        0..6i32,
        start_nanos(),
        attributes,
    )
        .prop_map(|(trace_id, span_id, name, kind, start, attributes)| Span {
            trace_id,
            span_id,
            name,
            kind,
            start_time_unix_nano: start,
            end_time_unix_nano: start + 1_000,
            attributes: attributes.iter().map(|(k, v)| string(k, v)).collect(),
            ..Span::default()
        })
}

fn arb_request() -> impl Strategy<Value = ExportTraceServiceRequest> {
    vec(vec(vec(arb_span(), 0..5), 1..3), 1..3).prop_map(|resources| ExportTraceServiceRequest {
        resource_spans: resources
            .into_iter()
            .map(|scopes| ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![string("service.name", "svc")],
                    ..Resource::default()
                }),
                scope_spans: scopes
                    .into_iter()
                    .map(|spans| ScopeSpans {
                        scope: Some(InstrumentationScope {
                            name: "scope".to_owned(),
                            ..InstrumentationScope::default()
                        }),
                        spans,
                        ..ScopeSpans::default()
                    })
                    .collect(),
                ..ResourceSpans::default()
            })
            .collect(),
    })
}

/// The ordinals that partial success must reject, computed from the request's own structs.
fn expected_rejections(request: &ExportTraceServiceRequest) -> Vec<u32> {
    let min_start_micros = SIM_EPOCH_MICROS - 400 * 86_400_000_000;
    let missing = |id: &[u8]| id.iter().all(|b| *b == 0);
    request
        .resource_spans
        .iter()
        .flat_map(|r| &r.scope_spans)
        .flat_map(|s| &s.spans)
        .enumerate()
        .filter(|(_, span)| {
            missing(&span.trace_id)
                || missing(&span.span_id)
                || ((span.start_time_unix_nano / 1_000) as i64) < min_start_micros
        })
        .map(|(ordinal, _)| ordinal as u32)
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Every accepted request leaves exactly one frame, whose payload decodes, and encodes back, to the
    /// canonical body and the envelope the request calls for: the credential's project and key, the body's
    /// origin, the SDK of `User-Agent`, the span count and the rejected ordinals, which the response counts.
    #[test]
    fn accepted_frames_decode_to_the_request_and_its_envelope(
        request in arb_request(),
        json: bool,
        compress: bool,
        agent in proptest::option::of("[A-Za-z-]{1,140}/[0-9.]{1,70}"),
        seed: u64,
    ) {
        let protobuf = request.encode_to_vec();
        let (body, content_type) = if json {
            (serde_json::to_vec(&request).unwrap(), JSON)
        } else {
            (protobuf.clone(), PROTOBUF)
        };
        let canonical = if json { transcode_json(&body).unwrap() } else { protobuf };
        let rejected = expected_rejections(&request);
        let mut req = Req::new(body, content_type);
        req = match &agent {
            Some(agent) => req.with("user-agent", agent),
            None => req.without("user-agent"),
        };
        if compress {
            req = req.gzip();
        }
        let request_spans: usize = request
            .resource_spans
            .iter()
            .flat_map(|r| &r.scope_spans)
            .map(|s| s.spans.len())
            .sum();

        with_node(seed, wal_config(), IngestConfig::default(), |node, _| async move {
            let reply = send(&node.router, req).await;
            assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
            let counted = if json {
                let value = reply.json();
                value["partialSuccess"]["rejectedSpans"]
                    .as_str()
                    .map_or(0, |n| n.parse::<usize>().unwrap())
            } else {
                reply.rejected_spans() as usize
            };
            assert_eq!(counted, rejected.len());

            let written = payloads(&node.wal).await;
            assert_eq!(written.len(), 1);
            let frame = envelope(&written[0]);
            assert_eq!(frame.otlp, canonical.as_slice());
            assert_eq!(ExportTraceServiceRequest::decode(frame.otlp).unwrap(), request);
            assert_eq!(frame.span_count as usize, request_spans);
            assert_eq!(rejected_ordinals(frame.rejected), rejected);
            assert_eq!(frame.rejected.last().map(|b| *b != 0), rejected.last().map(|_| true));
            assert_eq!(frame.project_id, PROJECT);
            assert_eq!(frame.api_key_id, KEY_ID);
            assert_eq!(frame.compat_dialect, 0);
            assert!(!frame.admin_import);
            assert_eq!(frame.idempotency_key, b"");
            let origin = if json { ContentOrigin::Json } else { ContentOrigin::Protobuf };
            assert_eq!(frame.content_origin, origin);
            let (name, version) = agent
                .as_deref()
                .map_or(("", ""), |a| a.split_once('/').unwrap());
            assert_eq!(frame.sdk_name, &name[..name.len().min(128)]);
            assert_eq!(frame.sdk_version, &version[..version.len().min(64)]);
            assert_eq!(inflight(&node), Some(0));
        });
    }
}
