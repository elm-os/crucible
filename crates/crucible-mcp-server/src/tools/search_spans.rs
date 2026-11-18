//! `search_spans`: full-text search over the spans' input and output, and over metadata entries (contract 8.7),
//! newest first, each hit with highlighted fragments around its first match.

use crucible_cursor::{CursorError, FilterHash, Position, Route, Sort};
use crucible_plan::{SearchField, SearchKey, TokenMode};
use crucible_query::{SEARCH_MAX_TOKENS, SearchCursor, SearchHit, SearchQuery, search_tokens};
use crucible_records::{TimeWindow, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ToolError;
use crate::shape::{Meta, SpanRow, fit, meta};
use crate::tools::{Call, Check, Tool, check_window};

/// The most hits one call returns.
const MAX_ROWS: u32 = 20;
/// The longest query, in bytes, as the REST search takes it.
const MAX_QUERY_BYTES: usize = 1_024;
/// The order of the hits.
const SORT: Sort = Sort::NewestFirst;

pub(crate) struct SearchSpans;

/// A field to search.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FieldArg {
    /// The input: its words, without regard to ASCII case.
    Input,
    /// The output, like the input.
    Output,
    /// Metadata entries: each term of the query is one whole `key=value` entry, case included.
    Metadata,
}

impl FieldArg {
    const fn plan(self) -> SearchField {
        match self {
            Self::Input => SearchField::Input,
            Self::Output => SearchField::Output,
            Self::Metadata => SearchField::Metadata,
        }
    }
}

/// How the words of the query combine.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModeArg {
    /// Every word.
    All,
    /// Any word.
    Any,
    /// The words in order, next to each other.
    Phrase,
}

/// The arguments.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Args {
    /// The start of the window over the spans' start, RFC 3339 (inclusive). Required.
    from: String,
    /// The end of the window, RFC 3339 (exclusive). Required; at most the project's maximum window after `from`.
    to: String,
    /// What to find: words for `input` and `output`, `key=value` entries for `metadata`; at most 1,024 bytes.
    query: String,
    /// The fields to search; `input` and `output` when absent.
    #[serde(default)]
    fields: Option<Vec<FieldArg>>,
    /// How the words combine; `all` when absent.
    #[serde(default)]
    mode: Option<ModeArg>,
    /// Hits per page, from 1 to 20; 20 when absent.
    #[serde(default)]
    limit: Option<u32>,
    /// The `next_cursor` of the previous page, with the same other arguments.
    #[serde(default)]
    cursor: Option<String>,
}

/// A run of a snippet's text.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct Fragment {
    /// The text.
    text: String,
    /// Whether it is a word of the query, or in a phrase search the phrase.
    highlighted: bool,
}

/// Where a hit matched.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct Match {
    /// `input`, `output` or `metadata`.
    field: &'static str,
    /// The snippet around the field's first match, in order; empty for a metadata match.
    fragments: Vec<Fragment>,
    /// Whether the field goes on before the snippet.
    cut_before: bool,
    /// Whether the field may go on after the snippet.
    cut_after: bool,
}

/// One hit.
#[derive(Clone, Serialize, JsonSchema)]
pub(crate) struct Hit {
    /// The span's trace id; get_trace reads the whole trace.
    trace_id: String,
    /// The span, with previews of its input and output.
    span: SpanRow,
    /// The fields with a highlighted match; empty when the matches lie outside the snippets.
    matches: Vec<Match>,
}

impl Hit {
    fn new(hit: &SearchHit) -> Self {
        Self {
            trace_id: hit.span.summary.trace_id.to_string(),
            span: SpanRow::new(&hit.span),
            matches: hit
                .matches
                .iter()
                .map(|found| Match {
                    field: found.field.as_str(),
                    fragments: found
                        .fragments
                        .iter()
                        .map(|fragment| Fragment {
                            text: fragment.text.clone(),
                            highlighted: fragment.highlighted,
                        })
                        .collect(),
                    cut_before: found.cut_before,
                    cut_after: found.cut_after,
                })
                .collect(),
        }
    }
}

/// One page of hits, newest first.
#[derive(Serialize, JsonSchema)]
pub(crate) struct Output {
    /// The hits. A page may hold fewer hits than its size while `has_more` is true.
    data: Vec<Hit>,
    /// Pass it as `cursor`, with the same other arguments, for the next page; null on the last page.
    next_cursor: Option<String>,
    /// Whether another page exists.
    has_more: bool,
    /// True when this page was cut to keep the result under 64 KiB; `next_cursor` continues after its last hit.
    truncated: bool,
    /// How complete and how fresh the page is: `lagging` while spans of the window are not searchable yet.
    meta: Meta,
}

impl Tool for SearchSpans {
    const NAME: &'static str = "search_spans";
    const DESCRIPTION: &'static str = "Search the text of the project's spans that started in the window [from, \
        to): the words of their input and output, or whole `key=value` metadata entries, newest first. Each hit \
        is a span with its trace id, previews of its input and output, and highlighted fragments around its first \
        match. Words combine as all (default), any or a phrase. At most 20 hits and 64 KiB per call; pass \
        `next_cursor` back as `cursor`, with the same other arguments, for more.";
    type Args = Args;
    type Output = Output;

    async fn call(call: &Call<'_>, args: Args) -> Result<Output, ToolError> {
        let mut check = Check::default();
        let window = check.window(&args.from, &args.to);
        let limit = check.limit(args.limit, MAX_ROWS, MAX_ROWS);
        let mode = match args.mode.unwrap_or(ModeArg::All) {
            ModeArg::All => TokenMode::All,
            ModeArg::Any => TokenMode::Any,
            ModeArg::Phrase => TokenMode::Phrase,
        };
        let fields: Vec<SearchField> = args
            .fields
            .unwrap_or_else(|| vec![FieldArg::Input, FieldArg::Output])
            .into_iter()
            .map(FieldArg::plan)
            .collect();
        check_terms(&mut check, &args.query, &fields, mode);
        check.finish()?;
        let Some(window) = window else {
            return Err(ToolError::validation(Vec::new()));
        };
        check_window(window, call.settings.max_window_days)?;
        let mut query = SearchQuery {
            window,
            text: args.query,
            mode,
            fields,
            limit: Some(limit),
            cursor: None,
        };
        let filters = FilterHash::of(&query.canonical());
        query.cursor = args
            .cursor
            .map(|text| open(call, &text, filters, window))
            .transpose()
            .map_err(ToolError::cursor)?;
        let page = call.service.search(&call.ctx, query).await;
        let page = page.map_err(|e| ToolError::query(&e, call.request_id))?;
        let rows: Vec<(SearchKey, Hit)> = page
            .data
            .iter()
            .map(|hit| {
                let s = &hit.span.summary;
                let key = (
                    s.start_time.floor_minute(),
                    s.trace_id.trace_hash(),
                    s.span_id,
                );
                (key, Hit::new(hit))
            })
            .collect();
        // A cut page resumes after its last hit, with the `as_of` that the first page fixed.
        let as_of = page.meta.as_of;
        let next = |cursor: &SearchCursor| {
            let (_, window, as_of, last) = cursor.parts();
            seal(call, filters, window, as_of, last)
        };
        Ok(fit(
            rows,
            |(_, hit)| hit.span.lighten(),
            |prefix, cut| Output {
                data: prefix.iter().map(|(_, hit)| hit.clone()).collect(),
                next_cursor: match prefix.last() {
                    Some((last, _)) if cut => Some(seal(call, filters, window, as_of, *last)),
                    _ => page.next_cursor.as_ref().map(next),
                },
                has_more: cut || page.has_more,
                truncated: cut,
                meta: meta(&page.meta),
            },
        ))
    }
}

/// The checks of the REST search on the query and the fields: each field once, at least one, and a query that
/// gives the fields' kinds something to search for, within the caps.
fn check_terms(check: &mut Check, text: &str, fields: &[SearchField], mode: TokenMode) {
    if fields.is_empty() {
        let message =
            "name at least one of `input`, `output` and `metadata`, or leave `fields` out";
        check.error("fields", "required", message);
    }
    if fields
        .iter()
        .enumerate()
        .any(|(at, field)| fields[..at].contains(field))
    {
        check.error("fields", "repeated", "name each field once");
    }
    if text.len() > MAX_QUERY_BYTES {
        let message = format!("the query is longer than {MAX_QUERY_BYTES} bytes; shorten it");
        check.error("query", "too_long", &message);
        return;
    }
    if fields.is_empty() {
        return;
    }
    // Text fields and metadata take different tokens: a kind without any is not searched, but one kind must
    // have some, and none too many.
    let counts: Vec<usize> = [SearchField::Input, SearchField::Metadata]
        .into_iter()
        .filter(|kind| fields.iter().any(|field| field.lowered() == kind.lowered()))
        .map(|kind| search_tokens(text, kind, mode).len())
        .collect();
    if counts.iter().all(|count| *count == 0) {
        let message = "the query has nothing to search for: `input` and `output` need a letter or a digit, \
                       `metadata` a `key=value` entry";
        check.error("query", "no_tokens", message);
    } else if counts.iter().any(|count| *count > SEARCH_MAX_TOKENS) {
        let message = format!("the query has more than {SEARCH_MAX_TOKENS} words; shorten it");
        check.error("query", "too_many_tokens", &message);
    }
}

/// The cursor of the page after `last`, on the REST search's route and bound to the query's canonical form.
fn seal(
    call: &Call<'_>,
    filters: FilterHash,
    window: TimeWindow,
    as_of: Timestamp,
    last: SearchKey,
) -> String {
    let position = Position::SearchPage {
        window,
        as_of,
        last,
    };
    call.cursors.seal(filters, SORT, position)
}

fn open(
    call: &Call<'_>,
    text: &str,
    filters: FilterHash,
    window: TimeWindow,
) -> Result<SearchCursor, CursorError> {
    match call
        .cursors
        .open(text, Route::Search, filters, SORT, Some(window))?
    {
        Position::SearchPage {
            window,
            as_of,
            last,
        } => Ok(SearchCursor::from_parts(
            call.cursors.project_id,
            window,
            as_of,
            last,
        )),
        _ => Err(CursorError::WrongKind),
    }
}
