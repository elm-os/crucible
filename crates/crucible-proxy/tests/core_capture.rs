//! Content capture in the core (contract 9.10): which calls are captured and what of them, by the SDKs' rules;
//! how captured text is sampled, made canonical, scrubbed and cut; and the bounds that keep capture from costing
//! more than telemetry.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use bytes::Bytes;
use crucible_env::{ChaChaRng, Instant};
use crucible_proxy::encode::{ResourceInfo, encode};
use crucible_proxy::{
    BodyKind, CaptureRule, CoreConfig, EndReason, ExportResult, HeaderView, Now, Op, ProxyCore,
    StreamId, Tap,
};
use crucible_records::{CaptureSource, Direction, Timestamp};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use prost::Message as _;

const C2S: Direction = Direction::ClientToServer;
const S2C: Direction = Direction::ServerToClient;

/// The planted secrets of the `mcp-secret` conformance scenario.
const NOTE: &str = "Reach ada.lovelace@example.com or +1 415 555 0134; card 4111 1111 1111 1111; header Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0; token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhZGEifQ.c2lnbmF0dXJl; key csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo; host 203.0.113.42 and 2001:db8::8a2e:370:7334; -----BEGIN PRIVATE KEY-----\\nMIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX6Ppy1tPf9Cnzj4p4WGeKLs1Pt8Qu\\n-----END PRIVATE KEY-----";
/// The same, scrubbed by the shared rules.
const SCRUBBED: &str = "Reach [redacted:email] or [redacted:phone]; card [redacted:card]; header [redacted:bearer]; token [redacted:jwt]; key [redacted:secret_key]; host [redacted:ip] and [redacted:ip]; [redacted:pem]";
const KEY: &str = "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo";

fn at(millis: u64) -> Now {
    Now {
        mono: Instant::from_nanos(millis * 1_000_000),
        wall: Timestamp::from_micros(1_800_000_000_000_000 + millis as i64 * 1_000),
    }
}

fn rules(rules: &[&str]) -> Vec<CaptureRule> {
    rules
        .iter()
        .map(|rule| CaptureRule::parse(rule).unwrap())
        .collect()
}

fn core(
    source: CaptureSource,
    capture: &[&str],
    tweak: impl FnOnce(&mut CoreConfig),
) -> (ProxyCore, Tap) {
    let mut config = CoreConfig {
        capture: rules(capture),
        ..CoreConfig::new(source)
    };
    tweak(&mut config);
    let core = ProxyCore::new(config, Arc::new(ChaChaRng::from_u64(5)));
    let tap = core.tap();
    (core, tap)
}

fn stdio(capture: &[&str]) -> (ProxyCore, Tap) {
    core(CaptureSource::ProxyStdio, capture, |_| {})
}

fn send(tap: &Tap, direction: Direction, line: &str, when: u64) {
    tap.forward(
        StreamId::STDIO,
        direction,
        Bytes::from(format!("{line}\n")),
        at(when),
    );
}

/// A call with its tool name first, as most clients write it.
fn call(id: u64, tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}"#
    )
}

/// A call with its arguments before its tool name.
fn call_arguments_first(id: u64, tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","params":{{"arguments":{arguments},"name":"{tool}"}},"id":{id},"method":"tools/call"}}"#
    )
}

fn answer(id: u64, result: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#)
}

fn text_result(text: &str) -> String {
    format!(r#"{{"content":[{{"type":"text","text":"{text}"}}]}}"#)
}

/// Everything finished so far, through a healthy export.
fn finished(core: &mut ProxyCore) -> Vec<Op> {
    core.drain();
    let far = Instant::from_nanos(u64::MAX / 2);
    let mut all = Vec::new();
    while let Some(batch) = core.next_export(far, true) {
        all.extend(batch.ops.iter().cloned());
        core.export_done(batch, ExportResult::Delivered, far);
    }
    all
}

fn by_id<'a>(ops: &'a [Op], id: &str) -> &'a Op {
    ops.iter()
        .find(|op| op.jsonrpc_id.as_deref() == Some(id))
        .unwrap()
}

fn args(op: &Op) -> Option<&str> {
    op.content.as_ref()?.args.as_deref()
}

fn result(op: &Op) -> Option<&str> {
    op.content.as_ref()?.result.as_deref()
}

#[test]
fn without_a_rule_nothing_is_captured() {
    let (mut core, tap) = stdio(&[]);
    send(
        &tap,
        C2S,
        &call(1, "secret", &format!(r#"{{"note":"{NOTE}"}}"#)),
        0,
    );
    send(&tap, S2C, &answer(1, &text_result(NOTE)), 1);
    let ops = finished(&mut core);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].content, None);
    let body = encode(&resource(), &ops);
    let spans = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
    let span = &spans.resource_spans[0].scope_spans[0].spans[0];
    let captured = span
        .attributes
        .iter()
        .find(|kv| kv.key == "crucible.mcp.content.captured")
        .and_then(|kv| kv.value.as_ref()?.value.clone());
    assert_eq!(captured, Some(Value::BoolValue(false)));
    assert!(!String::from_utf8_lossy(&body).contains("ada.lovelace"));
}

fn resource() -> ResourceInfo {
    ResourceInfo {
        service_name: "capture".to_owned(),
        instance_id: "instance-1".to_owned(),
        deploy_id: None,
        capture_source: CaptureSource::ProxyStdio,
    }
}

#[test]
fn a_captured_call_carries_its_arguments_and_result_scrubbed() {
    let (mut core, tap) = stdio(&["secret"]);
    send(
        &tap,
        C2S,
        &call(1, "secret", &format!(r#"{{"note":"{NOTE}"}}"#)),
        0,
    );
    // Only content, structured content and isError are taken: never the state or the metadata of a result.
    send(
        &tap,
        S2C,
        &answer(
            1,
            &format!(
                r#"{{"_meta":{{"trace":"x"}},"requestState":"opaque-state","content":[{{"type":"text","text":"{NOTE}"}}],"isError":false,"structuredContent":{{"note":"{NOTE}"}}}}"#
            ),
        ),
        1,
    );
    send(
        &tap,
        C2S,
        &call(2, "echo", r#"{"text":"ada@example.com"}"#),
        2,
    );
    send(&tap, S2C, &answer(2, &text_result("ada@example.com")), 3);
    let ops = finished(&mut core);
    let secret = by_id(&ops, "1");
    assert_eq!(
        args(secret),
        Some(format!(r#"{{"note":"{SCRUBBED}"}}"#).as_str())
    );
    assert_eq!(
        result(secret),
        Some(
            format!(
                r#"{{"content":[{{"type":"text","text":"{SCRUBBED}"}}],"structuredContent":{{"note":"{SCRUBBED}"}},"isError":false}}"#
            )
            .as_str()
        )
    );
    let content = secret.content.as_ref().unwrap();
    assert_eq!(content.sample_rate.get(), 1.0);
    assert!(!content.truncated);
    // A tool no rule names is not captured.
    assert_eq!(by_id(&ops, "2").content, None);

    // The span carries the keys the SDKs send, and no planted value.
    let body = encode(&resource(), &ops);
    let text = String::from_utf8_lossy(&body);
    for planted in [
        "ada.lovelace@example.com",
        KEY,
        "4111 1111 1111 1111",
        "opaque-state",
        "203.0.113.42",
    ] {
        assert!(!text.contains(planted), "{planted} was exported");
    }
    let spans = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
    let span = spans.resource_spans[0].scope_spans[0]
        .spans
        .iter()
        .find(|span| span.name == "tools/call secret")
        .unwrap();
    let value = |key: &str| {
        span.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref()?.value.clone())
    };
    assert_eq!(
        value("crucible.mcp.content.captured"),
        Some(Value::BoolValue(true))
    );
    assert_eq!(
        value("crucible.mcp.content.sample_rate"),
        Some(Value::DoubleValue(1.0))
    );
    assert_eq!(
        value("crucible.mcp.content.truncation"),
        Some(Value::StringValue("none".to_owned()))
    );
    assert_eq!(
        value("gen_ai.tool.call.arguments"),
        Some(Value::StringValue(format!(r#"{{"note":"{SCRUBBED}"}}"#)))
    );
}

#[test]
fn escaped_and_spaced_text_is_scrubbed_like_plain_text() {
    let (mut core, tap) = stdio(&["*"]);
    // The same secrets behind JSON escapes and between spaces: decoded before the rules run.
    let arguments = r#"{ "mail" : "ada@example.com", "key": "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo", "path": "a\/b", "quote": "say \"hi\"\n" }"#;
    send(&tap, C2S, &call(1, "lookup", arguments), 0);
    send(
        &tap,
        S2C,
        &answer(
            1,
            r#"{"content":[{"type":"text","text":"reply to ada@example.com é"}]}"#,
        ),
        1,
    );
    let ops = finished(&mut core);
    assert_eq!(
        args(&ops[0]),
        Some(
            r#"{"mail":"[redacted:email]","key":"[redacted:secret_key]","path":"a/b","quote":"say \"hi\"\n"}"#
        )
    );
    assert_eq!(
        result(&ops[0]),
        Some("{\"content\":[{\"type\":\"text\",\"text\":\"reply to [redacted:email] \u{e9}\"}]}")
    );
}

#[test]
fn the_tool_name_may_follow_the_arguments() {
    let (mut core, tap) = stdio(&["secret"]);
    send(
        &tap,
        C2S,
        &call_arguments_first(1, "secret", r#"{"note":"mail ada@example.com"}"#),
        0,
    );
    send(&tap, S2C, &answer(1, &text_result("ok")), 1);
    // Recorded before the name was known, then dropped: no rule names this tool.
    send(
        &tap,
        C2S,
        &call_arguments_first(2, "echo", r#"{"note":"mail ada@example.com"}"#),
        2,
    );
    send(&tap, S2C, &answer(2, &text_result("ok")), 3);
    let ops = finished(&mut core);
    assert_eq!(
        args(by_id(&ops, "1")),
        Some(r#"{"note":"mail [redacted:email]"}"#)
    );
    assert_eq!(
        result(by_id(&ops, "1")),
        Some(r#"{"content":[{"type":"text","text":"ok"}]}"#)
    );
    assert_eq!(by_id(&ops, "2").content, None);
}

#[test]
fn fields_are_cut_at_the_cap_and_no_part_of_a_secret_straddles_it() {
    let (mut core, tap) = stdio(&["*,max_bytes=100"]);
    for before in 70..=100 {
        let note = format!("{}{KEY} {}", "a".repeat(before), "z".repeat(20_000));
        let id = before as u64;
        send(
            &tap,
            C2S,
            &call(id, "big", &format!(r#"{{"note":"{note}"}}"#)),
            id,
        );
        send(&tap, S2C, &answer(id, &text_result(&note)), id);
    }
    let ops = finished(&mut core);
    assert_eq!(ops.len(), 31);
    for op in &ops {
        let content = op.content.as_ref().unwrap();
        assert!(content.truncated);
        for field in [args(op).unwrap(), result(op).unwrap()] {
            assert!(field.len() <= 100, "{} bytes", field.len());
            assert!(!field.contains("csk_"), "{field}");
        }
    }
    // A value within the cap is whole and not marked cut.
    let (mut core, tap) = stdio(&["*,max_bytes=100"]);
    send(&tap, C2S, &call(1, "small", r#"{"n":1}"#), 0);
    send(&tap, S2C, &answer(1, r#"{"content":[]}"#), 1);
    let ops = finished(&mut core);
    let content = ops[0].content.as_ref().unwrap();
    assert!(!content.truncated);
    assert_eq!(content.args.as_deref(), Some(r#"{"n":1}"#));
}

#[test]
fn a_rule_samples_its_calls_and_may_take_one_field() {
    let (mut core, tap) = stdio(&[
        "sampled,sample_rate=0.5",
        "args_only,result=false",
        "result_only,args=false",
    ]);
    for id in 0..400 {
        send(&tap, C2S, &call(id, "sampled", r#"{"n":1}"#), id);
        send(&tap, S2C, &answer(id, &text_result("r")), id);
    }
    send(&tap, C2S, &call(1_000, "args_only", r#"{"n":1}"#), 1_000);
    send(&tap, S2C, &answer(1_000, &text_result("r")), 1_000);
    send(&tap, C2S, &call(1_001, "result_only", r#"{"n":1}"#), 1_001);
    send(&tap, S2C, &answer(1_001, &text_result("r")), 1_001);
    let ops = finished(&mut core);
    let sampled = ops[..400].iter().filter(|op| op.content.is_some()).count();
    assert!(
        (150..=250).contains(&sampled),
        "{sampled} of 400 captured at rate 0.5"
    );
    for op in ops[..400].iter().filter(|op| op.content.is_some()) {
        assert_eq!(op.content.as_ref().unwrap().sample_rate.get(), 0.5);
        assert!(args(op).is_some() && result(op).is_some());
    }
    let args_only = by_id(&ops, "1000");
    assert!(args(args_only).is_some() && result(args_only).is_none());
    let result_only = by_id(&ops, "1001");
    assert!(args(result_only).is_none() && result(result_only).is_some());
}

#[test]
fn the_first_matching_rule_applies() {
    let (mut core, tap) = stdio(&["search_*,sample_rate=0", "*"]);
    send(&tap, C2S, &call(1, "search_docs", r#"{"q":"x"}"#), 0);
    send(&tap, C2S, &call(2, "lookup", r#"{"q":"x"}"#), 1);
    send(&tap, S2C, &answer(1, &text_result("r")), 2);
    send(&tap, S2C, &answer(2, &text_result("r")), 3);
    let ops = finished(&mut core);
    assert_eq!(by_id(&ops, "1").content, None);
    assert!(by_id(&ops, "2").content.is_some());
}

#[test]
fn a_result_belongs_to_its_own_call_whatever_the_order() {
    let (mut core, tap) = stdio(&["captured"]);
    send(&tap, C2S, &call(1, "captured", r#"{"n":1}"#), 0);
    send(&tap, C2S, &call(2, "other", r#"{"n":2}"#), 1);
    // Answers out of order, and the id after the result.
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"two"}]},"id":2}"#,
        2,
    );
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"one"}]},"id":1}"#,
        3,
    );
    let ops = finished(&mut core);
    assert_eq!(
        result(by_id(&ops, "1")),
        Some(r#"{"content":[{"type":"text","text":"one"}]}"#)
    );
    assert_eq!(by_id(&ops, "2").content, None);
    // An error answer has no result to capture, and the arguments stay.
    let (mut core, tap) = stdio(&["*"]);
    send(&tap, C2S, &call(1, "fails", r#"{"n":1}"#), 0);
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"no"}}"#,
        1,
    );
    let ops = finished(&mut core);
    assert_eq!(args(&ops[0]), Some(r#"{"n":1}"#));
    assert_eq!(result(&ops[0]), None);
}

#[test]
fn waiting_content_has_a_byte_bound_and_what_it_drops_is_counted() {
    let (mut core, tap) = core(CaptureSource::ProxyStdio, &["*"], |config| {
        config.content_bytes = 250
    });
    let arguments = format!(r#"{{"text":"{}"}}"#, "x".repeat(90));
    for id in 0..5 {
        send(&tap, C2S, &call(id, "slow", &arguments), id);
    }
    for id in 0..5 {
        send(&tap, S2C, &answer(id, &text_result("done")), 10 + id);
    }
    // Room again once the first answers arrived.
    send(&tap, C2S, &call(9, "slow", &arguments), 20);
    send(&tap, S2C, &answer(9, &text_result("done")), 21);
    let ops = finished(&mut core);
    assert_eq!(ops.len(), 6);
    let captured = ops.iter().filter(|op| op.content.is_some()).count();
    assert_eq!(captured, 3);
    assert!(by_id(&ops, "9").content.is_some());
    assert_eq!(core.stats().dropped_content, 3);
}

#[test]
fn http_answers_in_json_and_event_streams_are_captured() {
    let (mut core, tap) = core(CaptureSource::ProxyHttp, &["*"], |_| {});
    let view = HeaderView {
        body_readable: true,
        ..HeaderView::default()
    };
    tap.open(StreamId(1), view.clone(), at(0));
    tap.forward(
        StreamId(1),
        C2S,
        Bytes::from(call(1, "lookup", r#"{"mail":"ada@example.com"}"#)),
        at(1),
    );
    tap.head(StreamId(1), 200, BodyKind::Json, None, at(2));
    tap.forward(
        StreamId(1),
        S2C,
        Bytes::from(answer(1, &text_result("json answer"))),
        at(3),
    );
    tap.close(StreamId(1), EndReason::Complete, at(4));
    tap.open(StreamId(2), view, at(5));
    tap.forward(
        StreamId(2),
        C2S,
        Bytes::from(call(2, "lookup", r#"{"n":2}"#)),
        at(6),
    );
    tap.head(StreamId(2), 200, BodyKind::Sse, None, at(7));
    let event = format!(
        "event: message\ndata: {}\n\n",
        answer(2, &text_result("event answer"))
    );
    // The event arrives in two pieces.
    let (first, second) = event.split_at(30);
    tap.forward(StreamId(2), S2C, Bytes::from(first.to_owned()), at(8));
    tap.forward(StreamId(2), S2C, Bytes::from(second.to_owned()), at(9));
    tap.close(StreamId(2), EndReason::Complete, at(10));
    let ops = finished(&mut core);
    assert_eq!(
        args(by_id(&ops, "1")),
        Some(r#"{"mail":"[redacted:email]"}"#)
    );
    assert_eq!(
        result(by_id(&ops, "1")),
        Some(r#"{"content":[{"type":"text","text":"json answer"}]}"#)
    );
    assert_eq!(
        result(by_id(&ops, "2")),
        Some(r#"{"content":[{"type":"text","text":"event answer"}]}"#)
    );
}

#[test]
fn a_huge_value_is_read_to_its_end_and_kept_only_to_the_cap() {
    // 5 MiB of text, the largest capture-overhead shape: the next message is still found.
    let (mut core, tap) = stdio(&["*"]);
    let big = "y".repeat(5 << 20);
    send(&tap, C2S, &call(1, "shape", r#"{"shape":"text-5m"}"#), 0);
    // The analyser keeps up with the chunks, so no copy is dropped.
    let line = format!("{}\n", answer(1, &text_result(&big)));
    for chunk in line.as_bytes().chunks(64 * 1024) {
        tap.forward(StreamId::STDIO, S2C, Bytes::copy_from_slice(chunk), at(1));
        core.drain();
    }
    send(&tap, C2S, &call(2, "shape", r#"{"shape":"text-200b"}"#), 2);
    send(&tap, S2C, &answer(2, &text_result("short")), 3);
    let ops = finished(&mut core);
    assert_eq!(ops.len(), 2);
    let content = by_id(&ops, "1").content.as_ref().unwrap();
    assert!(content.truncated);
    assert!(content.result.as_ref().unwrap().len() <= 16 * 1024);
    assert_eq!(
        result(by_id(&ops, "2")),
        Some(r#"{"content":[{"type":"text","text":"short"}]}"#)
    );
}
