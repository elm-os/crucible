//! Agent helpers: context setters that put the session, user, tags and metadata on every span started under a
//! context, and observations, the spans of an agent's steps (contract 10.6).

use std::borrow::Cow;
use std::fmt;

use opentelemetry::trace::{SpanBuilder, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use serde::Serialize;

use crate::keys;

/// The kind of step an observation records (`crucible.observation.type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ObservationType {
    /// A generic step.
    Span,
    /// A model call.
    Generation,
    /// A point in time.
    Event,
    /// An embedding call.
    Embedding,
    /// An agent run.
    Agent,
    /// A tool call.
    Tool,
    /// A chain of steps.
    Chain,
    /// A retrieval.
    Retriever,
    /// An evaluation.
    Evaluator,
    /// A guardrail check.
    Guardrail,
}

impl ObservationType {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Span => "span",
            Self::Generation => "generation",
            Self::Event => "event",
            Self::Embedding => "embedding",
            Self::Agent => "agent",
            Self::Tool => "tool",
            Self::Chain => "chain",
            Self::Retriever => "retriever",
            Self::Evaluator => "evaluator",
            Self::Guardrail => "guardrail",
        }
    }
}

/// The trace-level values a context carries to every span started under it (contract 3.3: SDKs set session and
/// user ids at span start so that every span carries them).
#[derive(Clone, Debug, Default)]
pub(crate) struct TraceAttrs {
    session: Option<String>,
    user: Option<String>,
    tags: Vec<String>,
    metadata: Vec<KeyValue>,
}

impl TraceAttrs {
    fn of(cx: &Context) -> Self {
        cx.get::<Self>().cloned().unwrap_or_default()
    }

    /// Sets the values on a starting span.
    pub(crate) fn apply(&self, span: &mut opentelemetry_sdk::trace::Span) {
        use opentelemetry::trace::Span as _;
        if let Some(session) = &self.session {
            span.set_attribute(KeyValue::new(keys::SESSION_ID, session.clone()));
        }
        if let Some(user) = &self.user {
            span.set_attribute(KeyValue::new(keys::USER_ID, user.clone()));
        }
        if !self.tags.is_empty() {
            let tags = self.tags.iter().cloned().map(StringValue::from).collect();
            span.set_attribute(KeyValue::new(keys::TAGS, Value::Array(Array::String(tags))));
        }
        span.set_attributes(self.metadata.iter().cloned());
    }
}

/// A context whose spans carry the session id (`crucible.session.id`).
pub fn with_session(cx: &Context, id: impl Into<String>) -> Context {
    let mut attrs = TraceAttrs::of(cx);
    attrs.session = Some(id.into());
    cx.with_value(attrs)
}

/// A context whose spans carry the user id (`crucible.user.id`).
pub fn with_user(cx: &Context, id: impl Into<String>) -> Context {
    let mut attrs = TraceAttrs::of(cx);
    attrs.user = Some(id.into());
    cx.with_value(attrs)
}

/// A context whose spans carry the tags (`crucible.tags`), added to those `cx` carries.
pub fn with_tags<I, T>(cx: &Context, tags: I) -> Context
where
    I: IntoIterator<Item = T>,
    T: Into<String>,
{
    let mut attrs = TraceAttrs::of(cx);
    for tag in tags {
        let tag = tag.into();
        if !tag.is_empty() && !attrs.tags.contains(&tag) {
            attrs.tags.push(tag);
        }
    }
    cx.with_value(attrs)
}

/// A context whose spans carry one metadata entry (`crucible.metadata.<key>`), replacing an entry of the same
/// key. Strings, booleans, integers and doubles keep their type.
pub fn with_metadata(cx: &Context, key: impl AsRef<str>, value: impl Into<Value>) -> Context {
    let mut attrs = TraceAttrs::of(cx);
    let key = format!("{}{}", keys::METADATA_PREFIX, key.as_ref());
    attrs.metadata.retain(|kv| kv.key.as_str() != key);
    attrs.metadata.push(KeyValue::new(key, value));
    cx.with_value(attrs)
}

/// Token usage of a generation. `cache_read` and `cache_write` are parts of `input`, and `reasoning` a part of
/// `output`, as in the GenAI semantic conventions. Zero fields are not sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens (`gen_ai.usage.input_tokens`).
    pub input: u64,
    /// Output tokens (`gen_ai.usage.output_tokens`).
    pub output: u64,
    /// Input tokens read from a cache (`gen_ai.usage.cache_read.input_tokens`).
    pub cache_read: u64,
    /// Input tokens written to a cache (`gen_ai.usage.cache_creation.input_tokens`).
    pub cache_write: u64,
    /// Output tokens spent on reasoning (`gen_ai.usage.reasoning.output_tokens`).
    pub reasoning: u64,
}

/// A cost in USD that the caller already knows. `total` is the whole cost; the other fields are the cost of one
/// usage key each. A provided cost replaces the cost Crucible would calculate. Zero fields are not sent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cost {
    /// The whole cost (`crucible.cost.total`).
    pub total: f64,
    /// The cost of the input tokens (`crucible.cost.input`).
    pub input: f64,
    /// The cost of the output tokens (`crucible.cost.output`).
    pub output: f64,
    /// The cost of the cached input tokens (`crucible.cost.cache_read`).
    pub cache_read: f64,
    /// The cost of the cache writes (`crucible.cost.cache_write`).
    pub cache_write: f64,
    /// The cost of the reasoning tokens (`crucible.cost.reasoning`).
    pub reasoning: f64,
}

/// One step of an agent: a span that ends when [`Observation::end`] is called, or when the last context that
/// holds it is dropped.
#[derive(Debug)]
pub struct Observation {
    cx: Context,
}

/// Starts an observation named `name` as a child of the span in `parent`. Before [`crate::init`] and after
/// [`crate::shutdown`] the observation records nothing and costs almost nothing.
pub fn observe(
    parent: &Context,
    name: impl Into<Cow<'static, str>>,
    kind: ObservationType,
) -> Observation {
    let Some(client) = crate::client::active() else {
        // A non-recording span with the parent's ids, so spans of other tracers still nest correctly.
        let parent_span = parent.span().span_context().clone();
        return Observation {
            cx: parent.with_remote_span_context(parent_span),
        };
    };
    let builder = SpanBuilder::from_name(name)
        .with_attributes([KeyValue::new(keys::OBSERVATION_TYPE, kind.as_str())]);
    let span = client.tracer.build_with_context(builder, parent);
    Observation {
        cx: parent.with_span(span),
    }
}

impl Observation {
    /// A context that carries this observation's span: start child observations with it, or run work under it
    /// with `opentelemetry::context::FutureExt::with_context`.
    pub fn context(&self) -> Context {
        self.cx.clone()
    }

    /// Records the input (`crucible.input`): a string as it is, any other value as JSON text.
    pub fn set_input<T: Serialize + ?Sized>(&self, value: &T) {
        self.set_body(keys::INPUT, value);
    }

    /// Records the output (`crucible.output`): a string as it is, any other value as JSON text.
    pub fn set_output<T: Serialize + ?Sized>(&self, value: &T) {
        self.set_body(keys::OUTPUT, value);
    }

    fn set_body<T: Serialize + ?Sized>(&self, key: &'static str, value: &T) {
        let span = self.cx.span();
        if !span.is_recording() {
            return;
        }
        let text = match serde_json::to_value(value) {
            Ok(serde_json::Value::String(text)) => text,
            Ok(other) => other.to_string(),
            Err(_) => return,
        };
        span.set_attribute(KeyValue::new(key, text));
    }

    /// Records the model a generation requested (`gen_ai.request.model`), which prices its usage.
    pub fn set_model(&self, model: impl Into<String>) {
        self.cx
            .span()
            .set_attribute(KeyValue::new(keys::REQUEST_MODEL, model.into()));
    }

    /// Records token usage (`gen_ai.usage.*`). Crucible prices it when no cost is set.
    pub fn set_usage(&self, usage: Usage) {
        let span = self.cx.span();
        for (key, count) in [
            (keys::INPUT_TOKENS, usage.input),
            (keys::OUTPUT_TOKENS, usage.output),
            (keys::CACHE_READ_TOKENS, usage.cache_read),
            (keys::CACHE_WRITE_TOKENS, usage.cache_write),
            (keys::REASONING_TOKENS, usage.reasoning),
        ] {
            if count != 0 {
                span.set_attribute(KeyValue::new(key, i64::try_from(count).unwrap_or(i64::MAX)));
            }
        }
    }

    /// Records a provided cost (`crucible.cost.total` and `crucible.cost.<usage key>`, in USD).
    pub fn set_cost(&self, cost: Cost) {
        let span = self.cx.span();
        for (key, usd) in [
            ("total", cost.total),
            ("input", cost.input),
            ("output", cost.output),
            ("cache_read", cost.cache_read),
            ("cache_write", cost.cache_write),
            ("reasoning", cost.reasoning),
        ] {
            if usd != 0.0 {
                span.set_attribute(KeyValue::new(format!("{}{key}", keys::COST_PREFIX), usd));
            }
        }
    }

    /// Marks the observation as failed with `error`'s message.
    pub fn set_error(&self, error: impl fmt::Display) {
        self.cx.span().set_status(Status::error(error.to_string()));
    }

    /// Sets any other attribute, for example `crucible.trace.name` or `gen_ai.provider.name`.
    pub fn set_attribute(&self, attribute: KeyValue) {
        self.cx.span().set_attribute(attribute);
    }

    /// Ends the observation. Its span is exported in the background.
    pub fn end(self) {
        self.cx.span().end();
    }
}
