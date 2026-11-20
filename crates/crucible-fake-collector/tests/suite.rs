//! The committed suite through the driver, in process: the self-test runner passes every case, each capability
//! switch and the seeded mutant fail exactly the cases they should, and a runner that emits nothing fails all.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use crucible_env::Env;
use crucible_env_prod::ProdConfig;
use crucible_fake_collector::golden::{self, CaseInput, Finding};
use crucible_fake_collector::spec::{self, Case};
use crucible_fake_collector::suite::{Kind, ProgressCount, Skip, ThrowOutcome, Variant};
use crucible_fake_collector::value::{Value, get};
use crucible_fake_collector::{Collector, Event, FeedbackPost, RecordedSpan};
use crucible_fake_collector::{
    Manifest, Mutant, Options, Report, RunOutcome, RunSpec, Runner, SelfTestOptions,
    SelfTestRunner, Status, Suite, mrtr_hash, run_suite, session_hash,
};
use crucible_records::{SpanId, TraceId};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::{Event as SpanEvent, Link};
use opentelemetry_proto::tonic::trace::v1::{
    ResourceSpans, ScopeSpans, Span, Status as ProtoStatus,
};
use prost::Message;
use rust_embed::RustEmbed;
use serde_json::{Value as Json, json};

#[derive(RustEmbed)]
#[folder = "../../conformance/"]
struct SuiteDir;

fn files() -> Vec<(String, Vec<u8>)> {
    SuiteDir::iter()
        .map(|path| {
            let file = SuiteDir::get(&path).unwrap();
            (path.into_owned(), file.data.into_owned())
        })
        .collect()
}

fn suite() -> Suite {
    Suite::from_files(files()).unwrap()
}

fn file(path: &str) -> Vec<u8> {
    SuiteDir::get(path).unwrap().data.into_owned()
}

fn manifest() -> Manifest {
    Manifest::parse("sdks/selftest.json", &file("sdks/selftest.json")).unwrap()
}

fn env() -> Env {
    crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap()
        .env
}

fn fast() -> Options {
    Options {
        settle: Duration::from_millis(20),
        settle_cap: Duration::from_millis(500),
        ..Options::default()
    }
}

async fn run(runner: &dyn Runner, manifest: &Manifest) -> Report {
    let env = env();
    run_suite(&env, &suite(), manifest, runner, &fast())
        .await
        .unwrap()
}

fn selftest(options: SelfTestOptions) -> SelfTestRunner {
    SelfTestRunner::new(env(), options).unwrap()
}

fn failing(report: &Report) -> BTreeSet<String> {
    report
        .results
        .iter()
        .filter(|r| r.status != Status::Pass)
        .map(|r| r.case.clone())
        .collect()
}

fn variants_of(scenario: &str) -> BTreeSet<String> {
    Variant::ALL
        .iter()
        .map(|v| format!("{scenario}@{}", v.name()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_self_test_passes_every_case() {
    let suite = suite();
    assert_eq!(suite.scenarios.len(), 15);
    assert_eq!(suite.goldens.len(), 15);
    let report = run(&selftest(SelfTestOptions::default()), &manifest()).await;
    assert_eq!(report.results.len(), 40);
    for r in &report.results {
        assert_eq!(r.status, Status::Pass, "{}: {:#?}", r.case, r.failures);
        assert!(r.checks >= 20, "{} ran only {} checks", r.case, r.checks);
    }
    assert!(report.passed());
    let markdown = report.to_markdown();
    assert!(markdown.contains("40 cases: 40 passed, 0 failed, 0 errors, 0 skipped"));
    assert_eq!(report.to_jsonl().lines().count(), 40);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_other_capability_choices_pass_when_the_manifest_declares_them() {
    let runner = selftest(SelfTestOptions {
        throw_outcome: ThrowOutcome::ToolError,
        progress_count: ProgressCount::Unobserved,
        mutant: None,
    });
    let manifest = Manifest {
        throw_outcome: ThrowOutcome::ToolError,
        progress_count: ProgressCount::Unobserved,
        ..manifest()
    };
    let report = run(&runner, &manifest).await;
    assert!(failing(&report).is_empty(), "{:#?}", report.results);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_misdeclared_capability_fails_exactly_its_cases() {
    // The runner models `internal` and `observed`; the manifest claims the opposite.
    let manifest = Manifest {
        throw_outcome: ThrowOutcome::ToolError,
        progress_count: ProgressCount::Unobserved,
        ..manifest()
    };
    let report = run(&selftest(SelfTestOptions::default()), &manifest).await;
    let mut expected = variants_of("mcp-throw");
    expected.extend(variants_of("mcp-notifications"));
    assert_eq!(failing(&report), expected);
    let throw = report
        .results
        .iter()
        .find(|r| r.case == "mcp-throw@modern-stdio")
        .unwrap();
    assert!(
        throw
            .failures
            .iter()
            .any(|f| f.check == "attribute error.type" && f.actual == "\"-32603\""),
        "{:#?}",
        throw.failures
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mutant_fails_exactly_the_secret_cases() {
    let mutant = Mutant::parse("unscrubbed-secret").unwrap();
    let report = run(
        &selftest(SelfTestOptions {
            mutant: Some(mutant),
            ..SelfTestOptions::default()
        }),
        &manifest(),
    )
    .await;
    assert_eq!(failing(&report), variants_of("mcp-secret"));
    for r in report.results.iter().filter(|r| r.status == Status::Fail) {
        let leaks: Vec<_> = r
            .failures
            .iter()
            .filter(|f| f.check.ends_with("never leaves the sender"))
            .collect();
        assert_eq!(leaks.len(), 9, "{}: {:#?}", r.case, r.failures);
        assert!(
            leaks
                .iter()
                .all(|f| f.actual.contains("gen_ai.tool.call.arguments"))
        );
    }
    assert!(Mutant::parse("no-such-mutant").is_none());
}

#[derive(Debug)]
struct Silent {
    exit_code: i32,
}

#[async_trait]
impl Runner for Silent {
    async fn run(&self, _spec: &RunSpec) -> RunOutcome {
        RunOutcome {
            exit_code: Some(self.exit_code),
            ..RunOutcome::default()
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_that_emits_nothing_fails_every_case() {
    let report = run(&Silent { exit_code: 0 }, &manifest()).await;
    assert_eq!(report.results.len(), 40);
    for r in &report.results {
        assert_eq!(r.status, Status::Fail, "{}", r.case);
        assert!(
            r.failures
                .iter()
                .any(|f| f.check == "at least one export arrived")
        );
    }
    let report = run(&Silent { exit_code: 3 }, &manifest()).await;
    assert!(report.results.iter().all(|r| r.status == Status::Error));
    assert!(report.to_markdown().contains("runner exited with Some(3)"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manifest_kinds_variants_and_skips_are_reported_as_skips() {
    let manifest = Manifest {
        kinds: vec![Kind::Mcp],
        variants: vec![Variant::ModernStdio],
        skips: vec![Skip {
            case: "mcp-slow".to_owned(),
            reason: "not yet".to_owned(),
        }],
        ..manifest()
    };
    manifest.check_against("test", &suite()).unwrap();
    let report = run(&selftest(SelfTestOptions::default()), &manifest).await;
    let ran: BTreeSet<&str> = report
        .results
        .iter()
        .filter(|r| r.status == Status::Pass)
        .map(|r| r.case.as_str())
        .collect();
    assert_eq!(ran.len(), 8);
    assert!(ran.iter().all(|c| c.ends_with("@modern-stdio")));
    let slow = report
        .results
        .iter()
        .find(|r| r.case == "mcp-slow@modern-stdio")
        .unwrap();
    assert_eq!(slow.status, Status::Skip);
    assert_eq!(slow.skip_reason.as_deref(), Some("declared skip: not yet"));
    assert!(report.passed());
    let unknown = Manifest {
        skips: vec![Skip {
            case: "mcp-nope".to_owned(),
            reason: "x".to_owned(),
        }],
        ..manifest
    };
    assert!(unknown.check_against("test", &suite()).is_err());
}

#[test]
fn the_registry_hash_vectors_hold() {
    let suite = suite();
    let hashes = &suite.registry.hashes;
    assert!(!hashes.session_id.vectors.is_empty() && !hashes.mrtr_state.vectors.is_empty());
    for v in &hashes.session_id.vectors {
        assert_eq!(session_hash(&v.input), v.output);
    }
    for v in &hashes.mrtr_state.vectors {
        assert_eq!(mrtr_hash(v.key.as_deref().unwrap(), &v.input), v.output);
    }
    // The self-test's observed values are the first vectors, so its expectations are the vector outputs.
    assert_eq!(
        session_hash(crucible_fake_collector::SESSION_ID),
        hashes.session_id.vectors[0].output
    );
    assert_eq!(
        mrtr_hash(
            &suite.constants.keys.secret_key,
            crucible_fake_collector::REQUEST_STATE
        ),
        hashes.mrtr_state.vectors[0].output
    );
}

#[test]
fn the_manifest_schema_lists_exactly_the_manifest_fields() {
    let schema: Json = serde_json::from_slice(&file("capability-manifest.schema.json")).unwrap();
    let properties: BTreeSet<String> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let required: BTreeSet<String> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let fields: BTreeSet<String> = serde_json::to_value(manifest())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(properties, fields);
    assert_eq!(required, fields);
    for (key, values) in [
        ("throw_outcome", ["tool_error", "internal"]),
        ("progress_count", ["observed", "unobserved"]),
    ] {
        assert_eq!(schema["properties"][key]["enum"], json!(values));
    }
}

#[test]
fn the_committed_scrub_rules_are_the_ones_built_in() {
    let suite = suite();
    let builtin = crucible_fake_collector::scrub::ScrubRules::builtin().unwrap();
    let shape = |rules: &crucible_fake_collector::scrub::ScrubRules| {
        rules
            .rules
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    r.pattern.as_str().to_owned(),
                    r.replacement.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&suite.scrub), shape(&builtin));
    assert_eq!(suite.scrub.rules.len(), 9);
}

fn resolved(suite: &Suite, case: &str) -> Vec<crucible_fake_collector::suite::Step> {
    let cases = suite.cases();
    let case: &Case<'_> = cases.iter().find(|c| c.id == case).unwrap();
    spec::resolve_steps(&suite.constants, case).unwrap()
}

#[test]
fn cases_resolve_to_each_revisions_wire_rules() {
    let suite = suite();
    let legacy = resolved(&suite, "mcp-echo@legacy-http");
    assert_eq!(legacy[0].step, "initialize");
    assert_eq!(legacy[0].send.as_ref().unwrap()["id"], json!(0));
    assert!(legacy[0].headers.is_empty());
    assert_eq!(
        legacy[1].send.as_ref().unwrap()["method"],
        json!("notifications/initialized")
    );
    assert_eq!(legacy[2].headers["MCP-Protocol-Version"], "2025-11-25");
    assert!(
        legacy[2]
            .send
            .as_ref()
            .unwrap()
            .pointer("/params/_meta")
            .is_none()
    );

    let modern = resolved(&suite, "mcp-echo@modern-http");
    let call = modern.iter().find(|s| s.step == "echo-traced").unwrap();
    let message = call.send.as_ref().unwrap();
    let meta = &message["params"]["_meta"];
    assert_eq!(
        meta["io.modelcontextprotocol/protocolVersion"],
        json!("2026-07-28")
    );
    assert_eq!(
        meta["io.modelcontextprotocol/clientInfo"]["name"],
        json!("crucible-conformance-client")
    );
    assert_eq!(
        meta["traceparent"],
        json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
    );
    assert_eq!(call.headers["Mcp-Method"], "tools/call");
    assert_eq!(call.headers["Mcp-Name"], "echo");
    assert_eq!(call.headers["MCP-Protocol-Version"], "2026-07-28");
    assert!(
        resolved(&suite, "mcp-echo@modern-stdio")
            .iter()
            .all(|s| s.headers.is_empty())
    );

    let closed = resolved(&suite, "mcp-cancel@modern-http");
    assert!(closed.iter().any(|s| s.close.as_deref() == Some("slow")));
    for case in [
        "mcp-cancel@legacy-stdio",
        "mcp-cancel@legacy-http",
        "mcp-cancel@modern-stdio",
    ] {
        let steps = resolved(&suite, case);
        let cancel = steps.iter().find(|s| s.step == "cancel").unwrap();
        let message = cancel.send.as_ref().unwrap();
        assert_eq!(
            message["method"],
            json!("notifications/cancelled"),
            "{case}"
        );
        assert_eq!(message["params"]["requestId"], json!(2), "{case}");
    }
    assert!(
        !resolved(&suite, "mcp-notifications@modern-stdio")
            .iter()
            .any(|s| s.step == "roots-changed")
    );
    assert!(
        resolved(&suite, "mcp-notifications@legacy-stdio")
            .iter()
            .any(|s| s.step == "roots-changed")
    );
    let agent = resolved(&suite, "agent-tool");
    assert!(agent.last().unwrap().shutdown.is_some());
}

/// Replaces one scenario file and returns the loader's error.
fn load_with(path: &str, scenario: Json) -> String {
    let files = files().into_iter().map(|(p, bytes)| {
        if p == path {
            (p, serde_json::to_vec(&scenario).unwrap())
        } else {
            (p, bytes)
        }
    });
    Suite::from_files(files).unwrap_err().to_string()
}

#[test]
fn the_loader_refuses_cases_that_break_the_wire_rules() {
    let mcp = |steps: Json| {
        json!({
            "schema": "crucible.conformance.scenario/v1",
            "id": "mcp-echo",
            "kind": "mcp",
            "title": "t",
            "variants": ["legacy-stdio", "modern-stdio"],
            "steps": steps,
        })
    };
    let call = |id: u64| json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": "echo", "arguments": {} } });
    let cases = [
        (
            json!([{ "step": "a", "send": call(1) }, { "step": "a", "send": call(2) }]),
            "used twice",
        ),
        (
            json!([{ "step": "a", "send": call(1) }, { "step": "b", "send": call(1) }]),
            "reuses JSON-RPC id",
        ),
        (json!([{ "step": "a", "send": call(0) }]), "id 0 belongs"),
        (
            json!([{ "step": "a", "send": call(1) }, { "step": "b", "await": "a" }]),
            "not an earlier request in flight",
        ),
        (
            json!([{ "step": "a", "send": call(1), "wait": false }, { "step": "b", "close": "a" }]),
            "cancel",
        ),
        (
            json!([{ "step": "a", "cancel": "z" }]),
            "not an earlier request",
        ),
        (
            json!([{ "step": "a", "send": { "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "x", "requestState": "raw" } } }]),
            "requestState must echo",
        ),
        (
            json!([{ "step": "a", "send": { "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "x", "y": { "$from": "later/result" } } } }]),
            "not an earlier awaited request",
        ),
        (
            json!([{ "step": "a", "observe": { "name": "n", "type": "span" } }]),
            "MCP scenarios use",
        ),
        (
            json!([{ "step": "a", "send": { "jsonrpc": "1.0", "id": 1, "method": "m" } }]),
            "jsonrpc",
        ),
    ];
    for (steps, needle) in cases {
        let error = load_with("scenarios/mcp-echo.json", mcp(steps.clone()));
        assert!(error.contains(needle), "{steps}: {error}");
    }
    let agent = json!({
        "schema": "crucible.conformance.scenario/v1",
        "id": "agent-tool",
        "kind": "agent",
        "title": "t",
        "steps": [{ "step": "a", "observe": { "name": "n", "type": "widget" } }],
    });
    assert!(load_with("scenarios/agent-tool.json", agent).contains("not an observation type"));
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let feedback = |score: Json| {
        json!({
            "schema": "crucible.conformance.scenario/v1",
            "id": "agent-feedback",
            "kind": "agent",
            "title": "t",
            "steps": [
                { "step": "answer", "observe": { "name": "answer", "type": "agent" } },
                { "step": "a", "feedback": score },
                { "step": "b", "feedback": { "trace_id": trace, "name": "x", "value": 1 } },
                { "step": "c", "feedback": { "trace_id": trace, "name": "y", "value": 1 } },
            ],
        })
    };
    for (score, needle) in [
        (
            json!({ "trace_id": trace, "name": "a#b", "value": 1 }),
            "without '#'",
        ),
        (json!({ "name": "n", "value": 1 }), "names one target"),
        (
            json!({ "span_id": "00f067aa0ba902b7", "name": "n", "value": 1 }),
            "names one target",
        ),
        (
            json!({ "trace_id": "0".repeat(32), "name": "n", "value": 1 }),
            "names one target",
        ),
        (
            json!({ "trace_id": trace, "session_id": "s", "name": "n", "value": 1 }),
            "names one target",
        ),
        (
            json!({ "trace_id": trace, "name": "n", "value": "x", "data_type": "numeric" }),
            "takes a number",
        ),
        (
            json!({ "trace_id": trace, "name": "n", "value": [1] }),
            "takes a number",
        ),
        (
            json!({ "trace_id": trace, "name": "n", "value": 1, "metadata": [1] }),
            "metadata is a JSON object",
        ),
        (
            json!({ "trace_id": trace, "name": "n", "value": 1, "idempotency_key": "a b" }),
            "idempotency_key is 1 to 128",
        ),
    ] {
        let error = load_with("scenarios/agent-feedback.json", feedback(score.clone()));
        assert!(error.contains(needle), "{score}: {error}");
    }
    let unknown = json!({ "trace_id": trace, "name": "n", "value": 1, "user": "u" });
    assert!(
        load_with("scenarios/agent-feedback.json", feedback(unknown)).contains("unknown field")
    );
    // An MCP scenario has no feedback step, even when its golden expects the post.
    let mut golden: Json = serde_json::from_slice(&file("goldens/mcp-echo.json")).unwrap();
    golden["feedback"] = json!([{ "id": "a", "idempotency_key": "k", "body": {} }]);
    let mcp_feedback =
        mcp(json!([{ "step": "a", "feedback": { "trace_id": trace, "name": "n", "value": 1 } }]));
    let files = files().into_iter().map(|(p, bytes)| match p.as_str() {
        "scenarios/mcp-echo.json" => (p, serde_json::to_vec(&mcp_feedback).unwrap()),
        "goldens/mcp-echo.json" => (p, serde_json::to_vec(&golden).unwrap()),
        _ => (p, bytes),
    });
    let error = Suite::from_files(files).unwrap_err().to_string();
    assert!(error.contains("MCP scenarios use"), "{error}");
}

#[test]
fn a_golden_lists_one_feedback_post_per_feedback_step() {
    let golden: Json = serde_json::from_slice(&file("goldens/agent-feedback.json")).unwrap();
    let with = |golden: Json| {
        let files = files().into_iter().map(|(p, bytes)| {
            if p == "goldens/agent-feedback.json" {
                (p, serde_json::to_vec(&golden).unwrap())
            } else {
                (p, bytes)
            }
        });
        Suite::from_files(files).unwrap_err().to_string()
    };
    let mut fewer = golden.clone();
    fewer["feedback"].as_array_mut().unwrap().pop();
    assert!(with(fewer).contains("feedback lists 2 posts; the scenario has 3 feedback steps"));
    let mut unknown = golden.clone();
    unknown["feedback"][0]["after"] = json!(["nope"]);
    assert!(with(unknown).contains("after names unknown span \"nope\""));
    let mut bad_key = golden;
    bad_key["feedback"][1]["idempotency_key"] = json!({ "present": true });
    assert!(with(bad_key).contains("idempotency_key is a string or"));
}

#[test]
fn every_case_has_a_run_spec_a_runner_can_parse() {
    let suite = suite();
    let endpoints = spec::Endpoints {
        endpoint: "http://127.0.0.1:1".to_owned(),
        traces_url: "http://127.0.0.1:1/v1/traces".to_owned(),
        checkpoint_url: "http://127.0.0.1:1/conformance/checkpoint".to_owned(),
        api_url: "http://127.0.0.1:1".to_owned(),
    };
    let cases = suite.cases();
    assert_eq!(cases.len(), 40);
    for case in &cases {
        let run = spec::run_spec(&suite.constants, case, &endpoints).unwrap();
        let text = serde_json::to_string(&run).unwrap();
        let back: RunSpec = serde_json::from_str(&text).unwrap();
        assert_eq!(back, run);
        assert_eq!(run.schema, "crucible.conformance.run/v1");
        assert_eq!(run.server.is_some(), case.scenario.kind == Kind::Mcp);
        assert_eq!(run.project_id, suite.constants.project_id);
        assert_eq!(run.api_url, "http://127.0.0.1:1");
    }
}

/// Plays one case with the self-test runner against its own collector; returns the log and the responses.
async fn record(
    suite: &Suite,
    case_id: &str,
) -> (Vec<Event>, Vec<crucible_fake_collector::ResponseEvent>) {
    let env = env();
    let collector = Collector::start(&env).await.unwrap();
    let endpoints = spec::Endpoints {
        endpoint: collector.endpoint(),
        traces_url: collector.traces_url(),
        checkpoint_url: collector.checkpoint_url(),
        api_url: collector.endpoint(),
    };
    let cases = suite.cases();
    let case = cases.iter().find(|c| c.id == case_id).unwrap();
    let run = spec::run_spec(&suite.constants, case, &endpoints).unwrap();
    let responses = selftest(SelfTestOptions::default())
        .play(&run)
        .await
        .unwrap();
    (collector.take(), responses)
}

fn failures_of(
    suite: &Suite,
    case_id: &str,
    events: &[Event],
    responses: &[crucible_fake_collector::ResponseEvent],
) -> Vec<Finding> {
    let cases = suite.cases();
    let case = cases.iter().find(|c| c.id == case_id).unwrap();
    let manifest = manifest();
    golden::evaluate(&CaseInput {
        suite,
        case,
        manifest: &manifest,
        events,
        responses,
    })
    .failures
}

fn feedback_mut(events: &mut [Event]) -> impl Iterator<Item = &mut FeedbackPost> {
    events.iter_mut().filter_map(|e| match e {
        Event::Feedback(post) => Some(post),
        _ => None,
    })
}

fn exports_mut(events: &mut [Event]) -> impl Iterator<Item = &mut crucible_fake_collector::Export> {
    events.iter_mut().filter_map(|e| match e {
        Event::Export(x) => Some(x),
        Event::Checkpoint(_) | Event::Feedback(_) => None,
    })
}

fn span_mut(events: &mut [Event], pick: impl Fn(&RecordedSpan) -> bool) -> &mut RecordedSpan {
    exports_mut(events)
        .flat_map(|x| x.spans.iter_mut())
        .find(|s| pick(s))
        .unwrap()
}

fn has(span: &RecordedSpan, key: &str, value: &str) -> bool {
    get(&span.attributes, key).and_then(Value::as_str) == Some(value)
}

fn set(span: &mut RecordedSpan, key: &str, value: Value) {
    span.attributes.retain(|(k, _)| k != key);
    span.attributes.push((key.to_owned(), value));
}

fn any_value(value: &Value) -> AnyValue {
    use any_value::Value as V;
    AnyValue {
        value: match value {
            Value::Empty => None,
            Value::String(s) => Some(V::StringValue(s.clone())),
            Value::Bool(b) => Some(V::BoolValue(*b)),
            Value::Int(i) => Some(V::IntValue(*i)),
            Value::Double(d) => Some(V::DoubleValue(*d)),
            Value::Bytes(b) => Some(V::BytesValue(b.clone())),
            Value::Array(items) => Some(V::ArrayValue(ArrayValue {
                values: items.iter().map(any_value).collect(),
            })),
            Value::KvList(pairs) => Some(V::KvlistValue(KeyValueList {
                values: key_values(pairs),
            })),
        },
    }
}

fn key_values(pairs: &[(String, Value)]) -> Vec<KeyValue> {
    pairs
        .iter()
        .map(|(key, value)| KeyValue {
            key: key.clone(),
            value: Some(any_value(value)),
            ..KeyValue::default()
        })
        .collect()
}

/// Writes the body of every export again from its recorded spans, so that a mutation of a recorded span reaches
/// what the normalizer reads. A collector's log always holds the two in step.
fn reencode(events: &mut [Event]) {
    let nanos = |t: crucible_records::Timestamp| t.micros() as u64 * 1_000;
    for export in exports_mut(events) {
        let resource_spans = export
            .spans
            .iter()
            .map(|span| ResourceSpans {
                resource: Some(Resource {
                    attributes: key_values(&span.resource),
                    ..Resource::default()
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: span.scope_name.clone(),
                        version: span.scope_version.clone(),
                        attributes: key_values(&span.scope_attributes),
                        ..InstrumentationScope::default()
                    }),
                    spans: vec![Span {
                        trace_id: span.trace_id.to_bytes().to_vec(),
                        span_id: span.span_id.to_bytes().to_vec(),
                        parent_span_id: span
                            .parent_span_id
                            .map(|id| id.to_bytes().to_vec())
                            .unwrap_or_default(),
                        name: span.name.clone(),
                        kind: i32::from(span.kind.value()),
                        start_time_unix_nano: nanos(span.start),
                        end_time_unix_nano: nanos(span.end),
                        attributes: key_values(&span.attributes),
                        events: span
                            .events
                            .iter()
                            .map(|event| SpanEvent {
                                time_unix_nano: nanos(event.time),
                                name: event.name.clone(),
                                attributes: key_values(&event.attributes),
                                ..SpanEvent::default()
                            })
                            .collect(),
                        links: span
                            .links
                            .iter()
                            .map(|link| Link {
                                trace_id: link.trace_id.to_bytes().to_vec(),
                                span_id: link.span_id.to_bytes().to_vec(),
                                attributes: key_values(&link.attributes),
                                ..Link::default()
                            })
                            .collect(),
                        status: Some(ProtoStatus {
                            message: span.status_message.clone(),
                            code: i32::from(span.status.value()),
                        }),
                        ..Span::default()
                    }],
                    ..ScopeSpans::default()
                }],
                ..ResourceSpans::default()
            })
            .collect();
        export.body = ExportTraceServiceRequest { resource_spans }.encode_to_vec();
    }
}

type Mutation = fn(&mut Vec<Event>);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_check_class_fails_when_its_rule_is_broken() {
    let suite = suite();
    let cases: [(&str, &str, Mutation); 20] = [
        ("mcp-echo@modern-stdio", "exactly one span with", |events| {
            let export = exports_mut(events).next().unwrap();
            let mut copy = export.spans[0].clone();
            copy.span_id = SpanId::from_bytes([7; 8]);
            export.spans.push(copy);
        }),
        ("agent-nested-spans", "child of \"plan\"", |events| {
            let checkout = span_mut(events, |s| s.name == "checkout").span_id;
            span_mut(events, |s| s.name == "fetch-docs").parent_span_id = Some(checkout);
        }),
        (
            "mcp-echo@legacy-http",
            "attribute mcp.session.id",
            |events| {
                let raw = Value::String(crucible_fake_collector::SESSION_ID.to_owned());
                set(
                    span_mut(events, |s| has(s, "jsonrpc.request.id", "2")),
                    "mcp.session.id",
                    raw,
                );
            },
        ),
        (
            "mcp-needs-input@modern-http",
            "attribute crucible.mcp.mrtr.state_out",
            |events| {
                let wrong = Value::String("0".repeat(32));
                set(
                    span_mut(events, |s| has(s, "jsonrpc.request.id", "2")),
                    "crucible.mcp.mrtr.state_out",
                    wrong,
                );
            },
        ),
        (
            "agent-flush-shutdown",
            "arrived before checkpoint",
            |events| {
                let at = events
                    .iter()
                    .position(|e| matches!(e, Event::Checkpoint(_)))
                    .unwrap();
                let checkpoint = events.remove(at);
                events.insert(0, checkpoint);
            },
        ),
        ("mcp-fail@legacy-stdio", "no forbidden keys", |events| {
            let address = Value::String("203.0.113.9".to_owned());
            set(
                span_mut(events, |s| has(s, "jsonrpc.request.id", "2")),
                "client.address",
                address,
            );
        }),
        ("mcp-slow@modern-stdio", "registry types", |events| {
            set(
                span_mut(events, |s| has(s, "jsonrpc.request.id", "2")),
                "crucible.mcp.era",
                Value::Int(2),
            );
        }),
        ("mcp-lookup-order@legacy-stdio", "Authorization", |events| {
            exports_mut(events).next().unwrap().authorization = Some("Bearer csk_wrong".to_owned());
        }),
        ("agent-tool", "Content-Type", |events| {
            exports_mut(events).next().unwrap().content_type = Some("application/json".to_owned());
        }),
        ("mcp-fail@modern-http", "outcome status", |events| {
            let span = span_mut(events, |s| has(s, "jsonrpc.request.id", "2"));
            span.attributes
                .retain(|(k, _)| k != "error.type" && k != "crucible.mcp.error.class");
        }),
        ("agent-context", "field session_id", |events| {
            let span = span_mut(events, |s| s.name == "turn-2");
            span.attributes.retain(|(k, _)| k != "crucible.session.id");
        }),
        ("mcp-slow@legacy-stdio", "duration", |events| {
            let span = span_mut(events, |s| has(s, "jsonrpc.request.id", "2"));
            span.end = span.start;
        }),
        (
            "mcp-echo@modern-http",
            "trace id from the propagated traceparent",
            |events| {
                span_mut(events, |s| has(s, "jsonrpc.request.id", "3")).trace_id =
                    TraceId::from_bytes([9; 16]);
            },
        ),
        ("agent-feedback", "posts to the feedback route", |events| {
            let last = events
                .iter()
                .rposition(|e| matches!(e, Event::Feedback(_)))
                .unwrap();
            events.remove(last);
        }),
        ("agent-feedback", "the project's feedback route", |events| {
            feedback_mut(events).next().unwrap().path =
                "/api/v1/projects/01938a2e-0000-7000-8000-000000000000/feedback".to_owned();
        }),
        ("agent-feedback", "feedback Authorization", |events| {
            feedback_mut(events).next().unwrap().authorization =
                Some("Bearer cpk_conformance00000000000000000000000000000000".to_owned());
        }),
        ("agent-feedback", "Idempotency-Key", |events| {
            // A retried post must keep its key; two scores sharing one is a defect.
            let first = feedback_mut(events).nth(1).unwrap().idempotency_key.clone();
            feedback_mut(events).nth(2).unwrap().idempotency_key = first;
        }),
        ("agent-feedback", "Idempotency-Key", |events| {
            feedback_mut(events).next().unwrap().idempotency_key = None;
        }),
        ("agent-feedback", "feedback body", |events| {
            // camelCase field names would be refused by the route.
            feedback_mut(events).nth(2).unwrap().body =
                br#"{"sessionId":"conformance-session","name":"resolved","value":true}"#.to_vec();
        }),
        ("agent-feedback", "posted after the span", |events| {
            let at = events
                .iter()
                .position(|e| matches!(e, Event::Export(_)))
                .unwrap();
            let export = events.remove(at);
            events.push(export);
        }),
    ];
    for (case, check, mutate) in cases {
        let (events, responses) = record(&suite, case).await;
        let clean = failures_of(&suite, case, &events, &responses);
        assert!(clean.is_empty(), "{case} must pass unmodified: {clean:#?}");
        let mut broken = events.clone();
        mutate(&mut broken);
        reencode(&mut broken);
        let failures = failures_of(&suite, case, &broken, &responses);
        assert!(
            failures.iter().any(|f| f.check.starts_with(check)),
            "{case}: no {check:?} failure in {failures:#?}"
        );
    }
}
