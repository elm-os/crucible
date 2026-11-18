//! The exporter and the agent helpers against a fake collector.

mod common;

use std::time::{Duration, Instant};

use common::{Collector, GLOBAL, Mode, SECRET_KEY, init};
use crucible_sdk::{
    Context, Cost, InitError, KeyValue, ObservationType, Options, Usage, observe, with_metadata,
    with_session, with_tags, with_user,
};
use serde_json::json;

#[tokio::test]
async fn spans_carry_the_context_and_the_agent_keys() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |o| {
        o.service_version = "1.4.0".to_owned();
        o.environment = "staging".to_owned();
        o.release = "2026.11".to_owned();
        o.deploy_id = "deploy-7".to_owned();
    })
    .await;

    let cx = with_session(&Context::new(), "conv-42");
    let cx = with_user(&cx, "u-7");
    let cx = with_tags(&cx, ["beta", "checkout"]);
    let cx = with_tags(&cx, ["beta", "eu"]);
    let cx = with_metadata(&cx, "plan", "pro");
    let cx = with_metadata(&cx, "seats", 3_i64);
    let cx = with_metadata(&cx, "seats", 4_i64);
    let run = observe(&cx, "answer", ObservationType::Agent);
    run.set_input("Where is my refund?");
    let generation = observe(&run.context(), "draft", ObservationType::Generation);
    generation.set_model("model-large");
    generation.set_input(&json!([{ "role": "user", "content": "hi" }]));
    generation.set_usage(Usage {
        input: 1200,
        output: 300,
        cache_read: 1000,
        ..Usage::default()
    });
    generation.set_cost(Cost {
        total: 0.0042,
        ..Cost::default()
    });
    generation.set_attribute(KeyValue::new("gen_ai.provider.name", "acme"));
    generation.end();
    run.set_output(&json!({ "answer": 5 }));
    run.set_error("the model refused");
    run.end();
    crucible_sdk::flush().await.unwrap();

    let answer = collector.span("answer");
    let draft = collector.span("draft");
    assert_eq!(draft.trace_id, answer.trace_id);
    assert_eq!(draft.parent_span_id, answer.span_id);
    assert_eq!(answer.parent_span_id, "");
    for span in [&answer, &draft] {
        assert_eq!(span.attr("crucible.session.id"), json!("conv-42"));
        assert_eq!(span.attr("crucible.user.id"), json!("u-7"));
        assert_eq!(
            span.attr("crucible.tags"),
            json!(["beta", "checkout", "eu"])
        );
        assert_eq!(span.attr("crucible.metadata.plan"), json!("pro"));
        assert_eq!(span.attr("crucible.metadata.seats"), json!(4));
        assert_eq!(span.attr("crucible.release"), json!("2026.11"));
        assert_eq!(span.resource["service.name"], json!("sdk-tests"));
        assert_eq!(span.resource["service.version"], json!("1.4.0"));
        assert_eq!(
            span.resource["deployment.environment.name"],
            json!("staging")
        );
        assert_eq!(span.resource["crucible.deploy.id"], json!("deploy-7"));
        let instance = span.resource["service.instance.id"].as_str().unwrap();
        assert_eq!(instance.len(), 36);
    }
    assert_eq!(answer.attr("crucible.observation.type"), json!("agent"));
    assert_eq!(answer.attr("crucible.input"), json!("Where is my refund?"));
    assert_eq!(answer.attr("crucible.output"), json!(r#"{"answer":5}"#));
    assert_eq!(answer.status_code, 2);
    assert_eq!(answer.status_message, "the model refused");
    assert_eq!(draft.attr("crucible.observation.type"), json!("generation"));
    assert_eq!(draft.attr("gen_ai.request.model"), json!("model-large"));
    let input: serde_json::Value =
        serde_json::from_str(draft.attr("crucible.input").as_str().unwrap()).unwrap();
    assert_eq!(input, json!([{ "role": "user", "content": "hi" }]));
    assert_eq!(draft.attr("gen_ai.usage.input_tokens"), json!(1200));
    assert_eq!(draft.attr("gen_ai.usage.output_tokens"), json!(300));
    assert_eq!(
        draft.attr("gen_ai.usage.cache_read.input_tokens"),
        json!(1000)
    );
    assert_eq!(
        draft.attr("gen_ai.usage.reasoning.output_tokens"),
        json!(null)
    );
    assert_eq!(draft.attr("crucible.cost.total"), json!(0.0042));
    assert_eq!(draft.attr("crucible.cost.input"), json!(null));
    assert_eq!(draft.attr("gen_ai.provider.name"), json!("acme"));

    for export in collector.exports() {
        assert_eq!(export.path, "/v1/traces");
        assert_eq!(export.headers["content-type"], "application/x-protobuf");
        assert_eq!(
            export.headers["authorization"],
            format!("Bearer {SECRET_KEY}").as_str()
        );
        let agent = export.headers["user-agent"].to_str().unwrap();
        assert!(agent.starts_with("crucible-sdk-rust/"), "{agent}");
    }
    // The key travels in the header only, never in a body.
    assert!(collector.wire_contains("conv-42"));
    assert!(!collector.wire_contains(SECRET_KEY));
    crucible_sdk::shutdown().await.unwrap();
}

#[tokio::test]
async fn flush_and_shutdown_export_what_ended_and_later_spans_are_dropped() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;

    observe(&Context::new(), "first", ObservationType::Span).end();
    // With a long flush interval, nothing leaves before flush.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(collector.exports().is_empty());
    crucible_sdk::flush().await.unwrap();
    assert_eq!(collector.spans().len(), 1);
    let open = observe(&Context::new(), "open at shutdown", ObservationType::Span);
    observe(&Context::new(), "second", ObservationType::Span).end();
    crucible_sdk::shutdown().await.unwrap();
    let names: Vec<String> = collector.spans().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["first", "second"]);

    // After shutdown nothing is recorded and nothing fails.
    open.end();
    let late = observe(&Context::new(), "late", ObservationType::Span);
    late.set_input("ignored");
    late.end();
    crucible_sdk::flush().await.unwrap();
    assert_eq!(crucible_sdk::stats(), crucible_sdk::Stats::default());
    assert_eq!(collector.spans().len(), 2);
}

#[tokio::test]
async fn ending_spans_never_waits_for_a_stalled_collector() {
    let _turn = GLOBAL.lock().await;
    let collector = Collector::start(Mode::Stall).await;
    init(&collector, |o| {
        o.queue_size = 4;
        o.flush_interval = Duration::from_millis(10);
        o.export_timeout = Duration::from_secs(2);
    })
    .await;

    // The worker takes the first span and stalls exporting it.
    observe(&Context::new(), "first", ObservationType::Span).end();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    for _ in 0..20 {
        observe(&Context::new(), "queued", ObservationType::Span).end();
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    let stats = crucible_sdk::stats();
    assert_eq!(stats.dropped, 16, "{stats:?}");
    let error = crucible_sdk::shutdown().await.unwrap_err().to_string();
    assert!(error.contains("export"), "{error}");
}

#[tokio::test]
async fn a_refused_export_is_reported_and_counted() {
    let _turn = GLOBAL.lock().await;
    let _stopped = crucible_sdk::shutdown().await;
    // Nothing listens on a port that was just released.
    let endpoint = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    crucible_sdk::init(Options {
        endpoint,
        secret_key: SECRET_KEY.to_owned(),
        export_timeout: Duration::from_secs(2),
        ..Options::default()
    })
    .unwrap();
    observe(&Context::new(), "lost", ObservationType::Span).end();
    let error = crucible_sdk::flush().await.unwrap_err().to_string();
    assert!(error.starts_with("crucible: the export"), "{error}");
    assert_eq!(crucible_sdk::stats().failed, 1);
    let _stopped = crucible_sdk::shutdown().await;
}

#[tokio::test]
async fn init_refusals_say_which_option_to_change() {
    let _turn = GLOBAL.lock().await;
    let _stopped = crucible_sdk::shutdown().await;
    let refuse = |endpoint: &str, key: &str| {
        crucible_sdk::init(Options {
            endpoint: endpoint.to_owned(),
            secret_key: key.to_owned(),
            ..Options::default()
        })
        .unwrap_err()
    };
    assert!(matches!(refuse("", SECRET_KEY), InitError::MissingEndpoint));
    let invalid = refuse("localhost:4318", SECRET_KEY);
    assert!(
        invalid.to_string().contains("http://localhost:4318"),
        "{invalid}"
    );
    assert!(matches!(
        refuse("ftp://h", SECRET_KEY),
        InitError::InvalidEndpoint(_)
    ));
    assert!(matches!(
        refuse("http://h", ""),
        InitError::MissingSecretKey
    ));
    let public = refuse("http://h", "cpk_0000");
    assert!(public.to_string().contains("csk_"), "{public}");

    let collector = Collector::start(Mode::Accept).await;
    init(&collector, |_| {}).await;
    let again = crucible_sdk::init(Options {
        endpoint: collector.endpoint.clone(),
        secret_key: SECRET_KEY.to_owned(),
        ..Options::default()
    })
    .unwrap_err();
    assert!(matches!(again, InitError::AlreadyInitialized));
    crucible_sdk::shutdown().await.unwrap();
    // Debug output never shows the key.
    let options = Options {
        secret_key: SECRET_KEY.to_owned(),
        ..Options::default()
    };
    assert!(!format!("{options:?}").contains(SECRET_KEY));
}
