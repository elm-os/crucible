//! Full-text search (contract 8.7 and 12.6): the hits from the text indexes of `span_payload`, newest first and
//! paged by the key of the hits, with each searched field's snippet turned into highlight fragments here, then
//! the core rows of the page's spans.
//!
//! - **Tokens.** `input` and `output` are searched for the query's tokens as their indexes split text: runs of
//!   characters other than ASCII punctuation, spaces and controls, with ASCII letters lowered. `metadata` is
//!   searched for whole `key=value` entries, case included: the query's whitespace-separated terms. A query over
//!   both is two plans, merged by key.
//! - **Phrases.** The floor server has no phrase match, so a phrase plan finds every token (contract 8.9) and
//!   anchors its snippets on the phrase written with single spaces (on the field's start when that does not
//!   occur). A hit is kept when one of its snippets holds the phrase's tokens in order and adjacent, so a
//!   phrase written with other separators is found only within the first bytes of a field. Metadata entries
//!   have no order, so a phrase over them is all of its entries.
//! - **Paging.** Each plan reads one hit more than the page after the cursor. The merge is complete down to the
//!   newest last hit of a plan that came back full, so the page and its cursor come from the hit stream, never
//!   from the rows that survive the core lookup or the phrase check: a page whose hits all drop still moves on.
//! - **`meta`.** The recent index holds no payloads, so search reads the store only: an unavailable store fails
//!   the request, and the answer is `lagging` while the index holds spans of the window that the store may not
//!   serve yet (contract 8.9).

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crucible_plan::{
    Filter, MAX_SNIPPET_TOKENS, Row, SEARCH_MAX_LIMIT, SEARCH_SNIPPET, SearchField, SearchKey,
    SearchRequest, TokenMode, search, search_spans,
};
use crucible_recent::Consistency;
use crucible_records::{ProjectId, ProjectScope, TableId, TimeWindow, Timestamp};
use crucible_schema::{Column, SpanCoreCol};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::decode::Fields;
use crate::service::{QueryService, Read};
use crate::spans::{TraceSpan, stored};

/// The default page of search (contract 12.4).
pub const SEARCH_DEFAULT_LIMIT: u32 = 50;

/// The most tokens a query may hold for one field kind.
pub const SEARCH_MAX_TOKENS: usize = MAX_SNIPPET_TOKENS;

/// The tables a search depends on, for the quarantine rule.
const SEARCH_TABLES: [TableId; 2] = [TableId::SpanPayload, TableId::SpanCore];

/// A cursor of the search: the key of the last hit of a page and the `as_of` the first page fixed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SearchCursor {
    project_id: ProjectId,
    window: TimeWindow,
    as_of: Timestamp,
    last: SearchKey,
}

impl SearchCursor {
    /// A cursor from its decoded parts, for the cursor codec.
    pub const fn from_parts(
        project_id: ProjectId,
        window: TimeWindow,
        as_of: Timestamp,
        last: SearchKey,
    ) -> Self {
        Self {
            project_id,
            window,
            as_of,
            last,
        }
    }

    /// The parts, for the cursor codec: project, window, `as_of` and the key of the last hit.
    pub const fn parts(&self) -> (ProjectId, TimeWindow, Timestamp, SearchKey) {
        (self.project_id, self.window, self.as_of, self.last)
    }

    fn check(
        &self,
        scope: ProjectScope,
        window: TimeWindow,
    ) -> Result<(Timestamp, SearchKey), QueryServiceError> {
        if (self.project_id, self.window) == (scope.project_id(), window) {
            Ok((self.as_of, self.last))
        } else {
            Err(QueryServiceError::CursorInvalid)
        }
    }
}

/// A search request.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SearchQuery {
    /// The window over the spans' `start_time`.
    pub window: TimeWindow,
    /// The query as written; [`search_tokens`] splits it per field.
    pub text: String,
    /// How the tokens combine.
    pub mode: TokenMode,
    /// The fields to search, at least one, each once.
    pub fields: Vec<SearchField>,
    /// The page size; default 50, at most 200.
    pub limit: Option<u32>,
    /// The cursor of the previous page.
    pub cursor: Option<SearchCursor>,
}

impl SearchQuery {
    /// The canonical form of what selects the hits besides the window, which a cursor's filter hash covers:
    /// the mode, then per field in a fixed order its name and its tokens.
    pub fn canonical(&self) -> Vec<u8> {
        let mut out = vec![match self.mode {
            TokenMode::All => 1,
            TokenMode::Any => 2,
            TokenMode::Phrase => 3,
        }];
        for field in [
            SearchField::Input,
            SearchField::Output,
            SearchField::Metadata,
        ] {
            if !self.fields.contains(&field) {
                continue;
            }
            put(&mut out, field.as_str());
            let tokens = search_tokens(&self.text, field, self.mode);
            out.extend_from_slice(&(tokens.len() as u64).to_be_bytes());
            for token in &tokens {
                put(&mut out, token);
            }
        }
        out
    }
}

fn put(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u64).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// The tokens `text` is searched for in `field`. For `input` and `output`: the runs of characters other than
/// ASCII punctuation, spaces and controls, ASCII letters lowered, as their indexes split the stored text. For
/// `metadata`: the whitespace-separated terms with an `=`, as written, each a whole `key=value` entry (a term
/// without one can match no entry). Repeats are kept only in a phrase, whose tokens are its order.
pub fn search_tokens(text: &str, field: SearchField, mode: TokenMode) -> Vec<String> {
    let mut tokens: Vec<String> = if field.lowered() {
        runs(text)
            .into_iter()
            .map(|(from, to)| text[from..to].to_ascii_lowercase())
            .collect()
    } else {
        text.split_ascii_whitespace()
            .filter(|term| term.contains('='))
            .map(str::to_owned)
            .collect()
    };
    if mode != TokenMode::Phrase || !field.lowered() {
        let mut seen = Vec::with_capacity(tokens.len());
        tokens.retain(|token| {
            let first = !seen.contains(token);
            if first {
                seen.push(token.clone());
            }
            first
        });
    }
    tokens
}

/// The byte ranges of the tokens of `text` as the `input` and `output` indexes split it: maximal runs of
/// characters other than ASCII punctuation, spaces and controls.
fn runs(text: &str) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (at, c) in text.char_indices() {
        let separator = c.is_ascii() && !c.is_ascii_alphanumeric();
        match (separator, start) {
            (false, None) => start = Some(at),
            (true, Some(from)) => {
                runs.push((from, at));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        runs.push((from, text.len()));
    }
    runs
}

/// A run of a snippet's text, marked when it is a match.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Fragment {
    /// The text.
    pub text: String,
    /// Whether it is a token of the query, or in a phrase search the phrase.
    pub highlighted: bool,
}

/// Where a hit matched.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SearchMatch {
    /// The field.
    pub field: SearchField,
    /// The snippet of a text field around its first match, in order. A `metadata` match has none: its entries
    /// are the query's terms.
    pub fragments: Vec<Fragment>,
    /// Whether the field goes on before the snippet.
    pub cut_before: bool,
    /// Whether the field may go on after the snippet.
    pub cut_after: bool,
}

/// One hit: the span and where it matched.
#[derive(Clone, PartialEq, Debug)]
pub struct SearchHit {
    /// The span, from the store.
    pub span: TraceSpan,
    /// The fields with a highlighted match; empty when the matches lie outside the snippets.
    pub matches: Vec<SearchMatch>,
}

/// One page of hits, newest first along `(start_minute, trace_hash, span_id)`.
#[derive(Clone, PartialEq, Debug)]
pub struct SearchPage {
    /// The hits. A page may hold fewer than its size, or none, while `has_more` is set: hits whose core row
    /// has not landed, and phrase candidates without the phrase in their snippets, are dropped.
    pub data: Vec<SearchHit>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<SearchCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The response metadata.
    pub meta: Meta,
}

/// A snippet of one text field of one hit, as the store returned it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Snippet {
    field: SearchField,
    /// The byte position, from 1, of the first needle; 0 when none occurs.
    position: u64,
    bytes: Vec<u8>,
}

/// One hit of the hit stream, before the core lookup.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Hit {
    key: SearchKey,
    snippets: Vec<Snippet>,
    /// Whether the metadata plan found it.
    metadata: bool,
}

impl Hit {
    fn decode(
        row: &Row,
        fields: &[SearchField],
        metadata: bool,
    ) -> Result<Self, QueryServiceError> {
        let f = Fields(row.values());
        let hash = u32::try_from(f.uint(1)?).map_err(|_| {
            QueryServiceError::Internal("a hit's trace hash is out of range".to_owned())
        })?;
        let snippets = fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                Ok(Snippet {
                    field: *field,
                    position: f.uint(3 + 2 * i)?,
                    bytes: f.bytes(4 + 2 * i)?.to_vec(),
                })
            })
            .collect::<Result<_, QueryServiceError>>()?;
        Ok(Self {
            key: (f.time(0)?, hash, f.span_id(2)?),
            snippets,
            metadata,
        })
    }
}

/// Merges the hits of several plans, each its first `limit + 1` hits after the cursor in key order descending.
/// A plan that came back full may have more hits past its last one, so the merge is complete only down to the
/// newest such last hit. Returns the page and whether another page exists.
fn merge(sources: Vec<Vec<Hit>>, limit: usize) -> (Vec<Hit>, bool) {
    let frontier = sources
        .iter()
        .filter(|hits| hits.len() > limit)
        .filter_map(|hits| hits.last().map(|hit| hit.key))
        .max();
    let mut by_key: BTreeMap<SearchKey, Hit> = BTreeMap::new();
    for hit in sources.into_iter().flatten() {
        match by_key.entry(hit.key) {
            Entry::Vacant(entry) => {
                entry.insert(hit);
            }
            Entry::Occupied(mut entry) => {
                let known = entry.get_mut();
                known.metadata |= hit.metadata;
                for snippet in hit.snippets {
                    if !known.snippets.iter().any(|s| s.field == snippet.field) {
                        known.snippets.push(snippet);
                    }
                }
            }
        }
    }
    let mut hits: Vec<Hit> = by_key
        .into_values()
        .rev()
        .filter(|hit| frontier.is_none_or(|frontier| hit.key >= frontier))
        .collect();
    let has_more = frontier.is_some() || hits.len() > limit;
    hits.truncate(limit);
    (hits, has_more)
}

/// The text of a snippet: without the continuation bytes of a character cut at its start (at most three) and
/// without a character cut at its end.
fn snippet_text(bytes: &[u8]) -> String {
    let lead = bytes
        .iter()
        .take(3)
        .take_while(|byte| (**byte & 0xc0) == 0x80)
        .count();
    let body = bytes.get(lead..).unwrap_or_default();
    let end = match std::str::from_utf8(body) {
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        _ => body.len(),
    };
    String::from_utf8_lossy(body.get(..end).unwrap_or_default()).into_owned()
}

/// The highlighted byte ranges of `text`: its whole tokens that are query tokens, or in a phrase search each
/// run of whole tokens that spells the phrase. A token at an edge the snippet cut may be part of a longer one,
/// so it never counts.
fn highlights(
    text: &str,
    tokens: &[String],
    phrase: bool,
    cut_before: bool,
    cut_after: bool,
) -> Vec<(usize, usize)> {
    let runs: Vec<(usize, usize)> = runs(text)
        .into_iter()
        .filter(|(from, to)| !(cut_before && *from == 0) && !(cut_after && *to == text.len()))
        .collect();
    let lowered = |(from, to): (usize, usize)| text[from..to].to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if phrase {
        if tokens.is_empty() {
            return ranges;
        }
        // Consecutive runs are adjacent tokens of the text: only separators lie between them.
        for window in runs.windows(tokens.len()) {
            if window
                .iter()
                .zip(tokens)
                .all(|(run, t)| lowered(*run) == *t)
            {
                let (from, to) = (window[0].0, window[tokens.len() - 1].1);
                match ranges.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => ranges.push((from, to)),
                }
            }
        }
    } else {
        ranges.extend(
            runs.iter()
                .filter(|run| tokens.contains(&lowered(**run)))
                .copied(),
        );
    }
    ranges
}

/// The fragments of `text` with `ranges` highlighted.
fn fragments(text: &str, ranges: &[(usize, usize)]) -> Vec<Fragment> {
    let mut out = Vec::with_capacity(ranges.len() * 2 + 1);
    let mut at = 0;
    for &(from, to) in ranges {
        if from > at {
            out.push(Fragment {
                text: text[at..from].to_owned(),
                highlighted: false,
            });
        }
        out.push(Fragment {
            text: text[from..to].to_owned(),
            highlighted: true,
        });
        at = to;
    }
    if at < text.len() {
        out.push(Fragment {
            text: text[at..].to_owned(),
            highlighted: false,
        });
    }
    out
}

/// The matches of one hit, and whether a phrase search verified it.
fn matches(hit: &Hit, tokens: &[String], mode: TokenMode) -> (Vec<SearchMatch>, bool) {
    let phrase = mode == TokenMode::Phrase;
    let mut found = Vec::new();
    for snippet in &hit.snippets {
        // Without a needle the snippet is the field's start: a phrase may still be there with other separators.
        if snippet.position == 0 && !phrase {
            continue;
        }
        let cut_before = snippet.position > u64::from(SEARCH_SNIPPET.before) + 1;
        let cut_after = snippet.bytes.len() >= SEARCH_SNIPPET.len as usize;
        let text = snippet_text(&snippet.bytes);
        let ranges = highlights(&text, tokens, phrase, cut_before, cut_after);
        if ranges.is_empty() {
            continue;
        }
        found.push(SearchMatch {
            field: snippet.field,
            fragments: fragments(&text, &ranges),
            cut_before,
            cut_after,
        });
    }
    let verified = !phrase || hit.metadata || !found.is_empty();
    if hit.metadata {
        found.push(SearchMatch {
            field: SearchField::Metadata,
            fragments: Vec::new(),
            cut_before: false,
            cut_after: false,
        });
    }
    (found, verified)
}

impl QueryService {
    /// One page of a full-text search (contract 8.7): the hits of the text indexes of `span_payload` in the
    /// window, newest first, each with its core row and highlights. The first page fixes `as_of`; later pages
    /// read only rows that arrived by then.
    pub async fn search(
        &self,
        ctx: &ReadContext,
        query: SearchQuery,
    ) -> Result<SearchPage, QueryServiceError> {
        let mut read = self.read(ctx, "search");
        let scope = ctx.scope;
        let limit = query.limit.unwrap_or(SEARCH_DEFAULT_LIMIT);
        if limit == 0 || limit > SEARCH_MAX_LIMIT {
            return Err(QueryServiceError::Validation("limit"));
        }
        if query.fields.is_empty() {
            return Err(QueryServiceError::Validation("fields"));
        }
        let (as_of, after) = match query.cursor {
            Some(cursor) => {
                let (as_of, last) = cursor.check(scope, query.window)?;
                (as_of, Some(last))
            }
            None => (read.now, None),
        };
        let base = self.consistency();

        let text_fields: Vec<SearchField> = query
            .fields
            .iter()
            .copied()
            .filter(|f| f.lowered())
            .collect();
        let metadata = query.fields.contains(&SearchField::Metadata);
        let text_tokens = search_tokens(&query.text, SearchField::Input, query.mode);
        let mut sources = Vec::with_capacity(2);
        let groups = [
            (text_fields.clone(), false),
            (
                metadata
                    .then_some(SearchField::Metadata)
                    .into_iter()
                    .collect(),
                true,
            ),
        ];
        for (fields, is_metadata) in groups {
            let Some(first) = fields.first() else {
                continue;
            };
            // A kind of field without tokens is not searched; a query must have tokens for one kind.
            let tokens = search_tokens(&query.text, *first, query.mode);
            if tokens.len() > SEARCH_MAX_TOKENS {
                return Err(QueryServiceError::Validation("query"));
            }
            if tokens.is_empty() {
                continue;
            }
            // Metadata entries have no order: a phrase over them is all of them.
            let mode = match (is_metadata, query.mode) {
                (true, TokenMode::Phrase) => TokenMode::All,
                (_, mode) => mode,
            };
            let request = SearchRequest {
                tokens,
                mode,
                fields: fields.clone(),
                filter: Filter::all(),
                limit,
                cursor: after,
                as_of,
            };
            let plan = search(scope, query.window, &read.clamp, ctx.caller, &request)?;
            let rows = read.rows(Ok(plan.map(|plan| plan.hits))).await?;
            // The text plan's snippet columns follow its fields; metadata has none.
            let snippet_fields: &[SearchField] = if is_metadata { &[] } else { &fields };
            let hits = rows
                .iter()
                .map(|row| Hit::decode(row, snippet_fields, is_metadata))
                .collect::<Result<Vec<_>, _>>()?;
            sources.push(hits);
        }
        if sources.is_empty() {
            return Err(QueryServiceError::Validation("query"));
        }
        let (page, has_more) = merge(sources, limit as usize);

        let spans = read.page_spans(query.window, &page).await?;
        let data = page
            .iter()
            .filter_map(|hit| {
                let span = spans.get(&hit.key)?.clone();
                let (matches, verified) = matches(hit, &text_tokens, query.mode);
                verified.then_some(SearchHit { span, matches })
            })
            .collect();
        let next_cursor = match page.last() {
            Some(last) if has_more => Some(SearchCursor::from_parts(
                scope.project_id(),
                query.window,
                as_of,
                last.key,
            )),
            _ => None,
        };

        // The index holds no payloads: spans it has that the store may not serve yet are left out.
        let watermark = self.index.read_state(scope).watermark;
        let (unconfirmed, state) = self.index.since(scope, watermark);
        let left_out = read.retained(unconfirmed)?.iter().any(|span| {
            span.lsn > state.watermark
                && span.received_at <= as_of
                && query.window.contains(span.summary.start_time)
        });
        let consistency = if left_out { Consistency::Lagging } else { base };
        let meta = read.finish(as_of, consistency, state, &SEARCH_TABLES, false)?;
        Ok(SearchPage {
            data,
            next_cursor,
            has_more,
            meta,
        })
    }
}

impl Read<'_> {
    /// The core rows of a page of hits, by key. Of several rows of one span, the one with the greatest LSN.
    async fn page_spans(
        &mut self,
        window: TimeWindow,
        page: &[Hit],
    ) -> Result<BTreeMap<SearchKey, TraceSpan>, QueryServiceError> {
        let mut spans: BTreeMap<SearchKey, (u64, TraceSpan)> = BTreeMap::new();
        if page.is_empty() {
            return Ok(BTreeMap::new());
        }
        let keys: Vec<SearchKey> = page.iter().map(|hit| hit.key).collect();
        let (scope, caller) = (self.ctx.scope, self.ctx.caller);
        let plan = search_spans(scope, window, &self.clamp, caller, &keys, &Filter::all());
        let at = |column: SpanCoreCol| {
            SpanCoreCol::ALL
                .iter()
                .position(|c| *c == column)
                .unwrap_or(usize::MAX)
        };
        let (lsn_at, hash_at) = (at(SpanCoreCol::Lsn), at(SpanCoreCol::TraceHash));
        for row in self.rows(plan).await? {
            let f = Fields(row.values());
            let lsn = f.uint(lsn_at)?;
            // The key as the store holds it, so the join is exact.
            let hash = u32::try_from(f.uint(hash_at)?).map_err(|_| {
                QueryServiceError::Internal("a span's trace hash is out of range".to_owned())
            })?;
            let Some(span) = stored(vec![row])?.pop() else {
                continue;
            };
            let s = &span.summary;
            let key = (s.start_time.floor_minute(), hash, s.span_id);
            // The lists of the plan are a superset: keep only the rows of the page's own keys.
            if !keys.contains(&key) {
                continue;
            }
            match spans.entry(key) {
                Entry::Occupied(mut entry) if entry.get().0 < lsn => {
                    entry.insert((lsn, span));
                }
                Entry::Occupied(_) => {}
                Entry::Vacant(entry) => {
                    entry.insert((lsn, span));
                }
            }
        }
        Ok(spans
            .into_iter()
            .map(|(key, (_, span))| (key, span))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::SpanId;
    use proptest::prelude::*;

    use super::*;

    fn key(minute: i64, id: u8) -> SearchKey {
        (
            Timestamp::from_micros(minute * 60_000_000),
            7,
            SpanId::from_bytes([id; 8]),
        )
    }

    fn hit(minute: i64, id: u8) -> Hit {
        Hit {
            key: key(minute, id),
            snippets: Vec::new(),
            metadata: false,
        }
    }

    fn texts(fragments: &[Fragment]) -> Vec<(&str, bool)> {
        fragments
            .iter()
            .map(|f| (f.text.as_str(), f.highlighted))
            .collect()
    }

    fn lowered(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn tokens_follow_each_index() {
        let text = "Rate-limit EXCEEDED, h\u{e9}llo rate";
        assert_eq!(
            search_tokens(text, SearchField::Input, TokenMode::All),
            ["rate", "limit", "exceeded", "h\u{e9}llo"]
        );
        assert_eq!(
            search_tokens(text, SearchField::Output, TokenMode::Phrase),
            ["rate", "limit", "exceeded", "h\u{e9}llo", "rate"]
        );
        assert_eq!(
            search_tokens(
                " tenant=Acme  env=prod acme tenant=Acme",
                SearchField::Metadata,
                TokenMode::Phrase
            ),
            ["tenant=Acme", "env=prod"]
        );
        assert!(search_tokens("--- !!", SearchField::Input, TokenMode::Any).is_empty());
    }

    #[test]
    fn highlights_mark_whole_tokens_only() {
        let text = "accurate rate: Rate limited";
        let ranges = highlights(text, &lowered(&["rate"]), false, false, false);
        assert_eq!(
            texts(&fragments(text, &ranges)),
            [
                ("accurate ", false),
                ("rate", true),
                (": ", false),
                ("Rate", true),
                (" limited", false)
            ]
        );
        // A phrase is one fragment from its first token to its last, separators included.
        let phrase = lowered(&["rate", "limit", "exceeded"]);
        let text = "rate; RATE-limit exceeded rate limit";
        let ranges = highlights(text, &phrase, true, false, false);
        assert_eq!(
            texts(&fragments(text, &ranges)),
            [
                ("rate; ", false),
                ("RATE-limit exceeded", true),
                (" rate limit", false)
            ]
        );
        // A token at an edge the snippet cut is not trusted.
        let text = "rate limit rate";
        assert_eq!(
            highlights(text, &lowered(&["rate"]), false, true, true),
            Vec::<(usize, usize)>::new()
        );
    }

    #[test]
    fn snippets_drop_cut_characters_at_both_ends() {
        let bytes = "\u{e9}t\u{e9}".as_bytes();
        assert_eq!(snippet_text(&bytes[1..bytes.len() - 1]), "t");
        assert_eq!(snippet_text(b"plain"), "plain");
        assert_eq!(snippet_text(&[0xff, b'a']), "\u{fffd}a");
    }

    #[test]
    fn a_phrase_hit_needs_the_phrase_in_a_snippet() {
        let snippet = |text: &str| Snippet {
            field: SearchField::Input,
            position: 1,
            bytes: text.as_bytes().to_vec(),
        };
        let phrase = lowered(&["rate", "limit"]);
        let mut found = hit(1, 1);
        found.snippets = vec![snippet("limit rate")];
        assert!(!matches(&found, &phrase, TokenMode::Phrase).1);
        found.snippets.push(Snippet {
            field: SearchField::Output,
            ..snippet("a rate limit")
        });
        let (matches_found, verified) = matches(&found, &phrase, TokenMode::Phrase);
        assert!(verified);
        assert_eq!(matches_found.len(), 1);
        assert_eq!(matches_found[0].field, SearchField::Output);
        // Without a needle the snippet is the field's start: no token of it counts, but a phrase written with
        // another separator does.
        found.snippets = vec![Snippet {
            position: 0,
            ..snippet("rate limit")
        }];
        assert!(matches(&found, &phrase, TokenMode::All).0.is_empty());
        found.snippets = vec![Snippet {
            position: 0,
            ..snippet("the rate-limit")
        }];
        assert!(matches(&found, &phrase, TokenMode::Phrase).1);
    }

    #[test]
    fn the_merge_stops_at_the_newest_frontier() {
        // Text hits in minutes 9, 8, 7 (full at a limit of 2), metadata hits in 9 and 3.
        let text = vec![hit(9, 1), hit(8, 1), hit(7, 1)];
        let mut metadata = vec![hit(9, 1), hit(3, 1)];
        metadata.iter_mut().for_each(|h| h.metadata = true);
        let (page, more) = merge(vec![text, metadata], 2);
        assert!(more);
        assert_eq!(
            page.iter().map(|h| h.key).collect::<Vec<_>>(),
            [key(9, 1), key(8, 1)]
        );
        assert!(page[0].metadata);
        // Neither full: everything, and nothing more.
        let (page, more) = merge(vec![vec![hit(5, 2)], vec![hit(5, 1)]], 2);
        assert!(!more);
        assert_eq!(page.len(), 2);
        // A full source whose last hit is the newest frontier caps the page below the limit.
        let (page, more) = merge(
            vec![vec![hit(9, 1), hit(9, 1), hit(8, 1)], vec![hit(7, 1)]],
            2,
        );
        assert!(more);
        assert_eq!(
            page.iter().map(|h| h.key).collect::<Vec<_>>(),
            [key(9, 1), key(8, 1)]
        );
    }

    proptest! {
        #[test]
        fn fragments_spell_the_snippet_and_mark_only_tokens(
            bytes in proptest::collection::vec(any::<u8>(), 0..300),
            tokens in proptest::collection::vec("[a-z0-9]{1,4}", 1..4),
            phrase: bool,
            cut_before: bool,
            cut_after: bool,
        ) {
            let text = snippet_text(&bytes);
            let ranges = highlights(&text, &tokens, phrase, cut_before, cut_after);
            let pieces = fragments(&text, &ranges);
            prop_assert_eq!(pieces.iter().map(|f| f.text.as_str()).collect::<String>(), text.clone());
            for piece in &pieces {
                prop_assert!(!piece.text.is_empty());
                if piece.highlighted && !phrase {
                    prop_assert!(tokens.contains(&piece.text.to_ascii_lowercase()));
                }
            }
            for pair in pieces.windows(2) {
                prop_assert!(pair[0].highlighted || pair[1].highlighted);
            }
        }

        #[test]
        fn merged_pages_are_newest_first_and_never_skip_a_hit(
            a in proptest::collection::btree_set((0i64..20, 0u8..3), 0..12),
            b in proptest::collection::btree_set((0i64..20, 0u8..3), 0..12),
            limit in 1usize..6,
        ) {
            // Page through both sources as the plans would: each its first `limit + 1` after the cursor.
            let all: std::collections::BTreeSet<SearchKey> =
                a.iter().chain(&b).map(|(m, id)| key(*m, *id)).collect();
            let source = |keys: &std::collections::BTreeSet<(i64, u8)>, after: Option<SearchKey>| {
                let mut hits: Vec<Hit> = keys.iter().map(|(m, id)| hit(*m, *id)).collect();
                hits.sort_by_key(|x| std::cmp::Reverse(x.key));
                hits.retain(|h| after.is_none_or(|after| h.key < after));
                hits.truncate(limit + 1);
                hits
            };
            let mut seen = Vec::new();
            let mut after = None;
            for _ in 0..100 {
                let (page, more) = merge(vec![source(&a, after), source(&b, after)], limit);
                prop_assert!(page.len() <= limit);
                prop_assert!(page.windows(2).all(|p| p[0].key > p[1].key));
                seen.extend(page.iter().map(|h| h.key));
                if !more {
                    break;
                }
                prop_assert!(!page.is_empty());
                after = page.last().map(|h| h.key);
            }
            let expected: Vec<SearchKey> = all.into_iter().rev().collect();
            prop_assert_eq!(seen, expected);
        }
    }
}
