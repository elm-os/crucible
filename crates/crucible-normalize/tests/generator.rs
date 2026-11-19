//! The request generator the properties share reaches the mapping, not only the residual path: over a fixed-seed
//! sample, every field of every dialect layer receives a value, every dialect is chosen, every observation type is
//! seen, and usage is priced from the bundled table.

// Test code throughout: lets the helpers below use `unwrap` under the workspace lints.
#![cfg(test)]

mod common;

use std::collections::BTreeSet;

use common::{ctx, frame, generate};
use crucible_compat::{LAYERS, Target};
use crucible_normalize::normalize;
use crucible_records::{CostSource, Level, ObservationType, SpanRecord, TracePublic};
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
        Target::SessionId => !c.session_id.is_empty(),
        Target::TraceName => !c.trace_name.is_empty(),
        // Every record has an environment: the default stands in for an absent one.
        Target::Environment => &*c.environment != "default",
        Target::Release => !c.release.is_empty(),
        Target::Version => !c.version.is_empty(),
        Target::Tags => !c.tags.is_empty(),
        Target::TraceTags => !c.trace_tags.is_empty(),
        Target::TracePublic => c.trace_public != TracePublic::Unset,
        Target::Level => matches!(c.level, Level::Debug | Level::Warning),
        Target::StatusMessage => !c.status_message.is_empty(),
        Target::PromptVersion => !c.prompt_version.is_empty(),
        Target::CompletionStartTime => c.completion_start_time.is_some(),
        Target::Metadata => !p.metadata.is_empty() && !p.metadata_kv.is_empty(),
        Target::TraceMetadata => !p.trace_metadata.is_empty(),
        Target::ModelParameters => !p.model_parameters.is_empty(),
        Target::Cost(key) => p.cost_provided.iter().any(|(k, _)| &**k == key),
        Target::CostAll => !p.cost_provided.is_empty(),
        Target::ModelRequested => !c.model_requested.is_empty(),
        Target::ModelResponse => !c.model_response.is_empty(),
        Target::Provider => !c.provider.is_empty(),
        Target::PromptName => !c.prompt_name.is_empty(),
        Target::FinishReasons => !c.finish_reasons.is_empty(),
        Target::Usage(key) => p.usage_provided.iter().any(|(k, _)| &**k == key),
        Target::UsageAll => !p.usage_provided.is_empty(),
        Target::Input => !p.input.is_empty(),
        Target::Output => !p.output.is_empty(),
        Target::SystemInstructions => !p.system_instructions.is_empty(),
        Target::ToolDefinitions => !p.tool_definitions.is_empty(),
        Target::ToolCalls => !p.tool_calls.is_empty() && c.tool_call_count > 0,
        Target::ToolNames => !c.tool_names.is_empty(),
        // A root flag shows on a span that has a parent.
        Target::IsRoot => c.is_root && c.parent_span_id != crucible_records::SpanId::ZERO,
        Target::IsAppRoot => c.is_app_root,
    }
}

#[test]
fn generated_requests_reach_every_mapped_field_and_type_rule() {
    let mut runner = TestRunner::deterministic();
    let strategy = generate::request(false);
    let mut records = Vec::new();
    for _ in 0..5_000 {
        let request = strategy.new_tree(&mut runner).unwrap().current();
        let body = request.encode_to_vec();
        records.extend(normalize(&frame(&body, &[]), &ctx()).unwrap().spans);
    }
    assert!(records.len() > 500, "{} records", records.len());
    for rule in LAYERS.iter().flat_map(|table| table.fields) {
        assert!(
            records.iter().any(|r| filled(rule.target, r)),
            "{:?} never received a value",
            rule.target
        );
    }
    // Every dialect is chosen for some span.
    let dialects: BTreeSet<u8> = records.iter().map(|r| r.core.compat_dialect).collect();
    assert_eq!(dialects.len(), crucible_compat::TABLES.len());
    // Every type is seen; five of them only the explicit rule can yield.
    let types: BTreeSet<u8> = records
        .iter()
        .map(|r| r.core.observation_type.value())
        .collect();
    assert_eq!(types.len(), ObservationType::ALL.len());
    assert!(
        records
            .iter()
            .any(|r| r.core.cost_source == CostSource::Calculated
                && r.core.total_cost_nusd.nanos() > 0),
        "no record has a calculated cost"
    );
    // The MCP rule's key makes the span an MCP operation; which rule wins when several apply is `mapping.rs`'s
    // concern.
    assert!(records.iter().any(|r| {
        r.core.observation_type == ObservationType::Tool
            && r.core.mcp_side != crucible_records::McpSide::None
    }));
}
