//! The streaming scanner against `serde_json`: for any JSON-RPC shaped message, any escaping and any chunking,
//! the fields the scanner keeps are the ones a full parse of the same bytes finds.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use crucible_env::{ChaChaRng, Rng as _};
use crucible_proxy::CaptureRule;
use crucible_proxy::canonical::Recorded;
use crucible_proxy::capture::ContentCapture;
use crucible_proxy::scan::{JsonId, Message, ResultMembers, Scanner, StateKey};
use crucible_proxy::sse::SseReader;
use hmac::{Hmac, KeyInit as _, Mac as _};
use proptest::prelude::*;
use serde_json::{Map, Value, json};
use sha2::Sha256;

/// The secret key the scanners hash `requestState` with.
const SECRET: &str = "csk_scan0000000000000000000000000000000000000";

fn keyed(scanner: Scanner) -> Scanner {
    scanner.keyed(StateKey::new(SECRET))
}

/// Any JSON value, a few levels deep, with keys that collide with the ones the scanner looks for.
fn junk() -> BoxedStrategy<Value> {
    junk_unboxed().boxed()
}

fn junk_unboxed() -> impl Strategy<Value = Value> {
    let key = prop_oneof![
        Just("id".to_owned()),
        Just("method".to_owned()),
        Just("name".to_owned()),
        Just("type".to_owned()),
        Just("isError".to_owned()),
        Just("_meta".to_owned()),
        Just("text".to_owned()),
        Just("message".to_owned()),
        Just("requestState".to_owned()),
        "[a-z]{1,6}",
        ".{0,12}",
    ];
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        (-1.0e9..1.0e9f64).prop_map(Value::from),
        ".{0,24}".prop_map(Value::from),
        Just(Value::from("}{][\",:\\ \n")),
    ];
    leaf.prop_recursive(4, 24, 4, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((key.clone(), inner), 0..4)
                .prop_map(|pairs| Value::Object(pairs.into_iter().collect())),
        ]
    })
}

/// A value for a member the scanner reads: usually of the expected type, sometimes not.
fn member(expected: impl Strategy<Value = Value> + 'static) -> BoxedStrategy<Option<Value>> {
    prop_oneof![
        3 => Just(None),
        6 => expected.prop_map(Some),
        1 => junk().prop_map(Some),
    ]
    .boxed()
}

fn text() -> BoxedStrategy<Value> {
    text_unboxed().boxed()
}

fn text_unboxed() -> impl Strategy<Value = Value> {
    prop_oneof![
        "[a-z/_]{1,16}".prop_map(Value::from),
        ".{0,40}".prop_map(Value::from),
        Just(Value::from("\u{1f600} caf\u{e9} \"q\" \\ \u{7}")),
    ]
}

fn id() -> BoxedStrategy<Value> {
    id_unboxed().boxed()
}

fn id_unboxed() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<u32>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        "[a-zA-Z0-9-]{1,12}".prop_map(Value::from),
        Just(Value::Null),
    ]
}

fn object(members: Vec<(&'static str, Option<Value>)>, extra: Value) -> Value {
    let mut map = Map::new();
    if let Value::Object(extra) = extra {
        map.extend(extra);
    }
    for (key, value) in members {
        match value {
            Some(value) => {
                map.insert(key.to_owned(), value);
            }
            // The scanner's members must come from this list only.
            None => {
                map.remove(key);
            }
        }
    }
    Value::Object(map)
}

fn info() -> BoxedStrategy<Value> {
    info_unboxed().boxed()
}

fn info_unboxed() -> impl Strategy<Value = Value> {
    (member(text()), member(text()), junk()).prop_map(|(name, version, extra)| {
        object(vec![("name", name), ("version", version)], extra)
    })
}

fn params() -> BoxedStrategy<Value> {
    params_unboxed().boxed()
}

fn params_unboxed() -> impl Strategy<Value = Value> {
    let meta = (
        member(id()),
        member(text()),
        member(text()),
        member(info()),
        junk(),
    )
        .prop_map(|(token, traceparent, version, client, extra)| {
            object(
                vec![
                    ("progressToken", token),
                    ("traceparent", traceparent),
                    ("io.modelcontextprotocol/protocolVersion", version),
                    ("io.modelcontextprotocol/clientInfo", client),
                ],
                extra,
            )
        });
    (
        member(text()),
        member(id()),
        member(id()),
        member(text()),
        member(info()),
        member(meta),
        (member(text()), junk()),
    )
        .prop_map(
            |(name, request_id, token, version, client, meta, (state, extra))| {
                object(
                    vec![
                        ("name", name),
                        ("requestId", request_id),
                        ("progressToken", token),
                        ("protocolVersion", version),
                        ("clientInfo", client),
                        ("_meta", meta),
                        ("requestState", state),
                    ],
                    extra,
                )
            },
        )
}

fn result() -> BoxedStrategy<Value> {
    result_unboxed().boxed()
}

fn result_unboxed() -> impl Strategy<Value = Value> {
    let block = (
        member(prop_oneof!["text|image|audio|resource", ".{0,6}"].prop_map(Value::from)),
        member(text()),
        junk(),
    )
        .prop_map(|(kind, words, extra)| object(vec![("type", kind), ("text", words)], extra));
    let content =
        prop::collection::vec(prop_oneof![4 => block, 1 => junk()], 0..6).prop_map(Value::Array);
    let meta = (member(info()), junk()).prop_map(|(server, extra)| {
        object(vec![("io.modelcontextprotocol/serverInfo", server)], extra)
    });
    (
        member(any::<bool>().prop_map(Value::from)),
        member(prop_oneof!["complete|input_required|task", ".{0,8}"].prop_map(Value::from)),
        member(text()),
        member(info()),
        member(content),
        member(meta),
        (member(text()), junk()),
    )
        .prop_map(
            |(is_error, result_type, version, server, content, meta, (state, extra))| {
                object(
                    vec![
                        ("isError", is_error),
                        ("resultType", result_type),
                        ("protocolVersion", version),
                        ("serverInfo", server),
                        ("content", content),
                        ("_meta", meta),
                        ("requestState", state),
                    ],
                    extra,
                )
            },
        )
}

fn message() -> BoxedStrategy<Value> {
    message_unboxed().boxed()
}

fn message_unboxed() -> impl Strategy<Value = Value> {
    let code = prop_oneof![
        (-32768..-32000i64).prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        (-1.0e6..1.0e6f64).prop_map(Value::from),
    ];
    let error = (member(code), member(text()), junk())
        .prop_map(|(code, words, extra)| object(vec![("code", code), ("message", words)], extra));
    (
        member(id()),
        member(text()),
        member(params()),
        member(result()),
        member(error),
    )
        .prop_map(|(id, method, params, result, error)| {
            object(
                vec![
                    ("id", id),
                    ("method", method),
                    ("params", params),
                    ("result", result),
                    ("error", error),
                ],
                json!({"jsonrpc": "2.0"}),
            )
        })
}

/// Rewrites every character outside ASCII as `\uXXXX` escapes (a surrogate pair where needed).
fn escape_non_ascii(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn cut_at(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn cut(text: &str) -> String {
    cut_at(text, 512)
}

/// The keyed hash of contract 9.6.
fn state_hash(state: &str) -> [u8; 16] {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(b"crucible.mrtr.v1\0");
    mac.update(state.as_bytes());
    let mut hash = [0; 16];
    hash.copy_from_slice(&mac.finalize().into_bytes()[..16]);
    hash
}

/// What a full parse finds, by the rules the scanner documents.
fn reference(value: &Value, wire_len: usize, is_batch: bool) -> Message {
    let string = |value: Option<&Value>| value.and_then(Value::as_str).map(cut);
    let id = |value: Option<&Value>| match value? {
        Value::Number(number) => Some(JsonId::Number(number.to_string())),
        Value::String(text) => Some(JsonId::String(cut(text))),
        _ => None,
    };
    let at = |path: &[&str]| {
        path.iter()
            .try_fold(value, |value, key| value.as_object()?.get(*key))
    };
    let meta_version = string(at(&[
        "params",
        "_meta",
        "io.modelcontextprotocol/protocolVersion",
    ]));
    // Messages are kept up to the scrubbing window of the 2 KiB error message cap.
    let words = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(|text| cut_at(text, 3_072))
    };
    let tool_text = match at(&["result", "content"]) {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(Value::as_object)
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .find_map(|block| words(block.get("text"))),
        _ => None,
    };
    let hash = |value: Option<&Value>| value.and_then(Value::as_str).map(state_hash);
    let mut content_blocks = BTreeMap::new();
    if let Some(Value::Array(blocks)) = at(&["result", "content"]) {
        for kind in blocks
            .iter()
            .filter_map(|block| block.as_object()?.get("type")?.as_str())
        {
            let known = content_blocks.len();
            if let Some(count) = content_blocks.get_mut(&cut(kind)) {
                *count += 1;
            } else if known < 16 {
                content_blocks.insert(cut(kind), 1);
            }
        }
    }
    // Two members can fill one field (`clientInfo` in `params` and in `params._meta`); the scanner keeps
    // whichever comes later in the enclosing object. `paths[0]` goes through `_meta`.
    let first = |paths: [&[&str]; 2]| {
        let meta_last = at(&paths[0][..1])
            .and_then(Value::as_object)
            .is_some_and(|object| {
                let position = |key: &str| object.keys().position(|k| k == key);
                position(paths[0][1]) > position(paths[1][1])
            });
        match (string(at(paths[0])), string(at(paths[1]))) {
            (Some(meta), Some(plain)) => Some(if meta_last { meta } else { plain }),
            (meta, plain) => meta.or(plain),
        }
    };
    Message {
        id: id(at(&["id"])),
        method: string(at(&["method"])),
        has_result: at(&["result"]).is_some(),
        has_error: at(&["error"]).is_some(),
        name: string(at(&["params", "name"])),
        request_id: id(at(&["params", "requestId"])),
        progress_token: id(at(&["params", "progressToken"])),
        meta_progress_token: id(at(&["params", "_meta", "progressToken"])),
        traceparent: string(at(&["params", "_meta", "traceparent"])),
        version_in_meta: meta_version.is_some(),
        protocol_version: meta_version.or_else(|| {
            // Both members may be present; the scanner keeps whichever comes last in the object.
            let params = string(at(&["params", "protocolVersion"]));
            let result = string(at(&["result", "protocolVersion"]));
            let result_last = value.as_object().is_some_and(|object| {
                let position = |key| object.keys().position(|k| k == key);
                position("result") > position("params")
            });
            match (params, result) {
                (Some(params), Some(result)) => Some(if result_last { result } else { params }),
                (params, result) => params.or(result),
            }
        }),
        client_name: first([
            &[
                "params",
                "_meta",
                "io.modelcontextprotocol/clientInfo",
                "name",
            ],
            &["params", "clientInfo", "name"],
        ]),
        client_version: first([
            &[
                "params",
                "_meta",
                "io.modelcontextprotocol/clientInfo",
                "version",
            ],
            &["params", "clientInfo", "version"],
        ]),
        server_name: first([
            &[
                "result",
                "_meta",
                "io.modelcontextprotocol/serverInfo",
                "name",
            ],
            &["result", "serverInfo", "name"],
        ]),
        server_version: first([
            &[
                "result",
                "_meta",
                "io.modelcontextprotocol/serverInfo",
                "version",
            ],
            &["result", "serverInfo", "version"],
        ]),
        is_error: at(&["result", "isError"]) == Some(&Value::Bool(true)),
        result_type: string(at(&["result", "resultType"])),
        error_code: at(&["error", "code"])
            .filter(|code| code.is_number())
            .and_then(|code| code.to_string().parse().ok()),
        content_blocks,
        error_message: words(at(&["error", "message"])),
        tool_text,
        state_in: hash(at(&["params", "requestState"])),
        state_out: hash(at(&["result", "requestState"])),
        is_batch,
        bytes: wire_len as u64,
        // Without capture rules nothing is recorded.
        capture_rule: None,
        arguments: None,
        result_members: ResultMembers::default(),
    }
}

/// The scanner's result in the reference's terms: the `_meta` flag only matters when a version was found.
fn normal(mut message: Message) -> Message {
    message.version_in_meta = message.version_in_meta && message.protocol_version.is_some();
    message
}

/// A value with a long string in it, so that recordings reach their window.
fn padded() -> BoxedStrategy<Value> {
    (
        junk(),
        "[a-z \"\\\\\n\u{1}\u{e9}\u{1f600}]{0,24}",
        0usize..120,
    )
        .prop_map(|(value, unit, times)| json!({"pad": unit.repeat(times), "value": value}))
        .boxed()
}

/// Whether `recorded` is what a recording may keep of the canonical text `full` within `window`: the whole text,
/// or a prefix cut before a unit (a character or an escape, at most 6 bytes) that did not fit.
fn kept_within(recorded: Option<&Recorded>, full: &str, window: usize) -> Result<(), String> {
    let Some(recorded) = recorded else {
        return Err(format!("nothing recorded of {full:?}"));
    };
    let whole = full.len() <= window && recorded.text == full && !recorded.more;
    let cut = full.len() > window
        && recorded.more
        && full.starts_with(&recorded.text)
        && recorded.text.len() <= window
        && recorded.text.len() + 6 > window;
    if whole || cut {
        Ok(())
    } else {
        Err(format!(
            "recorded {recorded:?} of {} bytes within {window}: {full:?}",
            full.len()
        ))
    }
}

fn chunked<'a>(wire: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
    let mut points: Vec<usize> = cuts.iter().map(|cut| cut % (wire.len() + 1)).collect();
    points.push(0);
    points.push(wire.len());
    points.sort_unstable();
    points.dedup();
    points
        .windows(2)
        .map(|pair| &wire[pair[0]..pair[1]])
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn line_framed_messages_match_a_full_parse(
        messages in prop::collection::vec((message(), any::<bool>(), any::<bool>()), 1..5),
        cuts in prop::collection::vec(any::<usize>(), 0..12),
    ) {
        let mut wire = Vec::new();
        let mut expected = Vec::new();
        for (value, escape, batch) in &messages {
            let mut text = serde_json::to_string(value).unwrap();
            if *escape {
                text = escape_non_ascii(&text);
            }
            expected.push(reference(value, text.len(), *batch));
            if *batch {
                // A batch of the same message twice.
                expected.push(reference(value, text.len(), true));
                text = format!("[{text}, {text}]");
            }
            wire.extend_from_slice(text.as_bytes());
            wire.push(b'\n');
        }
        let mut scanner = keyed(Scanner::lines());
        for chunk in chunked(&wire, &cuts) {
            scanner.push(chunk);
        }
        let got: Vec<Message> = scanner.take().into_iter().map(normal).collect();
        prop_assert_eq!(got, expected);
        prop_assert_eq!(scanner.malformed(), 0);
    }

    #[test]
    fn pretty_documents_and_event_streams_match_a_full_parse(
        value in message(),
        cuts in prop::collection::vec(any::<usize>(), 0..12),
    ) {
        // An HTTP body may be pretty-printed over many lines.
        let pretty = serde_json::to_string_pretty(&value).unwrap();
        let mut scanner = keyed(Scanner::document());
        for chunk in chunked(pretty.as_bytes(), &cuts) {
            scanner.push(chunk);
        }
        scanner.end_document();
        let got: Vec<Message> = scanner.take().into_iter().map(normal).collect();
        prop_assert_eq!(got, vec![reference(&value, pretty.len(), false)]);

        // The same message as the data of an SSE event, between a comment and another field.
        let compact = serde_json::to_string(&value).unwrap();
        let stream = format!(": ping\r\n\r\nid: 7\nevent: message\ndata: {compact}\n\n");
        let mut reader = SseReader::new().keyed(StateKey::new(SECRET));
        for chunk in chunked(stream.as_bytes(), &cuts) {
            reader.push(chunk);
        }
        let got: Vec<Message> = reader.take().into_iter().map(normal).collect();
        prop_assert_eq!(got, vec![reference(&value, compact.len(), false)]);
        prop_assert_eq!(reader.malformed(), 0);
    }

    #[test]
    fn recorded_content_is_the_canonical_text_of_a_full_parse(
        (arguments, content, structured) in (padded(), padded(), junk()),
        (is_error, captured_tool, name_first, escape, pretty) in
            (any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>(), any::<bool>()),
        max_bytes in prop_oneof![Just(16_384usize), 1usize..400],
        cuts in prop::collection::vec(any::<usize>(), 0..12),
    ) {
        // Whitespace and escapes on the wire, and keys in either order, change nothing of the canonical text.
        let render = |value: &Value| {
            let text = if pretty {
                serde_json::to_string_pretty(value).unwrap()
            } else {
                serde_json::to_string(value).unwrap()
            };
            if escape { escape_non_ascii(&text) } else { text }
        };
        let rule = CaptureRule { tool: "secret".to_owned(), max_bytes, ..CaptureRule::default() };
        let window = rule.window();
        let capture = ContentCapture::new(&[rule], ChaChaRng::from_u64(1).fork("capture"));

        let name = if captured_tool { "secret" } else { "echo" };
        let args_text = render(&arguments);
        let params = if name_first {
            format!(r#"{{"name": "{name}", "arguments": {args_text}}}"#)
        } else {
            format!(r#"{{"arguments": {args_text}, "name": "{name}"}}"#)
        };
        let request = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{params}}}"#);
        let mut scanner = Scanner::document().capturing(capture);
        for chunk in chunked(request.as_bytes(), &cuts) {
            scanner.push(chunk);
        }
        scanner.end_document();
        let messages = scanner.take();
        prop_assert_eq!(messages.len(), 1);
        if captured_tool {
            prop_assert_eq!(messages[0].capture_rule, Some(0));
            let full = serde_json::to_string(&arguments).unwrap();
            prop_assert_eq!(kept_within(messages[0].arguments.as_ref(), &full, window), Ok(()));
        } else {
            prop_assert_eq!(messages[0].capture_rule, None);
            prop_assert_eq!(&messages[0].arguments, &None);
        }

        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "s"}},
                "content": content,
                "requestState": "opaque",
                "structuredContent": structured,
                "isError": is_error,
            },
        });
        let wire = render(&response);
        let mut scanner = Scanner::document();
        scanner.record_results(window);
        for chunk in chunked(wire.as_bytes(), &cuts) {
            scanner.push(chunk);
        }
        scanner.end_document();
        let messages = scanner.take();
        prop_assert_eq!(messages.len(), 1);
        let members = &messages[0].result_members;
        for (recorded, value) in [
            (&members.content, &content),
            (&members.structured_content, &structured),
            (&members.is_error, &Value::Bool(is_error)),
        ] {
            let full = serde_json::to_string(value).unwrap();
            prop_assert_eq!(kept_within(recorded.as_ref(), &full, window), Ok(()));
        }
    }
}
