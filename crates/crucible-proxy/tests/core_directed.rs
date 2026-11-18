//! Directed cases of `ProxyCore`: how HTTP exchanges end, session scope, and the export buffer and backoff.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{ChaChaRng, Instant};
use crucible_proxy::{
    BodyKind, CoreConfig, EndReason, ExportResult, HeaderView, Now, Op, ProxyCore, StreamId, Tap,
};
use crucible_records::{
    CancelSource, CaptureSource, Direction, ErrorClass, McpStatus, ResultType, Timestamp, Transport,
};

const C2S: Direction = Direction::ClientToServer;
const S2C: Direction = Direction::ServerToClient;

fn at(millis: u64) -> Now {
    Now {
        mono: Instant::from_nanos(millis * 1_000_000),
        wall: Timestamp::from_micros(1_800_000_000_000_000 + millis as i64 * 1_000),
    }
}

fn http_core(config: impl FnOnce(&mut CoreConfig)) -> (ProxyCore, Tap) {
    let mut settings = CoreConfig::new(CaptureSource::ProxyHttp);
    config(&mut settings);
    let core = ProxyCore::new(settings, Arc::new(ChaChaRng::from_u64(1)));
    let tap = core.tap();
    (core, tap)
}

fn view(session: Option<&str>) -> HeaderView {
    HeaderView {
        user_agent: "client/1".to_owned(),
        session_key: session.map(str::to_owned),
        ip_prefix: "10.1.2.0/24".to_owned(),
        body_readable: true,
        ..HeaderView::default()
    }
}

/// Everything finished so far, through a healthy export.
fn ops(core: &mut ProxyCore) -> Vec<Op> {
    core.drain();
    let far = Instant::from_nanos(u64::MAX / 2);
    let mut all = Vec::new();
    while let Some(batch) = core.next_export(far, true) {
        all.extend(batch.ops.iter().cloned());
        core.export_done(batch, ExportResult::Delivered, far);
    }
    all
}

fn request(tap: &Tap, stream: u64, id: u64, when: u64) {
    let body = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"echo\"}}}}"
    );
    tap.forward(StreamId(stream), C2S, Bytes::from(body), at(when));
}

#[test]
fn an_http_status_without_a_json_rpc_answer_classifies_the_operation() {
    let (mut core, tap) = http_core(|_| {});
    for (stream, status, class) in [
        (1, 429, ErrorClass::RateLimit),
        (2, 403, ErrorClass::Auth),
        (3, 500, ErrorClass::Internal),
    ] {
        tap.open(StreamId(stream), view(None), at(0));
        request(&tap, stream, stream, 1);
        tap.head(StreamId(stream), status, BodyKind::Json, None, at(5));
        tap.forward(
            StreamId(stream),
            S2C,
            Bytes::from_static(b"{\"error\":\"no\"}"),
            at(6),
        );
        tap.close(StreamId(stream), EndReason::Complete, at(7));
        let ops = ops(&mut core);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].classification.error_class, class);
        assert_eq!(ops[0].classification.status, McpStatus::ProtocolError);
        assert_eq!(ops[0].http_status, Some(status));
        assert_eq!(ops[0].duration, Duration::from_millis(6));
        assert_eq!(ops[0].transport, Transport::StreamableHttp);
        assert_eq!(ops[0].user_agent, "client/1");
        assert_eq!(ops[0].ip_prefix, "10.1.2.0/24");
        assert_eq!(ops[0].result_type, ResultType::None);
    }
    assert_eq!(core.pending(), 0);
}

#[test]
fn how_an_exchange_ends_decides_what_becomes_of_a_pending_request() {
    let (mut core, tap) = http_core(|_| {});
    let cases = [
        (
            EndReason::Complete,
            McpStatus::TransportError,
            ErrorClass::Transport,
            CancelSource::None,
        ),
        (
            EndReason::UpstreamFailed,
            McpStatus::TransportError,
            ErrorClass::Transport,
            CancelSource::None,
        ),
        (
            EndReason::ClientClosed,
            McpStatus::Cancelled,
            ErrorClass::Cancelled,
            CancelSource::StreamClose,
        ),
    ];
    for (stream, (reason, status, class, cancel)) in (1..).zip(cases) {
        tap.open(StreamId(stream), view(None), at(0));
        request(&tap, stream, 1, 1);
        tap.head(StreamId(stream), 200, BodyKind::Sse, None, at(2));
        tap.forward(
            StreamId(stream),
            S2C,
            Bytes::from_static(b": keep-alive\n\n"),
            at(3),
        );
        tap.close(StreamId(stream), reason, at(4));
        let ops = ops(&mut core);
        assert_eq!(ops.len(), 1, "{reason:?}");
        assert_eq!(ops[0].classification.status, status, "{reason:?}");
        assert_eq!(ops[0].classification.error_class, class);
        assert_eq!(ops[0].cancel_source, cancel);
    }
}

#[test]
fn in_a_legacy_session_an_answer_may_arrive_on_another_stream() {
    let (mut core, tap) = http_core(|_| {});
    let session = "0123456789abcdef0123456789abcdef";
    // The standalone event stream of the session.
    tap.open(StreamId(1), view(Some(session)), at(0));
    tap.head(StreamId(1), 200, BodyKind::Sse, None, at(1));
    // A request that is acknowledged with 202 and answered later on the event stream.
    tap.open(StreamId(2), view(Some(session)), at(10));
    request(&tap, 2, 7, 11);
    tap.head(StreamId(2), 202, BodyKind::Opaque, None, at(12));
    tap.close(StreamId(2), EndReason::Complete, at(13));
    core.drain();
    assert_eq!(core.pending(), 1, "the request waits for its answer");
    // The same id in another session is another request.
    tap.open(
        StreamId(3),
        view(Some("ffffffffffffffffffffffffffffffff")),
        at(14),
    );
    tap.head(StreamId(3), 200, BodyKind::Sse, None, at(15));
    let answer =
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[]}}\n\n";
    tap.forward(StreamId(3), S2C, Bytes::from(answer), at(16));
    core.drain();
    assert_eq!(
        core.pending(),
        1,
        "an answer in another session does not match"
    );
    tap.forward(StreamId(1), S2C, Bytes::from(answer), at(40));
    let ops = ops(&mut core);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].classification.status, McpStatus::Ok);
    assert_eq!(ops[0].duration, Duration::from_millis(29));
    assert_eq!(ops[0].session_key.as_deref(), Some(session));
    assert_eq!(ops[0].result_type, ResultType::Complete);
}

#[test]
fn a_body_that_cannot_be_read_is_reported_from_its_headers() {
    let (mut core, tap) = http_core(|_| {});
    let compressed = HeaderView {
        mcp_method: Some("tools/call".to_owned()),
        mcp_name: Some("echo".to_owned()),
        protocol_version: Some("2026-07-28".to_owned()),
        body_readable: false,
        ..view(None)
    };
    tap.open(StreamId(1), compressed, at(0));
    tap.forward(
        StreamId(1),
        C2S,
        Bytes::from_static(&[0x1f, 0x8b, 8, 0, 1, 2, 3]),
        at(1),
    );
    tap.head(StreamId(1), 200, BodyKind::Opaque, None, at(8));
    tap.forward(
        StreamId(1),
        S2C,
        Bytes::from_static(&[0x1f, 0x8b, 8, 0, 9]),
        at(9),
    );
    tap.close(StreamId(1), EndReason::Complete, at(10));
    // The same without the headers: nothing to report.
    tap.open(
        StreamId(2),
        HeaderView {
            body_readable: false,
            ..view(None)
        },
        at(11),
    );
    tap.close(StreamId(2), EndReason::Complete, at(12));
    let ops = ops(&mut core);
    assert_eq!(ops.len(), 1);
    let op = &ops[0];
    assert_eq!(
        (op.method.as_str(), op.target.as_str()),
        ("tools/call", "echo")
    );
    assert_eq!(op.jsonrpc_id, None);
    assert!(!op.is_notification);
    assert_eq!(op.http_status, Some(200));
    assert_eq!(op.classification.status, McpStatus::Ok);
    assert_eq!(op.duration, Duration::from_millis(10));
    assert_eq!(op.protocol_version, "2026-07-28");
    assert_eq!(core.stats().malformed, 0);
}

#[test]
fn the_export_buffer_drops_its_oldest_and_the_drops_are_reported_once() {
    let (mut core, tap) = http_core(|config| config.buffer_ops = 3);
    tap.open(StreamId(1), view(None), at(0));
    for n in 0..5 {
        let body = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/n{n}\"}}");
        tap.forward(StreamId(1), C2S, Bytes::from(body), at(n));
        // Each notification is its own document.
        tap.close(StreamId(1), EndReason::Complete, at(n));
        tap.open(StreamId(1), view(None), at(n));
    }
    core.drain();
    assert_eq!(core.buffered(), 3);
    assert_eq!(core.stats().dropped_ops, 2);
    let now = Instant::from_nanos(10_000_000_000);
    let batch = core.next_export(now, false).unwrap();
    let methods: Vec<&str> = batch.ops.iter().map(|op| op.method.as_str()).collect();
    assert_eq!(
        methods,
        ["notifications/n2", "notifications/n3", "notifications/n4"]
    );
    // While one export is in flight no other starts.
    assert!(core.next_export(now, true).is_none());
    assert_eq!(core.export_done(batch, ExportResult::Delivered, now), 2);
    assert_eq!(core.stats().exported_ops, 3);
}

#[test]
fn failed_exports_back_off_and_keep_the_batch_and_refusals_drop_it() {
    let (mut core, tap) = http_core(|config| config.batch_ops = 2);
    tap.open(StreamId(1), view(None), at(0));
    for id in 1..=3 {
        tap.forward(
            StreamId(1),
            C2S,
            Bytes::from(format!(
                "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/n{id}\"}}\n"
            )),
            at(0),
        );
        tap.close(StreamId(1), EndReason::Complete, at(0));
        tap.open(StreamId(1), view(None), at(0));
    }
    core.drain();
    let second = |seconds: u64| Instant::from_nanos(seconds * 1_000_000_000);
    // Nothing is due before the flush interval unless a full batch waits; here one does.
    let mut now = second(0);
    let mut waits = Vec::new();
    for _ in 0..7 {
        let batch = core.next_export(now, false).unwrap();
        assert_eq!(batch.ops.len(), 2);
        assert_eq!(core.export_done(batch, ExportResult::Retry, now), 0);
        assert_eq!(core.buffered(), 3, "a failed batch returns to the buffer");
        assert!(
            core.next_export(now, true).is_none(),
            "even a flush waits for the backoff"
        );
        let next = core.next_export_at().unwrap();
        waits.push(next.saturating_duration_since(now).as_secs());
        now = next;
    }
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
    assert_eq!(core.stats().export_failures, 7);
    // The oldest operations are first again after the failures.
    let batch = core.next_export(now, false).unwrap();
    assert_eq!(batch.ops[0].method, "notifications/n1");
    assert_eq!(core.export_done(batch, ExportResult::Rejected, now), 0);
    assert_eq!(core.stats().dropped_ops, 2);
    // A refusal is no outage: the next batch goes out at once, and reports what was dropped.
    let batch = core.next_export(now, true).unwrap();
    assert_eq!(batch.ops.len(), 1);
    assert_eq!(core.export_done(batch, ExportResult::Delivered, now), 2);
    assert_eq!(core.buffered(), 0);
    assert!(core.next_export_at().is_none());
}
