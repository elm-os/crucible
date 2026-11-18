//! The paged children of a span (contract 8.8).

use crucible_plan::{SPAN_CHILDREN_DEFAULT_LIMIT, SPAN_CHILDREN_MAX_LIMIT, span_children};
use crucible_records::{SpanId, TraceId};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::ChildCursor;
use crate::service::QueryService;
use crate::spans::{SPAN_TABLES, TraceSpan, merge, recent, stored};

/// A request for one page of the direct children of a span.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanChildrenQuery {
    /// The trace.
    pub trace_id: TraceId,
    /// The parent span.
    pub parent: SpanId,
    /// The cursor of the previous page.
    pub cursor: Option<ChildCursor>,
    /// The page size; default 500, at most 2,000.
    pub limit: Option<u32>,
}

/// One page of the direct children of a span, in `(start_time, span_id)` order. Each carries `child_count`.
#[derive(Clone, PartialEq, Debug)]
pub struct SpanChildrenPage {
    /// The children.
    pub data: Vec<TraceSpan>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<ChildCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// The response metadata.
    pub meta: Meta,
}

impl QueryService {
    /// One page of the direct children of a span (contract 8.8), merged with the recent index and
    /// de-duplicated by `span_id`.
    pub async fn span_children(
        &self,
        ctx: &ReadContext,
        query: SpanChildrenQuery,
    ) -> Result<SpanChildrenPage, QueryServiceError> {
        let mut read = self.read(ctx);
        let (scope, caller) = (ctx.scope, ctx.caller);
        let SpanChildrenQuery {
            trace_id, parent, ..
        } = query;
        let limit = query.limit.unwrap_or(SPAN_CHILDREN_DEFAULT_LIMIT);
        if limit == 0 || limit > SPAN_CHILDREN_MAX_LIMIT {
            return Err(QueryServiceError::Validation("limit"));
        }
        let after = match query.cursor {
            Some(cursor) => Some(cursor.check(scope, trace_id, parent)?),
            None => None,
        };
        let consistency = self.consistency();
        let (entries, state) = self.index.trace(scope, trace_id);
        let entries = read.retained(entries)?;
        let known = !entries.is_empty();
        let children = entries
            .iter()
            .filter(|span| span.summary.parent_span_id == parent)
            .map(recent);

        let bounds = read.stored_trace(trace_id).await?.map(|trace| trace.bounds);
        let rows = match bounds {
            Some(trace) => {
                let plan = span_children(
                    scope,
                    trace,
                    &read.clamp,
                    caller,
                    parent,
                    after,
                    Some(limit),
                );
                read.rows(plan).await?
            }
            None => Vec::new(),
        };
        // The store's rows are its first `limit + 1` after the cursor, so the first `limit` of the merge are
        // the first `limit` of the whole.
        let mut data = merge(stored(rows)?, children);
        data.retain(|span| after.is_none_or(|after| span.key() > after));
        let has_more = data.len() > limit as usize;
        data.truncate(limit as usize);
        read.count_children(bounds, &mut data, &entries, state)
            .await?;
        let meta = read.finish(read.now, consistency, state, &SPAN_TABLES, known)?;
        if bounds.is_none() && !known && meta.is_whole() {
            return Err(QueryServiceError::NotFound);
        }
        let next_cursor = match data.last() {
            Some(last) if has_more => Some(ChildCursor::from_parts(
                scope.project_id(),
                trace_id,
                parent,
                last.key(),
            )),
            _ => None,
        };
        Ok(SpanChildrenPage {
            data,
            next_cursor,
            has_more,
            meta,
        })
    }
}
