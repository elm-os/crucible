//! Handler tests over the real WAL on crucible-sim's `SimDisk`, in the deterministic runtime with its paused
//! virtual clock: durability ordering, protobuf and JSON equivalence, gzip, partial success, admission,
//! authentication, media types, the request cap, metrics, and a round-trip property over random requests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use common::*;
use crucible_admission::{THROTTLED_MAX_S, THROTTLED_MIN_S, UNAVAILABLE_MAX_S, UNAVAILABLE_MIN_S};
use crucible_env::metrics::registry;
use crucible_ingest::IngestConfig;
use crucible_otlp::{MAX_BODY_BYTES, MAX_SPANS, transcode_json};
use crucible_records::Lsn;
use crucible_sim::{DiskFaults, Host, SIM_EPOCH_MICROS, Sim, SimDisk, run};
use crucible_wal::{Mutant, Wal, WalConfig};
use crucible_wal_codec::{ContentOrigin, Record};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span, Status};
use proptest::collection::vec;
use proptest::prelude::*;
use prost::Message as _;

/// The simulated wall clock at the start of every run, in nanoseconds.
const NOW_NANOS: u64 = SIM_EPOCH_MICROS as u64 * 1_000;

struct Node {
    host: Host,
    wal: Wal,
    router: Router,
}

async fn start(sim: &Sim, disk: &SimDisk, config: &WalConfig, ingest: IngestConfig) -> Node {
    let host = sim.host("node/boot-1", disk, 0);
    let (wal, _report) = Wal::open(&host.env, config.clone()).await.unwrap();
    let router = router(&host.env, &wal, config, ingest);
    Node { host, wal, router }
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

/// What a client saw of one request sent while each sync takes [`SYNC`].
#[derive(Debug)]
struct Observed {
    status: StatusCode,
    /// No answer had arrived halfway through the sync.
    pending_mid_sync: bool,
    /// When the answer arrived, the WAL's durable LSN covered the request's frame.
    durable_when_answered: bool,
    /// Virtual time from sending to the answer.
    waited: Duration,
}

fn observe(mutant: Option<Mutant>) -> Observed {
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
        let request = Req::protobuf(&export(vec![good_span(1, NOW_NANOS)]));
        let mut reply = pin!(send(&node.router, request));
        let mid_sync = tokio::select! {
            biased;
            reply = &mut reply => Some(reply),
            () = clock.sleep(SYNC / 2) => None,
        };
        let pending_mid_sync = mid_sync.is_none();
        if pending_mid_sync {
            assert_eq!(node.wal.durable_lsn(), Lsn::ZERO);
        }
        // The runtime has one thread, so nothing runs between the answer and this read.
        let (reply, durable_when_answered) = match mid_sync {
            Some(reply) => (reply, node.wal.durable_lsn() >= Lsn::new(1)),
            None => {
                let reply = reply.await;
                (reply, node.wal.durable_lsn() >= Lsn::new(1))
            }
        };
        Observed {
            status: reply.status,
            pending_mid_sync,
            durable_when_answered,
            waited: clock.now().saturating_duration_since(started),
        }
    })
}

#[test]
fn the_ack_waits_until_the_frame_is_durable() {
    let observed = observe(None);
    assert_eq!(observed.status, StatusCode::OK);
    assert!(observed.pending_mid_sync, "{observed:?}");
    assert!(observed.durable_when_answered, "{observed:?}");
    assert!(observed.waited >= SYNC, "{observed:?}");
}

#[test]
fn the_durability_test_catches_an_ack_before_the_sync() {
    let observed = observe(Some(Mutant::AckBeforeSync));
    assert_eq!(observed.status, StatusCode::OK);
    assert!(!observed.pending_mid_sync, "{observed:?}");
    assert!(!observed.durable_when_answered, "{observed:?}");
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
                assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
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
