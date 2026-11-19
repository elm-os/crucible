//! The trace list (contract 8.7): one page of trace heads, newest first, with each trace's merged aggregates.

use crucible_plan::{Pred, TRACE_LIST_OVER_READ, TraceListRequest};
use crucible_records::{TableId, TimeWindow};

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::{HeadCursor, HeadKey, PageCursor};
use crate::heads::{Candidate, LIST_TABLES, TraceListItem, consistency, newest_first, page_limit};
use crate::service::QueryService;

/// A page reads the store at most this many times. Past it, a page of mostly hidden provisional heads comes
/// back short, with a cursor at the last head read.
const MAX_ROUNDS: usize = 4;

/// A request for one page of the trace list.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceListQuery {
    /// The window over the heads' `start_time`.
    pub window: TimeWindow,
    /// The filter over `trace_heads` columns; `Pred::all()` for none.
    pub filter: Pred,
    /// The page size; default 50, at most 200.
    pub limit: Option<u32>,
    /// The cursor of the previous page.
    pub cursor: Option<PageCursor>,
}

/// One page of the trace list, newest first.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceListPage {
    /// The traces.
    pub data: Vec<TraceListItem>,
    /// The cursor of the next page, when there is one.
    pub next_cursor: Option<PageCursor>,
    /// Whether another page exists.
    pub has_more: bool,
    /// On a first page: the cursor a live-tail request passes as `since`.
    pub head_cursor: Option<HeadCursor>,
    /// The response metadata.
    pub meta: Meta,
}

/// Cuts a full read back to the last sort tuple it is known to hold completely. Two heads of one trace can
/// share a tuple (a provisional head and a root that start in the same microsecond), and the scan may have
/// stopped between them; resuming strictly after the tuple would then skip the root for good. The rows of the
/// last tuple are dropped here and read again by the next scan, which resumes after the tuple before it.
fn trim_boundary(chunk: &mut Vec<Candidate>) {
    let Some(last) = chunk.last().map(|c| c.head.key()) else {
        return;
    };
    let whole = chunk.iter().take_while(|c| c.head.key() != last).count();
    // A read that holds one tuple only cannot be cut; it stays, so that paging still moves on.
    if whole > 0 {
        chunk.truncate(whole);
    }
}

impl QueryService {
    /// One page of the trace list (contract 8.7): a keyset scan of `trace_heads` in key order, merged with the
    /// heads in the recent index, then the merged `trace_stats` of the page's traces, without the hidden
    /// provisional heads.
    ///
    /// The first page fixes `as_of`; later pages read only heads that arrived by then, from the store and from
    /// the index, so pages do not shift. A request with a filter reads the store only, and answers
    /// `consistency: lagging` when the index holds heads the store may not serve yet.
    pub async fn trace_list(
        &self,
        ctx: &ReadContext,
        query: TraceListQuery,
    ) -> Result<TraceListPage, QueryServiceError> {
        // The applied LSN is read before the clock, so every frame at or below it was stamped by `as_of`: a head
        // this page leaves out for arriving after `as_of` is above the head cursor's LSN.
        let applied = self.index.applied();
        let mut read = self.read(ctx);
        let scope = ctx.scope;
        let limit = page_limit(query.limit)?;
        let (as_of, mut position) = match query.cursor {
            Some(cursor) => {
                let (as_of, last) = cursor.check(scope, query.window)?;
                (as_of, Some(last))
            }
            None => (read.now, None),
        };
        let base = self.consistency();
        let (entries, state) = match read.clamp.window(TableId::TraceHeads, query.window)? {
            Some(window) => self.index.heads(scope, window),
            None => (Vec::new(), self.index.read_state(scope)),
        };
        let after = position;
        let mut recent = read.recent_heads(entries, state, &query.filter, |span| {
            let s = &span.summary;
            let key: HeadKey = (s.start_time, s.trace_id.trace_hash(), s.trace_id);
            span.received_at <= as_of && after.is_none_or(|after| key < after)
        })?;

        let over_read = (limit * TRACE_LIST_OVER_READ) as usize;
        let mut data = Vec::new();
        let mut exhausted = false;
        for _ in 0..MAX_ROUNDS {
            let request = TraceListRequest {
                filter: query.filter.clone(),
                limit,
                cursor: position,
                as_of,
                since: None,
            };
            let mut chunk = read.store_heads(query.window, request).await?;
            exhausted = chunk.len() < over_read;
            if !exhausted {
                trim_boundary(&mut chunk);
            }
            position = chunk.last().map(|c| c.head.key()).or(position);
            // The index heads that sort at or before the last store row read belong to this chunk; with the
            // store exhausted, all of them do.
            let low = if exhausted { None } else { position };
            while let Some(head) = recent.heads.pop_front() {
                if low.is_some_and(|low| head.head.key() < low) {
                    recent.heads.push_front(head);
                    break;
                }
                chunk.push(head);
            }
            read.visible(newest_first(chunk), &mut data).await?;
            if exhausted || data.len() > limit as usize {
                break;
            }
        }
        let full = data.len() > limit as usize;
        data.truncate(limit as usize);
        read.decorate(&mut data).await?;
        let has_more = full || !exhausted;
        let next = if full {
            data.last().map(|item| item.head.key())
        } else {
            position.filter(|_| !exhausted)
        };
        let meta = read.finish(
            as_of,
            consistency(base, &recent),
            recent.state,
            &LIST_TABLES,
            recent.used,
        )?;
        let project_id = scope.project_id();
        Ok(TraceListPage {
            data,
            next_cursor: next
                .map(|last| PageCursor::from_parts(project_id, query.window, as_of, last)),
            has_more,
            head_cursor: query.cursor.is_none().then(|| {
                HeadCursor::from_parts(project_id, query.window, as_of, self.wal_epoch, applied)
            }),
            meta,
        })
    }
}
