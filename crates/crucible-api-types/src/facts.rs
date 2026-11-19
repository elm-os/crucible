//! Facts: `POST projects/{project_id}/feedback`, `.../tags`, `.../flags`, `.../corrections` and
//! `GET .../facts` (contract 3.8 and 12.6), and the facts that decorate traces.
//!
//! A write names its target with `trace_id` (a trace), `trace_id` and `span_id` (a span) or `session_id` (a
//! session). Facts are append-only: a later write of the same key wins, and a retraction removes the key's
//! value.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::enums::{AuthorKind, ScoreDataType, ScoreSource, TagOp};
use crate::meta::Meta;
use crate::wire::SafeU64;

/// A score's value: a number for `numeric`, a boolean for `boolean`, a string for `categorical` and `text`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(untagged)]
pub enum ScoreValue {
    /// A number.
    Number(f64),
    /// A boolean.
    Bool(bool),
    /// A string.
    Text(String),
}

/// The body of `POST projects/{project_id}/feedback`: one score with `source = feedback`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct FeedbackRequest {
    /// The target trace (32 hex characters); with `span_id`, its span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trace_id: Option<String>,
    /// The target span (16 hex characters), with `trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span_id: Option<String>,
    /// The target session, instead of a trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_id: Option<String>,
    /// The score name, without `#`.
    pub name: String,
    /// The client's id of this score: scores with the same name and different ids are kept apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub score_id: Option<String>,
    /// The value.
    pub value: ScoreValue,
    /// How to read the value; by default `numeric` for a number, `boolean` for a boolean and `categorical` for
    /// a string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub data_type: Option<ScoreDataType>,
    /// A comment, at most 4 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub comment: Option<String>,
    /// A JSON object, at most 16 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    #[ts(optional, type = "Record<string, unknown>")]
    pub metadata: Option<serde_json::Value>,
    /// Retracts the score of this name (and id): the target has no value for it afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub retracted: Option<bool>,
}

/// The body of `POST projects/{project_id}/tags`: adds a tag to the target or removes it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct TagRequest {
    /// The target trace (32 hex characters); with `span_id`, its span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trace_id: Option<String>,
    /// The target span (16 hex characters), with `trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span_id: Option<String>,
    /// The target session, instead of a trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_id: Option<String>,
    /// The tag, 1 to 256 bytes.
    pub tag: String,
    /// `add` or `remove`. Removing a tag the trace was sent with hides it.
    pub op: TagOp,
}

/// A flag of a target.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum FlagName {
    /// The target is bookmarked.
    Bookmarked,
    /// The trace is public.
    Public,
}

/// The body of `POST projects/{project_id}/flags`: sets a flag of the target.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct FlagRequest {
    /// The target trace (32 hex characters); with `span_id`, its span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trace_id: Option<String>,
    /// The target span (16 hex characters), with `trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span_id: Option<String>,
    /// The target session, instead of a trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_id: Option<String>,
    /// The flag.
    pub flag: FlagName,
    /// Its new value.
    pub value: bool,
}

/// The body of `POST projects/{project_id}/corrections`: the corrected output of the target.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct CorrectionRequest {
    /// The target trace (32 hex characters); with `span_id`, its span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trace_id: Option<String>,
    /// The target span (16 hex characters), with `trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span_id: Option<String>,
    /// The target session, instead of a trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_id: Option<String>,
    /// The corrected output: any JSON value.
    #[schema(value_type = Object)]
    #[ts(type = "unknown")]
    pub corrected: serde_json::Value,
    /// A note, at most 4 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub note: Option<String>,
    /// Retracts the correction: the target has none afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub retracted: Option<bool>,
}

/// What the server assigned to a fact it made durable.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct FactWritten {
    /// The fact's id (a UUIDv7).
    pub fact_id: String,
    /// The key latest-wins resolves over: the score name (with `#` and the score id), the tag, the flag or
    /// `output`.
    pub key: String,
    /// The server's microsecond clock at acceptance: of two facts of one key, the greater version wins.
    pub version: SafeU64,
    /// When the fact was accepted.
    #[schema(format = DateTime)]
    pub created_at: String,
    /// The start of the target the fact is filed under: a trace's earliest known start, a session's first
    /// seen.
    #[schema(format = DateTime)]
    pub target_time: String,
}

/// The query of `GET projects/{project_id}/facts`: one target.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct FactsParams {
    /// The target trace (32 hex characters); with `span_id`, its span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trace_id: Option<String>,
    /// The target span (16 hex characters), with `trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span_id: Option<String>,
    /// The target session, instead of a trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_id: Option<String>,
}

/// The fact that holds a value: the latest write of its key.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct FactRef {
    /// The fact's id.
    pub fact_id: String,
    /// The fact's version.
    pub version: SafeU64,
    /// When it was written.
    #[schema(format = DateTime)]
    pub created_at: String,
    /// Who wrote it.
    pub author_kind: AuthorKind,
    /// The user or API key that wrote it.
    pub author_id: String,
}

/// A score.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Score {
    /// The score name.
    pub name: String,
    /// The client's score id; null when the score was written without one.
    #[schema(required = true)]
    pub score_id: Option<String>,
    /// How to read the value.
    pub data_type: ScoreDataType,
    /// The value.
    pub value: ScoreValue,
    /// Where it came from.
    pub source: ScoreSource,
    /// The comment; empty in trace lists.
    pub comment: String,
    /// The metadata object; null when there is none, and in trace lists.
    #[schema(value_type = Option<Object>, required = true)]
    #[ts(type = "Record<string, unknown> | null")]
    pub metadata: Option<serde_json::Value>,
    /// The fact.
    pub fact: FactRef,
}

/// The latest edit of a tag.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct TagEdit {
    /// The tag.
    pub tag: String,
    /// Whether it was added or removed.
    pub op: TagOp,
    /// The fact.
    pub fact: FactRef,
}

/// The latest value of a flag.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct FlagValue {
    /// The value.
    pub value: bool,
    /// The fact.
    pub fact: FactRef,
}

/// The latest correction of an output.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Correction {
    /// The corrected output.
    #[schema(value_type = Object)]
    #[ts(type = "unknown")]
    pub corrected: serde_json::Value,
    /// The note.
    pub note: String,
    /// The fact.
    pub fact: FactRef,
}

/// The effective facts of one target: per key the latest write, without retracted keys.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Facts {
    /// The scores, by name.
    pub scores: Vec<Score>,
    /// The latest edit of each tag. The target's effective tags are its ingest tags plus the added ones minus
    /// the removed ones.
    pub tags: Vec<TagEdit>,
    /// The `bookmarked` flag; null when it was never set.
    #[schema(required = true)]
    pub bookmarked: Option<FlagValue>,
    /// The `public` flag; null when it was never set.
    #[schema(required = true)]
    pub public: Option<FlagValue>,
    /// The correction of the output; null when there is none.
    #[schema(required = true)]
    pub correction: Option<Correction>,
    /// How complete and how fresh the response is.
    pub meta: Meta,
}

/// The facts a trace view shows.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct TraceFacts {
    /// The effective tags: the tags the trace was sent with, plus the added ones, minus the removed ones.
    pub tags: Vec<String>,
    /// Whether the trace is bookmarked.
    pub bookmarked: bool,
    /// The `public` flag; null when it was never set.
    #[schema(required = true)]
    pub public: Option<bool>,
    /// The scores, by name. In trace lists they carry no comment or metadata.
    pub scores: Vec<Score>,
    /// The correction of the trace's output; null when there is none, and in trace lists.
    #[schema(required = true)]
    pub correction: Option<Correction>,
}
