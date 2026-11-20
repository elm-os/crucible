//! The span list (contract 8.7): one page of spans, newest first, under the list filter.

use std::collections::BTreeMap;

use crucible_plan::{Filter, FilterTarget, SpanListRequest, span_list};
use crucible_recent::SpanSummary;
use crucible_records::{Lsn, SpanId, TableId, TimeWindow, Timestamp, TraceId};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::SpanListCursor;
use crate::decode;
use crate::heads::page_limit;
use crate::service::{QueryService, lagging_if};

/// The tables the span list depends on, for the quarantine rule.
const SPAN_LIST_TABLES: [TableId; 1] = [TableId::SpanCore];

/// A request for one page of the span list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanListQuery {
    /// The window over the spans' start times.
    pub window: TimeWindow,
    /// The filter; [`Filter::all`] for none.
    pub filter: Filter,
    /// The page size; default 50, at most 200.
    pub limit: Option<u32>,
    /// The cursor of the previous page.
    pub cursor: Option<SpanListCursor>,
}

/// One page of the span list, newest first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanListPage {
    /// The spans.
    pub data: Vec<SpanSummary>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<SpanListCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The response metadata.
    pub meta: Meta,
}

/// The order of the list, newest first; the trace id only separates spans that share the sort tuple.
type Key = (Timestamp, SpanId, TraceId);

impl QueryService {
    /// One page of the span list (contract 8.7): a keyset scan of `span_core`, newest first by
    /// `(start_time, span_id)`, merged with the spans of the recent index that the scan may not return yet.
    ///
    /// The first page fixes `as_of`; later pages read only spans that arrived by then, from the store and from
    /// the index, so pages do not shift. A filtered request reads the store only, because the index cannot
    /// evaluate every field, and answers `consistency: lagging` when the index holds matching-window spans
    /// that the store may not hold yet. A span the store returns twice (a client retry) is shown once.
    pub async fn span_list(
        &self,
        ctx: &ReadContext,
        query: SpanListQuery,
    ) -> Result<SpanListPage, QueryServiceError> {
        let mut read = self.read(ctx, "span_list");
        let limit = page_limit(query.limit)?;
        let (as_of, after) = match &query.cursor {
            Some(cursor) => {
                let (as_of, last) = cursor.check(ctx.scope, query.window)?;
                (as_of, Some(last))
            }
            None => (read.now, None),
        };
        let base = self.consistency();
        // Every entry the index holds for the project: those the store may not hold yet, and those still
        // inside the visibility grace.
        let (entries, state) = self.index.since(ctx.scope, Lsn::ZERO);
        let window = read.clamp.window(TableId::SpanCore, query.window)?;
        let entries: Vec<_> = read
            .retained(entries)?
            .into_iter()
            .filter(|span| {
                let s = &span.summary;
                window.is_some_and(|window| window.contains(s.start_time))
                    && span.received_at <= as_of
                    && after.is_none_or(|after| (s.start_time, s.span_id) < after)
            })
            .collect();
        let filtered = !query.filter.is_all();
        let left_out = filtered && entries.iter().any(|span| span.lsn > state.watermark);
        // Metadata conditions are evaluated on `span_payload`.
        let tables: &[TableId] = if query.filter.split(FilterTarget::SpanCore).1.is_all() {
            &SPAN_LIST_TABLES
        } else {
            &[TableId::SpanCore, TableId::SpanPayload]
        };

        let request = SpanListRequest {
            filter: query.filter,
            limit,
            cursor: after,
            as_of,
        };
        let plan = span_list(ctx.scope, query.window, &read.clamp, ctx.caller, request);
        let rows = read.rows(plan).await?;
        let mut merged: BTreeMap<Key, SpanSummary> = BTreeMap::new();
        for row in &rows {
            let span = decode::span(row)?;
            merged
                .entry((span.start_time, span.span_id, span.trace_id))
                .or_insert(span);
        }
        let from_index = !filtered && !entries.is_empty();
        if !filtered {
            for span in entries {
                let s = span.summary;
                merged
                    .entry((s.start_time, s.span_id, s.trace_id))
                    .or_insert(s);
            }
        }
        let mut data: Vec<SpanSummary> = merged.into_values().rev().collect();
        // The store read one row more than the page; repeats of a span can hide that row in the merge.
        let has_more = data.len() > limit as usize || rows.len() > limit as usize;
        data.truncate(limit as usize);
        let meta = read.finish(as_of, lagging_if(base, left_out), state, tables, from_index)?;
        let project_id = ctx.scope.project_id();
        let next_cursor = data.last().filter(|_| has_more).map(|last| {
            let position = (last.start_time, last.span_id);
            SpanListCursor::from_parts(project_id, query.window, as_of, position)
        });
        Ok(SpanListPage {
            data,
            next_cursor,
            has_more,
            meta,
        })
    }
}
