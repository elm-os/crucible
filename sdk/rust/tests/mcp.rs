//! The server wrapper over rmcp's in-memory transport, in both protocol eras, against a fake collector.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::{Collector, GLOBAL, Mode, SECRET_KEY, Span, init};
use crucible_sdk::mcp::{
    self, CaptureRule, Field, Identity, Instrumentation, Subclass, annotate_error, mark_not_found,
};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ClientRequest, ContentBlock, ErrorData, Implementation, InputRequiredResult,
    ProgressNotificationParam, ProtocolVersion, RequestMetaObject, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::{
    ClientLifecycleMode, PeerRequestOptions, RequestContext, RoleClient, RoleServer,
    RunningService, serve_client_with_lifecycle,
};
use rmcp::{ServerHandler, ServiceExt as _};
use serde_json::{Value, json};

const STATE: &str = "state-1";

struct Orders;

fn text(s: &str) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(s)]).into()
}

impl ServerHandler for Orders {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("orders", "2.1.0"))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arg = |name: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|a| a.get(name))
                .cloned()
                .unwrap_or(Value::Null)
        };
        match request.name.as_ref() {
            "echo" => Ok(text(arg("text").as_str().unwrap_or(""))),
            "fail" => {
                annotate_error(&context, Subclass::Upstream);
                Ok(CallToolResult::error(vec![ContentBlock::text("upstream refused")]).into())
            }
            "missing" => {
                mark_not_found(&context);
                Ok(CallToolResult::error(vec![ContentBlock::text("no such order")]).into())
            }
            "throw" => Err(ErrorData::internal_error(
                "exploded holding csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo",
                None,
            )),
            "slow" => {
                let steps = arg("steps").as_u64().unwrap_or(0);
                for i in 0..steps {
                    if let Some(token) = context.meta.get_progress_token() {
                        let params = ProgressNotificationParam::new(token, i as f64);
                        mcp::notify_progress(&context, params).await.unwrap();
                    }
                }
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(arg("ms").as_u64().unwrap_or(0))) => Ok(text("done")),
                    () = context.ct.cancelled() => Err(ErrorData::internal_error("cancelled", None)),
                }
            }
            "needs_input" => Ok(match &request.request_state {
                None => InputRequiredResult::from_request_state(STATE).into(),
                Some(_) => text("confirmed"),
            }),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        }
    }
}

fn client_info() -> ClientConfig {
    ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("test-client", "0.9.0"),
    )
}

/// Serves `Orders` wrapped by `instrumentation` and connects a client in `lifecycle`.
async fn connect(
    instrumentation: &Instrumentation,
    lifecycle: ClientLifecycleMode,
) -> RunningService<RoleClient, ClientConfig> {
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let server = instrumentation.wrap(Orders);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _quit = running.waiting().await;
        }
    });
    serve_client_with_lifecycle(client_info(), client_io, lifecycle)
        .await
        .unwrap()
}

fn legacy() -> ClientLifecycleMode {
    ClientLifecycleMode::Initialize
}

fn modern() -> ClientLifecycleMode {
    ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    }
}

fn call(name: &str, args: Value) -> CallToolRequestParams {
    let Value::Object(args) = args else {
        panic!("arguments are an object")
    };
    CallToolRequestParams::new(name.to_owned()).with_arguments(args)
}

/// The server spans, by name.
async fn finish(
    collector: &Collector,
    mut client: RunningService<RoleClient, ClientConfig>,
) -> BTreeMap<String, Span> {
    client.close().await.unwrap();
    // The server's handlers have returned before the client saw their responses; give the notification
    // handlers, which run detached, a moment.
    tokio::time::sleep(Duration::from_millis(100)).await;
    crucible_sdk::flush().await.unwrap();
    collector
        .spans()
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect()
}

#[tokio::test]
async fn legacy_operations_carry_the_wire_keys() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |o| o.deploy_id = "deploy-7".to_owned()).await;
    let instrumentation = Instrumentation::new(mcp::Options::default()).unwrap();
    let client = connect(&instrumentation, legacy()).await;
    client
        .call_tool(call("echo", json!({ "text": "hi" })))
        .await
        .unwrap();
    client.call_tool(call("fail", json!({}))).await.unwrap();
    client.call_tool(call("missing", json!({}))).await.unwrap();
    let thrown = client
        .call_tool(call("throw", json!({})))
        .await
        .unwrap_err();
    assert!(thrown.to_string().contains("exploded"), "{thrown}");
    let spans = finish(&collector, client).await;

    let initialize = &spans["initialize"];
    assert_eq!(initialize.attr("mcp.protocol.version"), json!("2025-11-25"));
    assert_eq!(initialize.attr("jsonrpc.request.id"), json!("0"));
    let initialized = &spans["notifications/initialized"];
    assert_eq!(
        initialized.attr("crucible.mcp.is_notification"),
        json!(true)
    );
    assert_eq!(initialized.attr("crucible.mcp.era"), json!("legacy"));
    assert_eq!(initialized.attr("jsonrpc.request.id"), json!(null));

    let echo = &spans["tools/call echo"];
    assert_eq!(echo.kind, 2, "SERVER");
    assert_eq!(echo.parent_span_id, "");
    for (key, value) in [
        ("mcp.method.name", json!("tools/call")),
        ("gen_ai.tool.name", json!("echo")),
        ("crucible.mcp.capture_source", json!("sdk")),
        ("crucible.mcp.transport", json!("stdio")),
        ("crucible.mcp.era", json!("legacy")),
        ("mcp.protocol.version", json!("2025-11-25")),
        ("crucible.mcp.client.name", json!("test-client")),
        ("crucible.mcp.client.version", json!("0.9.0")),
        ("crucible.mcp.server.name", json!("orders")),
        ("crucible.mcp.server.version", json!("2.1.0")),
        ("crucible.mcp.result_type", json!("complete")),
        ("crucible.mcp.content.blocks.text", json!(1)),
        ("error.type", json!(null)),
        ("crucible.mcp.content.captured", json!(null)),
        ("gen_ai.tool.call.arguments", json!(null)),
        ("mcp.session.id", json!(null)),
        ("crucible.mcp.progress.count", json!(null)),
    ] {
        assert_eq!(echo.attr(key), value, "{key}");
    }
    assert!(echo.attr("jsonrpc.request.id").is_string());
    assert_eq!(echo.resource["crucible.deploy.id"], json!("deploy-7"));

    let fail = &spans["tools/call fail"];
    assert_eq!(fail.attr("error.type"), json!("tool_error"));
    assert_eq!(
        fail.attr("crucible.mcp.error.class"),
        json!("tool_execution")
    );
    assert_eq!(fail.attr("crucible.mcp.error.subclass"), json!("upstream"));
    assert_eq!(fail.status_message, "upstream refused");
    let missing = &spans["tools/call missing"];
    assert_eq!(missing.attr("crucible.mcp.error.class"), json!("not_found"));
    assert_eq!(missing.attr("crucible.mcp.error.subclass"), json!(null));

    let throw = &spans["tools/call throw"];
    assert_eq!(throw.attr("error.type"), json!("-32603"));
    assert_eq!(throw.attr("rpc.response.status_code"), json!("-32603"));
    assert_eq!(throw.attr("crucible.mcp.error.class"), json!("internal"));
    assert_eq!(throw.attr("crucible.mcp.result_type"), json!(null));
    assert_eq!(
        throw.status_message,
        "exploded holding [redacted:secret_key]"
    );
    crucible_sdk::shutdown().await.unwrap();
}

#[tokio::test]
async fn modern_operations_carry_the_wire_keys_and_multi_round_hashes() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;
    let instrumentation = Instrumentation::new(mcp::Options::default()).unwrap();
    let client = connect(&instrumentation, modern()).await;
    client
        .call_tool(call("echo", json!({ "text": "hi" })))
        .await
        .unwrap();
    let CallToolResponse::InputRequired(asked) = client
        .call_tool_once(call("needs_input", json!({})))
        .await
        .unwrap()
    else {
        panic!("needs_input answers input_required")
    };
    let mut retry = call("needs_input", json!({}));
    retry.request_state = asked.request_state;
    client.call_tool_once(retry).await.unwrap();
    client
        .call_tool(call("slow", json!({ "steps": 3 })))
        .await
        .unwrap();
    let spans = finish(&collector, client).await;

    let echo = &spans["tools/call echo"];
    assert_eq!(echo.attr("crucible.mcp.era"), json!("modern"));
    assert_eq!(echo.attr("mcp.protocol.version"), json!("2026-07-28"));
    assert_eq!(echo.attr("crucible.mcp.client.name"), json!("test-client"));
    assert_eq!(echo.attr("crucible.mcp.result_type"), json!("complete"));
    assert!(spans.contains_key("server/discover"));
    assert!(!spans.contains_key("initialize"));

    let hash = |state: &str| {
        // Contract 9.6, computed independently: HMAC-SHA256 over the label, a zero byte and the state.
        use hmac::{Hmac, KeyInit as _, Mac as _};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(SECRET_KEY.as_bytes()).unwrap();
        mac.update(b"crucible.mrtr.v1\0");
        mac.update(state.as_bytes());
        let bytes = mac.finalize().into_bytes();
        bytes[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let all = collector.spans();
    let ask = all
        .iter()
        .find(|s| s.attr("crucible.mcp.result_type") == json!("input_required"))
        .unwrap();
    assert_eq!(ask.attr("crucible.mcp.mrtr.state_out"), json!(hash(STATE)));
    assert_eq!(ask.attr("crucible.mcp.mrtr.state_in"), json!(null));
    let answer = all
        .iter()
        .find(|s| s.attr("crucible.mcp.mrtr.state_in") != json!(null))
        .unwrap();
    assert_eq!(
        answer.attr("crucible.mcp.mrtr.state_in"),
        json!(hash(STATE))
    );
    assert_eq!(answer.attr("crucible.mcp.result_type"), json!("complete"));
    for span in &all {
        let wire = format!("{:?}", span.attributes);
        assert!(
            !wire.contains(STATE),
            "the raw state never leaves the sender: {wire}"
        );
    }

    let slow = &spans["tools/call slow"];
    assert_eq!(slow.attr("crucible.mcp.progress.count"), json!(3));
    crucible_sdk::shutdown().await.unwrap();
}

#[tokio::test]
async fn trace_context_cancellation_and_identity() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;
    let instrumentation = Instrumentation::new(mcp::Options {
        identify: Some(Arc::new(|_| Identity {
            user_id: "u-7".to_owned(),
            subject: "subject-1".to_owned(),
            issuer: "https://issuer.example".to_owned(),
            tenant: "acme".to_owned(),
            handle_id: "cart-9".to_owned(),
        })),
        handle_params: BTreeMap::from([("echo".to_owned(), "text".to_owned())]),
        ..mcp::Options::default()
    })
    .unwrap();
    let client = connect(&instrumentation, legacy()).await;

    let mut meta = RequestMetaObject::new();
    meta.set_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
    meta.0.0.insert("baggage".to_owned(), json!("user=ada"));
    let request = ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(call(
        "echo",
        json!({ "text": "from-arguments" }),
    )));
    client
        .send_request_with_option(request, PeerRequestOptions::no_options().with_meta(meta))
        .await
        .unwrap()
        .await_response()
        .await
        .unwrap();

    let slow = ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(call(
        "slow",
        json!({ "ms": 5000 }),
    )));
    let pending = client
        .send_request_with_option(slow, PeerRequestOptions::no_options())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    pending
        .cancel(Some("user gave up".to_owned()))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let spans = finish(&collector, client).await;

    let echo = &spans["tools/call echo"];
    assert_eq!(echo.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(echo.parent_span_id, "00f067aa0ba902b7");
    assert_eq!(echo.attr("user.id"), json!("u-7"));
    assert_eq!(echo.attr("crucible.mcp.user.subject"), json!("subject-1"));
    assert_eq!(
        echo.attr("crucible.mcp.user.issuer"),
        json!("https://issuer.example")
    );
    assert_eq!(echo.attr("crucible.mcp.tenant"), json!("acme"));
    assert_eq!(
        echo.attr("crucible.mcp.identity.source"),
        json!("identify_hook")
    );
    // The hook's handle id wins over the handle_params argument.
    assert_eq!(echo.attr("crucible.mcp.handle.id"), json!("cart-9"));
    assert!(!format!("{:?}", collector.exports()).contains("user=ada"));

    let slow = &spans["tools/call slow"];
    assert_eq!(
        slow.attr("crucible.mcp.cancel.source"),
        json!("notification")
    );
    assert_eq!(slow.attr("crucible.mcp.error.class"), json!("cancelled"));
    assert_eq!(slow.attr("error.type"), json!(null));
    let cancelled = &spans["notifications/cancelled"];
    assert_eq!(cancelled.attr("crucible.mcp.is_notification"), json!(true));
    assert_eq!(cancelled.attr("crucible.mcp.era"), json!("legacy"));
    crucible_sdk::shutdown().await.unwrap();
}

#[tokio::test]
async fn captured_content_is_sampled_scrubbed_and_capped() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;
    let instrumentation = Instrumentation::new(mcp::Options {
        capture: vec![
            CaptureRule {
                tool: "ech?".to_owned(),
                args: true,
                result: true,
                max_bytes: 64,
                ..CaptureRule::default()
            },
            CaptureRule {
                tool: "*".to_owned(),
                args: true,
                result: true,
                sample_rate: 1e-12,
                ..CaptureRule::default()
            },
        ],
        handle_params: BTreeMap::from([("echo".to_owned(), "account".to_owned())]),
        scrub: Some(Arc::new(|field, text| match field {
            Field::Result => None,
            _ => Some(text.replace("acct-1042", "[account]")),
        })),
        ..mcp::Options::default()
    })
    .unwrap();
    let client = connect(&instrumentation, modern()).await;
    let note = format!("mail ada@example.com about acct-1042 {}", "x".repeat(200));
    client
        .call_tool(call(
            "echo",
            json!({ "text": note, "account": "acct-1042" }),
        ))
        .await
        .unwrap();
    client
        .call_tool(call("fail", json!({ "secret": "s3cret" })))
        .await
        .unwrap();
    let spans = finish(&collector, client).await;

    let echo = &spans["tools/call echo"];
    assert_eq!(echo.attr("crucible.mcp.content.captured"), json!(true));
    assert_eq!(echo.attr("crucible.mcp.content.sample_rate"), json!(1.0));
    assert_eq!(
        echo.attr("crucible.mcp.content.truncation"),
        json!("cap_exceeded")
    );
    let arguments = echo.attr("gen_ai.tool.call.arguments");
    let arguments = arguments.as_str().unwrap();
    assert!(arguments.len() <= 64, "{arguments}");
    assert!(
        arguments.contains("[redacted:email]") || !arguments.contains('@'),
        "{arguments}"
    );
    assert!(!arguments.contains("acct-1042"), "{arguments}");
    // The hook dropped the result.
    assert_eq!(echo.attr("gen_ai.tool.call.result"), json!(null));
    assert_eq!(echo.attr("crucible.mcp.handle.id"), json!("[account]"));

    // The catch-all rule samples (almost) nothing.
    let fail = &spans["tools/call fail"];
    assert_eq!(fail.attr("crucible.mcp.content.captured"), json!(null));
    assert_eq!(fail.attr("gen_ai.tool.call.arguments"), json!(null));
    assert!(!format!("{:?}", collector.exports()).contains("s3cret"));
    crucible_sdk::shutdown().await.unwrap();
}

#[tokio::test]
async fn no_identity_suppresses_the_identity_keys() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;
    let instrumentation = Instrumentation::new(mcp::Options {
        identify: Some(Arc::new(|_| Identity {
            user_id: "u-7".to_owned(),
            ..Identity::default()
        })),
        no_identity: true,
        ..mcp::Options::default()
    })
    .unwrap();
    let client = connect(&instrumentation, legacy()).await;
    client
        .call_tool(call("echo", json!({ "text": "hi" })))
        .await
        .unwrap();
    let spans = finish(&collector, client).await;
    let echo = &spans["tools/call echo"];
    assert_eq!(echo.attr("user.id"), json!(null));
    assert_eq!(echo.attr("crucible.mcp.identity.source"), json!(null));
    crucible_sdk::shutdown().await.unwrap();
}

#[tokio::test]
async fn options_are_checked_and_uninitialized_servers_still_serve() {
    let _turn = GLOBAL.lock().await;
    let bad = Instrumentation::new(mcp::Options {
        capture: vec![CaptureRule {
            tool: "*".to_owned(),
            sample_rate: 1.5,
            ..CaptureRule::default()
        }],
        ..mcp::Options::default()
    })
    .unwrap_err();
    assert!(bad.to_string().contains("capture[0].sample_rate"), "{bad}");

    // Before init the wrapper records nothing and the handler answers as usual.
    let _stopped = crucible_sdk::shutdown().await;
    let instrumentation = Instrumentation::new(mcp::Options::default()).unwrap();
    let mut client = connect(&instrumentation, legacy()).await;
    let result = client
        .call_tool(call("echo", json!({ "text": "plain" })))
        .await
        .unwrap();
    assert_eq!(result.content[0].as_text().unwrap().text, "plain");
    client.close().await.unwrap();
}
