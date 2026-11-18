//! The request generator the properties share reaches the mapping, not only the residual path: over a fixed-seed
//! sample, every field of dialect 0's table receives a value and every observation-type rule yields a type.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use std::collections::BTreeSet;

use common::{ctx, frame, generate};
use crucible_compat::{DIALECT_0, Target};
use crucible_normalize::normalize;
use crucible_records::{ObservationType, SpanRecord};
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use prost::Message;

fn filled(target: Target, r: &SpanRecord) -> bool {
    let (c, p) = (&r.core, &r.payload);
    match target {
        Target::ServiceName => !c.service_name.is_empty(),
        Target::ServiceVersion => !c.service_version.is_empty(),
        Target::TelemetrySdkName => !c.telemetry_sdk_name.is_empty(),
        Target::TelemetrySdkLanguage => !c.telemetry_sdk_language.is_empty(),
        Target::TelemetrySdkVersion => !c.telemetry_sdk_version.is_empty(),
        Target::UserId => !c.user_id.is_empty(),
        Target::ModelRequested => !c.model_requested.is_empty(),
        Target::ModelResponse => !c.model_response.is_empty(),
        Target::Provider => !c.provider.is_empty(),
        Target::PromptName => !c.prompt_name.is_empty(),
        Target::FinishReasons => !c.finish_reasons.is_empty(),
        Target::Usage(key) => p.usage_provided.iter().any(|(k, _)| &**k == key),
        Target::Input => !p.input.is_empty(),
        Target::Output => !p.output.is_empty(),
        Target::SystemInstructions => !p.system_instructions.is_empty(),
        Target::ToolDefinitions => !p.tool_definitions.is_empty(),
    }
}

#[test]
fn generated_requests_reach_every_mapped_field_and_type_rule() {
    let mut runner = TestRunner::deterministic();
    let strategy = generate::request(false);
    let mut records = Vec::new();
    for _ in 0..300 {
        let request = strategy.new_tree(&mut runner).unwrap().current();
        let body = request.encode_to_vec();
        records.extend(normalize(&frame(&body, &[]), &ctx()).unwrap().spans);
    }
    assert!(records.len() > 500, "{} records", records.len());
    for rule in DIALECT_0.fields {
        assert!(
            records.iter().any(|r| filled(rule.target, r)),
            "{:?} never received a value",
            rule.target
        );
    }
    // Every type is seen; five of them only the explicit rule can yield.
    let types: BTreeSet<u8> = records
        .iter()
        .map(|r| r.core.observation_type.value())
        .collect();
    assert_eq!(types.len(), ObservationType::ALL.len());
    // The MCP rule's key keeps its value in the residual, so its yielding value is visible there; which rule wins
    // when several apply is `mapping.rs`'s concern.
    assert!(records.iter().any(|r| {
        let residual = core::str::from_utf8(&r.payload.attributes).unwrap_or_default();
        r.core.observation_type == ObservationType::Tool
            && residual.contains(r#""mcp.method.name":"tools/call""#)
    }));
}
