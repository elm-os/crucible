//! Full-text search (contracts 8.7 and 12.6): the hits from the text indexes of `span_payload`, newest first
//! along the table's key and paged by a keyset cursor, with a snippet of each searched string field for
//! highlighting, then the core rows of the hits.
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
use crate::node::{ArrivalBounds, Direction, Node, Scan, Snippet, TextSearch, TokenMode};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// The key of a hit, `(start_minute, trace_hash, span_id)`: its sort tuple and its cursor.
pub type SearchKey = (Timestamp, u32, SpanId);

/// The most hits one search returns.
pub const SEARCH_MAX_LIMIT: u32 = 200;
/// The most hits one core lookup ([`search_spans`]) names. Its three key lists then take at most 192 of the
/// 1,000 values and settings one ClickHouse request holds, which leaves room for the largest list filter the
/// core rows test and for the pending deletions a read carries; a larger page takes several lookups.
pub const SEARCH_SPANS_PER_LOOKUP: usize = 64;
/// The snippet of each searched string field: it starts this many bytes before the first token found.
pub const SEARCH_SNIPPET: Snippet = Snippet {
    before: 80,
    len: 240,
};

/// A field of `span_payload` that search covers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SearchField {
    /// `input`: tokens are its runs of ASCII letters and digits, without regard to ASCII case.
    Input,
    /// `output`: as `input`.
    Output,
    /// `metadata`: a token is one whole `key=value` entry, case included.
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

    /// Whether the field's index lowers ASCII letters, so the tokens it is searched for must be lowered too.
    pub const fn lowered(self) -> bool {
        match self {
            Self::Input | Self::Output => true,
            Self::Metadata => false,
        }
    }
}

/// A search request.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SearchRequest {
    /// The tokens of the query. For `input` and `output` the builder lowers their ASCII letters, as those
    /// text indexes do to the columns; `metadata` entries keep their case.
    pub tokens: Vec<String>,
    /// How the tokens combine. A phrase is read as all of its tokens; the caller verifies the order on the
    /// snippets, which a phrase search anchors on the phrase ([`TextSearch::needles`]).
    pub mode: TokenMode,
    /// The fields to search: `input` and `output`, or `metadata`, whose tokens are whole entries.
    pub fields: Vec<SearchField>,
    /// The filter; [`Filter::all`] for none.
    pub filter: Filter,
    /// The page size, 1 to [`SEARCH_MAX_LIMIT`].
    pub limit: u32,
    /// The key of the last hit of the previous page.
    pub cursor: Option<SearchKey>,
    /// Fixed by the first page: only rows with `received_at <= as_of` are read, so pages do not shift.
    pub as_of: Timestamp,
}

/// The first step of a search.
#[derive(Clone, PartialEq, Debug)]
pub struct SearchPlan {
    /// The hits, ordered by the key of `span_payload` (`start_minute`, `trace_hash`, `span_id`) descending,
    /// after the cursor: at most `limit + 1`, so that the caller knows whether another page exists. The
    /// outputs are, in order: that key, then for each searched string field, in request order,
    /// `<field>_match` and `<field>_snippet` ([`TextSearch::with_snippets`], with [`SEARCH_SNIPPET`]).
    pub hits: QueryPlan,
    /// The conjuncts of the filter the hits do not apply, because `span_payload` lacks their fields.
    /// [`search_spans`] applies them.
    pub rest: Filter,
}

/// Search, step 1: the hits. Conditions on `user_id`, `session_id` and `metadata` that are top-level conjuncts
/// of the filter restrict the hits; the rest is returned for step 2. One search covers fields whose indexes
/// tokenize alike: `metadata` with `input` or `output` is [`PlanError::Tokenizers`].
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
    let lowered = request.fields.first().is_some_and(|field| field.lowered());
    if request
        .fields
        .iter()
        .any(|field| field.lowered() != lowered)
    {
        return Err(PlanError::Tokenizers);
    }
    let (payload, rest) = request.filter.split(FilterTarget::SpanPayload);
    // Refuse now what step 2 could not evaluate, before any read runs.
    rest.pred(FilterTarget::SpanCore)?;
    let tokens = request
        .tokens
        .iter()
        .map(|token| {
            if lowered {
                token.to_ascii_lowercase()
            } else {
                token.clone()
            }
        })
        .collect();
    let mut hits = TextSearch::new(
        TableId::SpanPayload,
        request.fields.iter().map(|field| field.column()),
        request.mode,
        tokens,
        payload.pred(FilterTarget::SpanPayload)?,
        request.limit + 1,
    )?
    .with_snippets(SEARCH_SNIPPET)?
    .ordered(Direction::Desc)
    .arrived(ArrivalBounds {
        as_of: Some(request.as_of),
        since: None,
    })?;
    if let Some((minute, hash, span_id)) = request.cursor {
        hits = hits.after(vec![minute.into(), hash.into(), Literal::from(span_id)])?;
    }
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
/// no row. More than [`SEARCH_SPANS_PER_LOOKUP`] hits are refused.
pub fn search_spans(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    hits: &[SearchKey],
    rest: &Filter,
) -> Result<Option<QueryPlan>, PlanError> {
    if hits.len() > SEARCH_SPANS_PER_LOOKUP {
        return Err(PlanError::ListLength("hits"));
    }
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
