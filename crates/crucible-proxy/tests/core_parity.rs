//! What the proxy records that the SDKs record too (contracts 9.1, 9.6 and 9.10): the error message of a failed
//! operation, scrubbed with the shared rules and cut at 2 KiB, and the keyed hashes of the `requestState` of a
//! multi-round exchange, which itself is never kept.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use bytes::Bytes;
use crucible_env::{ChaChaRng, Instant};
use crucible_proxy::scan::StateKey;
use crucible_proxy::{CoreConfig, ExportResult, Now, Op, ProxyCore, StreamId, Tap};
use crucible_records::{CaptureSource, Direction, ErrorClass, McpStatus, ResultType, Timestamp};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

const C2S: Direction = Direction::ClientToServer;
const S2C: Direction = Direction::ServerToClient;
const SECRET: &str = "csk_parity00000000000000000000000000000000000";
const STATE: &str = "crucible-conformance-request-state-1";

fn at(millis: u64) -> Now {
    Now {
        mono: Instant::from_nanos(millis * 1_000_000),
        wall: Timestamp::from_micros(1_800_000_000_000_000 + millis as i64 * 1_000),
    }
}

fn stdio_core(key: Option<&str>) -> (ProxyCore, Tap) {
    let config = CoreConfig {
        state_key: key.and_then(StateKey::new),
        ..CoreConfig::new(CaptureSource::ProxyStdio)
    };
    let core = ProxyCore::new(config, Arc::new(ChaChaRng::from_u64(1)));
    let tap = core.tap();
    (core, tap)
}

fn send(tap: &Tap, direction: Direction, line: &str, when: u64) {
    tap.forward(
        StreamId::STDIO,
        direction,
        Bytes::from(format!("{line}\n")),
        at(when),
    );
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

/// The hash of contract 9.6, computed as an SDK computes it.
fn state_hash(secret: &str, state: &str) -> [u8; 16] {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(b"crucible.mrtr.v1\0");
    mac.update(state.as_bytes());
    let mut hash = [0; 16];
    hash.copy_from_slice(&mac.finalize().into_bytes()[..16]);
    hash
}

fn call(id: u64, tool: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}"}}}}"#)
}

#[test]
fn a_failed_operation_carries_the_servers_words_scrubbed_and_a_successful_one_none() {
    let (mut core, tap) = stdio_core(None);
    send(&tap, C2S, &call(1, "echo"), 0);
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"fine, mail ada@example.com"}]}}"#,
        1,
    );
    send(&tap, C2S, &call(2, "nope"), 2);
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"no tool nope; key csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo, mail ada@example.com"}}"#,
        3,
    );
    // A tool error's message is the text of its first text block, wherever `type` and `isError` fall.
    send(&tap, C2S, &call(3, "fail"), 4);
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"image","data":"AAAA"},{"text":"upstream said \u0022no\u0022 to ada@example.com","type":"text"},{"type":"text","text":"second"}],"isError":true}}"#,
        5,
    );
    let ops = ops(&mut core);
    assert_eq!(ops.len(), 3);
    assert_eq!(ops[0].classification.status, McpStatus::Ok);
    assert_eq!(ops[0].error_message, "");
    assert_eq!(ops[1].classification.error_class, ErrorClass::Validation);
    assert_eq!(
        ops[1].error_message,
        "no tool nope; key [redacted:secret_key], mail [redacted:email]"
    );
    assert_eq!(ops[2].classification.status, McpStatus::ToolError);
    assert_eq!(
        ops[2].error_message,
        "upstream said \"no\" to [redacted:email]"
    );
}

#[test]
fn a_long_message_is_cut_at_2_kib_after_scrubbing_and_no_secret_straddles_the_cut() {
    let (mut core, tap) = stdio_core(None);
    // A secret key that starts just before the cut: scrubbing reads past the cap, so no prefix of it is left.
    let message = format!(
        "{}csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo{}",
        "e".repeat(2_030),
        "x".repeat(100_000)
    );
    send(&tap, C2S, &call(1, "big"), 0);
    let answer =
        format!(r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-32603,"message":"{message}"}}}}"#);
    send(&tap, S2C, &answer, 1);
    // Text after the cut is never read, however long: the next message is still found.
    send(&tap, C2S, &call(2, "echo"), 2);
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"é"}}"#,
        3,
    );
    let ops = ops(&mut core);
    assert_eq!(ops.len(), 2);
    let cut = &ops[0].error_message;
    assert!(cut.len() <= 2_048, "{} bytes", cut.len());
    assert!(cut.starts_with(&"e".repeat(2_030)), "{cut}");
    assert!(!cut.contains("csk_"), "{cut}");
    assert_eq!(ops[1].error_message, "é");
}

#[test]
fn what_the_proxy_observed_is_the_message_when_the_server_gave_none() {
    let (mut core, tap) = stdio_core(None);
    send(&tap, C2S, &call(1, "slow"), 0);
    send(
        &tap,
        C2S,
        r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
        1,
    );
    send(&tap, C2S, &call(2, "hang"), 2);
    tap.session_end(at(3));
    let ops = ops(&mut core);
    let by_id = |id: &str| {
        ops.iter()
            .find(|op| op.jsonrpc_id.as_deref() == Some(id))
            .unwrap()
    };
    assert_eq!(by_id("1").classification.status, McpStatus::Cancelled);
    assert_eq!(by_id("1").error_message, "cancelled");
    assert_eq!(by_id("2").classification.status, McpStatus::TransportError);
    assert_eq!(
        by_id("2").error_message,
        "the connection ended before the server answered"
    );
    // The cancellation notification itself succeeded.
    let notification = ops.iter().find(|op| op.is_notification).unwrap();
    assert_eq!(notification.error_message, "");
}

#[test]
fn a_multi_round_exchange_carries_keyed_hashes_of_its_request_state_and_never_the_state() {
    let (mut core, tap) = stdio_core(Some(SECRET));
    send(&tap, C2S, &call(1, "needs_input"), 0);
    let input_required = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"resultType":"input_required","requestState":"{STATE}","inputRequests":{{"confirm":{{}}}}}}}}"#
    );
    send(&tap, S2C, &input_required, 1);
    // The retry echoes the state; JSON escapes are decoded before hashing, as a parser hands the value over.
    let escaped = STATE.replace('-', "\\u002d");
    let retry = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"needs_input","requestState":"{escaped}","inputResponses":{{"confirm":{{"action":"accept"}}}}}}}}"#
    );
    send(&tap, C2S, &retry, 2);
    send(
        &tap,
        S2C,
        r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"confirmed"}],"requestState":"ignored"}}"#,
        3,
    );
    let ops = ops(&mut core);
    assert_eq!(ops.len(), 2);
    let hash = state_hash(SECRET, STATE);
    assert_eq!(ops[0].result_type, ResultType::InputRequired);
    assert_eq!(ops[0].mrtr_state_out, Some(hash));
    assert_eq!(ops[0].mrtr_state_in, None);
    assert_eq!(ops[1].mrtr_state_in, Some(hash));
    // Only an input_required result has an outgoing state.
    assert_eq!(ops[1].result_type, ResultType::Complete);
    assert_eq!(ops[1].mrtr_state_out, None);
    let debug = format!("{ops:?}");
    assert!(!debug.contains(STATE), "the raw state was kept");
}

#[test]
fn without_a_key_no_request_state_is_read() {
    let (mut core, tap) = stdio_core(None);
    send(&tap, C2S, &call(1, "needs_input"), 0);
    let input_required = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"resultType":"input_required","requestState":"{STATE}"}}}}"#
    );
    send(&tap, S2C, &input_required, 1);
    let ops = ops(&mut core);
    assert_eq!(ops[0].result_type, ResultType::InputRequired);
    assert_eq!(ops[0].mrtr_state_out, None);
}
