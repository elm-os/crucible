//! From the service's values to the wire DTOs (contract 1.3): times as RFC 3339 strings, ids as hex, costs in
//! USD and counts saturating at 2^53 - 1.
//! The conversions here are the ones the route groups share; each group converts its own responses.

use crucible_api_types as wire;
use crucible_api_types::{SafeU64, Saturation, usd_from_nanos};
use crucible_query as query;
use crucible_records::{ScoreDataType, SpanId, Timestamp};

pub(crate) fn micros_between(start: Timestamp, end: Timestamp) -> u64 {
    u64::try_from(end.micros().saturating_sub(start.micros())).unwrap_or(0)
}

pub(crate) fn meta(meta: &query::Meta) -> wire::Meta {
    wire::Meta {
        as_of: meta.as_of.to_string(),
        partial: meta.partial,
        partial_reason: meta.partial_reason.map(|reason| match reason {
            query::PartialReason::StoreUnavailable => wire::PartialReason::StoreUnavailable,
            query::PartialReason::Deadline => wire::PartialReason::Deadline,
            query::PartialReason::Quarantined => wire::PartialReason::Quarantined,
        }),
        consistency: match meta.consistency {
            query::Consistency::Strong => wire::Consistency::Strong,
            query::Consistency::Lagging => wire::Consistency::Lagging,
        },
    }
}

pub(crate) fn span(span: &query::TraceSpan) -> wire::Span {
    let summary = &span.summary;
    let mut s = Saturation::default();
    wire::Span {
        span_id: summary.span_id.to_string(),
        parent_span_id: (summary.parent_span_id != SpanId::ZERO)
            .then(|| summary.parent_span_id.to_string()),
        name: summary.name.to_string(),
        observation_type: summary.observation_type.into(),
        start_time: summary.start_time.to_string(),
        end_time: summary.end_time.to_string(),
        duration_us: s.u64(micros_between(summary.start_time, summary.end_time)),
        status_code: summary.status_code.into(),
        level: summary.level.into(),
        model: summary.model.to_string(),
        total_cost_usd: usd_from_nanos(summary.total_cost_nusd.nanos()),
        input_tokens: s.u64(summary.input_tokens),
        output_tokens: s.u64(summary.output_tokens),
        total_tokens: s.u64(summary.total_tokens),
        child_count: span.child_count.map(|count| s.u64(count)),
        io_shape: span.io_shape().map(Into::into),
        saturated: s.saturated(),
    }
}

fn fact_ref(resolved: &query::Resolved) -> wire::FactRef {
    wire::FactRef {
        fact_id: resolved.fact_id.to_string(),
        version: SafeU64::saturating(resolved.version).0,
        created_at: resolved.created_at.to_string(),
        author_kind: resolved.author_kind.into(),
        author_id: resolved.author_id.to_string(),
    }
}

/// Stored JSON text as a JSON value; text that does not parse stays a string.
fn json_value(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_owned()))
}

fn score(score: &query::ScoreFact) -> wire::Score {
    let value = match score.data_type {
        ScoreDataType::Numeric => wire::ScoreValue::Number(score.value_num),
        ScoreDataType::Boolean => wire::ScoreValue::Bool(score.value_num != 0.0),
        ScoreDataType::Categorical | ScoreDataType::Text => {
            wire::ScoreValue::Text(score.value_str.to_string())
        }
    };
    wire::Score {
        name: score.name.to_string(),
        score_id: score.score_id.as_ref().map(ToString::to_string),
        data_type: score.data_type.into(),
        value,
        source: score.source.into(),
        comment: score.comment.to_string(),
        metadata: (!score.metadata.is_empty()).then(|| json_value(&score.metadata)),
        fact: fact_ref(&score.resolved),
    }
}

fn correction(correction: &query::CorrectionFact) -> wire::Correction {
    wire::Correction {
        corrected: json_value(&correction.corrected),
        note: correction.note.to_string(),
        fact: fact_ref(&correction.resolved),
    }
}

fn flag(flag: &query::FlagFact) -> wire::FlagValue {
    wire::FlagValue {
        value: flag.value,
        fact: fact_ref(&flag.resolved),
    }
}

/// The effective facts of one target.
pub(crate) fn facts(target: &query::TargetFacts) -> wire::Facts {
    let facts = &target.facts;
    wire::Facts {
        scores: facts.scores.iter().map(score).collect(),
        tags: facts
            .tags
            .iter()
            .map(|edit| wire::TagEdit {
                tag: edit.tag.to_string(),
                op: edit.op.into(),
                fact: fact_ref(&edit.resolved),
            })
            .collect(),
        bookmarked: facts.bookmarked.as_ref().map(flag),
        public: facts.public.as_ref().map(flag),
        correction: facts.correction.as_ref().map(correction),
        meta: meta(&target.meta),
    }
}

/// The facts of a trace view.
pub(crate) fn trace_facts(trace: &query::TraceFacts) -> wire::TraceFacts {
    let facts = &trace.facts;
    wire::TraceFacts {
        tags: trace.tags.iter().map(ToString::to_string).collect(),
        bookmarked: facts.bookmarked.is_some_and(|flag| flag.value),
        public: facts.public.map(|flag| flag.value),
        scores: facts.scores.iter().map(score).collect(),
        correction: facts.correction.as_ref().map(correction),
    }
}
