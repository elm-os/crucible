//! Full-text search (contracts 8.7 and 12.6): the hits from the text indexes of `span_payload`, with a snippet
//! of each searched string field for highlighting, then the core rows of the hits.
//!
//! The `name` field of the route is not searched here: a name match is the list filter's `name` condition
//! (`Filter::Name`), which the core step applies.

use crucible_records::{ProjectScope, SpanId, TableId, TimeWindow, Timestamp};
use crucible_schema::{Column, ColumnRef, SpanCoreCol, SpanPayloadCol};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::filter::{Filter, FilterTarget};
use crate::literal::Literal;
use crate::node::{Node, Scan, Snippet, TextSearch, TokenMode};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// The most hits one search returns.
pub const SEARCH_MAX_LIMIT: u32 = 200;
/// The snippet of each searched string field: it starts this many bytes before the first token found.
pub const SEARCH_SNIPPET: Snippet = Snippet {
    before: 80,
    len: 240,
};

/// A field of `span_payload` that search covers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SearchField {
    /// `input`: tokens are its runs of ASCII letters and digits.
    Input,
    /// `output`: as `input`.
    Output,
    /// `metadata`: a token is one whole `key=value` entry.
    Metadata,
}

impl SearchField {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::Metadata => "metadata",
        }
    }

    fn column(self) -> ColumnRef {
        match self {
            Self::Input => SpanPayloadCol::Input,
            Self::Output => SpanPayloadCol::Output,
            Self::Metadata => SpanPayloadCol::MetadataKv,
        }
        .column_ref()
    }
}

/// A search request.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SearchRequest {
    /// The tokens of the query. The builder lowers their ASCII letters, as the text indexes do to the columns.
    pub tokens: Vec<String>,
    /// How the tokens combine. A phrase is read as all of its tokens; the caller verifies the order on the
    /// snippets.
    pub mode: TokenMode,
    /// The fields to search.
    pub fields: Vec<SearchField>,
    /// The filter; [`Filter::all`] for none.
    pub filter: Filter,
    /// The most hits, 1 to [`SEARCH_MAX_LIMIT`].
    pub limit: u32,
}

/// The first step of a search.
#[derive(Clone, PartialEq, Debug)]
pub struct SearchPlan {
    /// The hits. The outputs are, in order: the key of `span_payload` (`start_minute`, `trace_hash`,
    /// `span_id`), then for each searched string field, in request order, `<field>_match` and
    /// `<field>_snippet` ([`TextSearch::with_snippets`], with [`SEARCH_SNIPPET`]). Which hits a limit keeps is
    /// not defined.
    pub hits: QueryPlan,
    /// The conjuncts of the filter the hits do not apply, because `span_payload` lacks their fields.
    /// [`search_spans`] applies them.
    pub rest: Filter,
}

/// Search, step 1: the hits. Conditions on `user_id`, `session_id` and `metadata` that are top-level conjuncts
/// of the filter restrict the hits; the rest is returned for step 2.
pub fn search(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    request: &SearchRequest,
) -> Result<Option<SearchPlan>, PlanError> {
    if request.limit == 0 || request.limit > SEARCH_MAX_LIMIT {
        return Err(PlanError::Limit);
    }
    let (payload, rest) = request.filter.split(FilterTarget::SpanPayload);
    // Refuse now what step 2 could not evaluate, before any read runs.
    rest.pred(FilterTarget::SpanCore)?;
    let hits = TextSearch::new(
        TableId::SpanPayload,
        request.fields.iter().map(|field| field.column()),
        request.mode,
        request
            .tokens
            .iter()
            .map(|token| token.to_ascii_lowercase())
            .collect(),
        payload.pred(FilterTarget::SpanPayload)?,
        request.limit,
    )?
    .with_snippets(SEARCH_SNIPPET)?;
    let hits = clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Search),
        Node::TextSearch(hits),
    )?;
    Ok(hits.map(|hits| SearchPlan { hits, rest }))
}

/// Search, step 2: every column of `span_core` for the hits of step 1 (their `start_minute`, `trace_hash` and
/// `span_id`) that also match `rest`, in no defined order. A hit whose core row is missing or filtered out has
/// no row.
pub fn search_spans(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    hits: &[(Timestamp, u32, SpanId)],
    rest: &Filter,
) -> Result<Option<QueryPlan>, PlanError> {
    let list = |values: Vec<Literal>| {
        let mut distinct: Vec<Literal> = Vec::with_capacity(values.len());
        for value in values {
            if !distinct.contains(&value) {
                distinct.push(value);
            }
        }
        distinct
    };
    let minutes = list(hits.iter().map(|hit| hit.0.floor_minute().into()).collect());
    let hashes = list(hits.iter().map(|hit| hit.1.into()).collect());
    let ids = list(hits.iter().map(|hit| hit.2.into()).collect());
    let mut conditions = vec![
        Pred::is_in(SpanCoreCol::StartMinute, minutes),
        Pred::is_in(SpanCoreCol::TraceHash, hashes),
        Pred::is_in(SpanCoreCol::SpanId, ids),
    ];
    if !rest.is_all() {
        conditions.push(rest.pred(FilterTarget::SpanCore)?);
    }
    let scan = Scan::new(
        TableId::SpanCore,
        SpanCoreCol::ALL.iter().copied(),
        Pred::And(conditions),
    )?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Search),
        Node::Scan(scan),
    )
}
