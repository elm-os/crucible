//! Agent cases: the runner is the agent program and calls the SDK in process.

use std::collections::BTreeMap;
use std::time::Duration;

use crucible_sdk::{
    Context, Cost, KeyValue, ObservationType, Usage, Value as AttributeValue, observe,
    with_metadata, with_session, with_tags, with_user,
};
use opentelemetry::{Array, StringValue};
use serde::Deserialize;
use serde_json::Value;

use crate::RunSpec;

/// One span of an agent case and the fields set on it.
#[derive(Debug, Deserialize)]
pub(crate) struct Node {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    trace_name: Option<String>,
    context: Option<NodeContext>,
    input: Option<Value>,
    output: Option<Value>,
    model: Option<String>,
    provider: Option<String>,
    input_messages: Option<Value>,
    output_messages: Option<Value>,
    finish_reasons: Option<Vec<String>>,
    #[serde(default)]
    usage: BTreeMap<String, u64>,
    #[serde(default)]
    cost_usd: BTreeMap<String, String>,
    #[serde(default)]
    children: Vec<Node>,
}

#[derive(Debug, Deserialize)]
struct NodeContext {
    session_id: Option<String>,
    user_id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    metadata: BTreeMap<String, Value>,
}

pub(crate) async fn run(spec: &RunSpec) -> Result<(), String> {
    crate::init_sdk(spec)?;
    let mut shut_down = false;
    for step in &spec.steps {
        let fail = |e: String| format!("step {}: {e}", step.step);
        if let Some(node) = &step.observe {
            observe_node(&Context::new(), node).map_err(fail)?;
        } else if step.flush.is_some() {
            crucible_sdk::flush()
                .await
                .map_err(|e| fail(e.to_string()))?;
        } else if let Some(name) = &step.checkpoint {
            checkpoint(&spec.checkpoint_url, name).await.map_err(fail)?;
        } else if step.shutdown.is_some() {
            crucible_sdk::shutdown()
                .await
                .map_err(|e| fail(e.to_string()))?;
            shut_down = true;
        } else if let Some(ms) = step.sleep_ms {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        } else {
            return Err(fail("not an agent step".to_owned()));
        }
    }
    if !shut_down {
        crucible_sdk::shutdown().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Starts a span for `node` with the SDK's helpers, runs its children inside it and ends it. Keys the SDK has
/// no setter for (trace name, provider, messages, finish reasons) go through `set_attribute`.
fn observe_node(parent: &Context, node: &Node) -> Result<(), String> {
    let mut cx = parent.clone();
    if let Some(c) = &node.context {
        if let Some(session) = &c.session_id {
            cx = with_session(&cx, session.clone());
        }
        if let Some(user) = &c.user_id {
            cx = with_user(&cx, user.clone());
        }
        cx = with_tags(&cx, c.tags.iter().cloned());
        for (key, value) in &c.metadata {
            cx = with_metadata(&cx, key, scalar(value)?);
        }
    }
    let observation = observe(&cx, node.name.clone(), observation_type(&node.kind)?);
    if let Some(name) = &node.trace_name {
        observation.set_attribute(KeyValue::new("crucible.trace.name", name.clone()));
    }
    if let Some(input) = &node.input {
        observation.set_input(input);
    }
    if let Some(output) = &node.output {
        observation.set_output(output);
    }
    if let Some(model) = &node.model {
        observation.set_model(model.clone());
    }
    if let Some(provider) = &node.provider {
        observation.set_attribute(KeyValue::new("gen_ai.provider.name", provider.clone()));
    }
    if let Some(messages) = &node.input_messages {
        observation.set_attribute(KeyValue::new("gen_ai.input.messages", messages.to_string()));
    }
    if let Some(messages) = &node.output_messages {
        observation.set_attribute(KeyValue::new(
            "gen_ai.output.messages",
            messages.to_string(),
        ));
    }
    if let Some(reasons) = &node.finish_reasons {
        let reasons = reasons.iter().cloned().map(StringValue::from).collect();
        observation.set_attribute(KeyValue::new(
            "gen_ai.response.finish_reasons",
            AttributeValue::Array(Array::String(reasons)),
        ));
    }
    if !node.usage.is_empty() {
        observation.set_usage(usage(&node.usage)?);
    }
    if !node.cost_usd.is_empty() {
        observation.set_cost(cost(&node.cost_usd)?);
    }
    for child in &node.children {
        observe_node(&observation.context(), child)?;
    }
    observation.end();
    Ok(())
}

fn observation_type(name: &str) -> Result<ObservationType, String> {
    Ok(match name {
        "span" => ObservationType::Span,
        "generation" => ObservationType::Generation,
        "event" => ObservationType::Event,
        "embedding" => ObservationType::Embedding,
        "agent" => ObservationType::Agent,
        "tool" => ObservationType::Tool,
        "chain" => ObservationType::Chain,
        "retriever" => ObservationType::Retriever,
        "evaluator" => ObservationType::Evaluator,
        "guardrail" => ObservationType::Guardrail,
        other => return Err(format!("unknown observation type {other:?}")),
    })
}

/// A metadata value: a string, a bool, an integer or a double.
fn scalar(value: &Value) -> Result<AttributeValue, String> {
    Ok(match value {
        Value::String(s) => AttributeValue::from(s.clone()),
        Value::Bool(b) => AttributeValue::from(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => AttributeValue::from(i),
            None => AttributeValue::from(n.as_f64().ok_or("a metadata number is out of range")?),
        },
        other => return Err(format!("metadata value {other} is not a scalar")),
    })
}

fn usage(usage: &BTreeMap<String, u64>) -> Result<Usage, String> {
    let mut out = Usage::default();
    for (key, count) in usage {
        let field = match key.as_str() {
            "input_tokens" => &mut out.input,
            "output_tokens" => &mut out.output,
            "cache_read.input_tokens" => &mut out.cache_read,
            "cache_creation.input_tokens" => &mut out.cache_write,
            "reasoning.output_tokens" => &mut out.reasoning,
            other => return Err(format!("usage key {other:?} has no field")),
        };
        *field = *count;
    }
    Ok(out)
}

fn cost(usd: &BTreeMap<String, String>) -> Result<Cost, String> {
    let mut out = Cost::default();
    for (key, value) in usd {
        let field = match key.as_str() {
            "total" => &mut out.total,
            "input" => &mut out.input,
            "output" => &mut out.output,
            "cache_read" => &mut out.cache_read,
            "cache_write" => &mut out.cache_write,
            "reasoning" => &mut out.reasoning,
            other => return Err(format!("cost key {other:?} has no field")),
        };
        *field = value
            .parse()
            .map_err(|e| format!("cost {key} {value:?}: {e}"))?;
    }
    Ok(out)
}

/// Asks the collector to order a named checkpoint against the exports so far.
async fn checkpoint(url: &str, name: &str) -> Result<(), String> {
    let body = serde_json::json!({ "name": name }).to_string();
    let response = crate::mcp::post(
        url,
        &[("content-type".to_owned(), "application/json".to_owned())],
        body.into_bytes(),
        None,
    )
    .await?;
    if response.http_status != Some(204) {
        return Err(format!(
            "checkpoint {name}: status {:?}",
            response.http_status
        ));
    }
    Ok(())
}
