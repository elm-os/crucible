//! Full-text search: `POST projects/{project_id}/search` (contract 8.7 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::meta::Meta;
use crate::trace_detail::Span;

/// How the tokens of a query combine.
#[derive(
    Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize, ToSchema, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Every token occurs in the field.
    #[default]
    All,
    /// At least one token occurs.
    Any,
    /// The tokens occur in order and adjacent. A phrase written with separators other than single spaces
    /// is found only within the first 240 bytes of a field.
    Phrase,
}

/// A field that search covers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum SearchField {
    /// The span's input. Its tokens are its runs of characters other than ASCII punctuation, spaces and
    /// controls, compared without regard to ASCII case.
    Input,
    /// The span's output, tokenized as `input`.
    Output,
    /// The span's metadata. Its tokens are whole `key=value` entries, compared as written.
    Metadata,
}

/// The body of `POST projects/{project_id}/search`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    /// What to find, at most 1,024 bytes. For `input` and `output` it is split into tokens as those fields
    /// are; for `metadata` its whitespace-separated terms are `key=value` entries. At most 64 tokens.
    pub query: String,
    /// How the tokens combine; default `all`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mode: Option<SearchMode>,
    /// The fields to search, each once; default `input` and `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub fields: Option<Vec<SearchField>>,
    /// Start of the window over the spans' start times, inclusive (RFC 3339).
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the window, exclusive (RFC 3339). The window may span at most the project's maximum (default
    /// 30 days).
    #[schema(format = DateTime)]
    pub to: String,
    /// Page size: 1 to 200, default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 200)]
    #[ts(optional)]
    pub limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same query, mode, fields and window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cursor: Option<String>,
}

/// A run of a snippet's text.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SearchFragment {
    /// The text.
    pub text: String,
    /// Whether it matched: a token of the query, or in a phrase search the phrase.
    pub highlighted: bool,
}

/// Where a hit matched.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SearchMatch {
    /// The field.
    pub field: SearchField,
    /// The snippet around the field's first match, as fragments in order: at most 240 bytes. Empty for
    /// `metadata`, whose matching entries are the query's terms.
    pub fragments: Vec<SearchFragment>,
    /// Whether the field goes on before the snippet.
    pub cut_before: bool,
    /// Whether the field may go on after the snippet.
    pub cut_after: bool,
}

/// One hit: a span and where it matched.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SearchHit {
    /// The span's trace: 32 lowercase hex characters.
    pub trace_id: String,
    /// The span.
    pub span: Span,
    /// The fields with a match to show. Empty when the matches lie past the snippets.
    pub matches: Vec<SearchMatch>,
}

/// One page of hits, newest first.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SearchPage {
    /// The hits. A page can hold fewer hits than its size, even none, while `has_more` is true: phrase
    /// candidates whose snippets lack the phrase are left out.
    pub data: Vec<SearchHit>,
    /// The cursor of the next page; null on the last page.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// Whether another page exists.
    pub has_more: bool,
    /// How complete and how fresh the page is. Search reads the store only, so spans acknowledged moments
    /// ago may be missing, and the page then says `lagging`.
    pub meta: Meta,
}
