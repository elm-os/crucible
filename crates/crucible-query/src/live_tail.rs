//! Live tail (contract 8.8): the traces whose head arrived after a head cursor, newest first.

use crucible_plan::{CmpOp, Pred, TRACE_LIST_OVER_READ, TraceListRequest};
use crucible_records::{Lsn, TableId, TimeWindow, Timestamp, WalEpoch};
use crucible_schema::TraceHeadsCol;

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::HeadCursor;
use crate::heads::{LIST_TABLES, TraceListItem, consistency, newest_first, page_limit};
use crate::service::QueryService;

/// `new_count` of a live-tail response saturates here.
pub const NEW_COUNT_MAX: u32 = 100;

/// A live-tail request: what arrived after a head cursor, with unchanged window and filters.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceTailQuery {
    /// The window of the list the cursor came from.
    pub window: TimeWindow,
    /// The filter of that list.
    pub filter: Pred,
    /// The most traces to return; default 50, at most 200.
    pub limit: Option<u32>,
    /// The head cursor of the first page, or of the previous live-tail response.
    pub since: HeadCursor,
}

/// A live-tail response.
#[derive(Clone, PartialEq, Debug)]
pub struct TraceTail {
    /// The traces whose head arrived after the cursor, newest first, at most `limit`.
    pub data: Vec<TraceListItem>,
    /// How many arrived, saturating at [`NEW_COUNT_MAX`].
    pub new_count: u32,
    /// Whether more arrived than `data` holds.
    pub has_more: bool,
    /// The cursor of the next live-tail request. Page cursors are not affected.
    pub head_cursor: HeadCursor,
    /// The response metadata.
    pub meta: Meta,
}

impl QueryService {
    /// Live tail (contract 8.8): the traces whose head the response that issued a head cursor did not reflect,
    /// newest first. Heads of the issuing node's WAL are told by LSN: those above the cursor's are new, whenever
    /// they arrived, so a head that was durable but not yet applied when the cursor was issued is not missed;
    /// so are those that arrived after its `as_of`. Heads of any other WAL are told by arrival: those with
    /// `received_at >= as_of` are new. The store is read with exactly that predicate; the recent index through
    /// its (project, lsn) map from the cursor's LSN when it holds the issuing node's WAL, else by arrival.
    pub async fn trace_list_since(
        &self,
        ctx: &ReadContext,
        query: TraceTailQuery,
    ) -> Result<TraceTail, QueryServiceError> {
        // Before the clock, as in `trace_list`, for the head cursor of this response.
        let applied = self.index.applied();
        let mut read = self.read(ctx);
        let scope = ctx.scope;
        let limit = page_limit(query.limit)?;
        let (since, epoch, since_lsn) = query.since.check(scope, query.window)?;
        let base = self.consistency();
        let window = read.clamp.window(TableId::TraceHeads, query.window)?;
        let issued_here = epoch == self.wal_epoch;
        let after = if issued_here { since_lsn } else { Lsn::ZERO };
        let (entries, state) = self.index.since(scope, after);
        let mut recent = read.recent_heads(entries, state, &query.filter, |span| {
            window.is_some_and(|window| window.contains(span.summary.start_time))
                && (issued_here || span.received_at >= since)
        })?;

        // Enough heads to tell whether `new_count` saturates, in one read.
        let scan_limit = limit.max(NEW_COUNT_MAX);
        let unseen = unseen_heads(epoch, since_lsn, since);
        let request = TraceListRequest {
            filter: if query.filter == Pred::all() {
                unseen
            } else {
                Pred::And(vec![query.filter.clone(), unseen])
            },
            limit: scan_limit,
            cursor: None,
            as_of: read.now,
            since: None,
        };
        let mut chunk = read.store_heads(query.window, request).await?;
        let truncated = chunk.len() >= (scan_limit * TRACE_LIST_OVER_READ) as usize;
        chunk.extend(recent.heads.drain(..));
        let mut data = Vec::new();
        read.visible(newest_first(chunk), &mut data).await?;

        let new_count = (data.len() as u64).min(u64::from(NEW_COUNT_MAX)) as u32;
        let has_more = truncated || data.len() > limit as usize;
        data.truncate(limit as usize);
        let meta = read.finish(
            read.now,
            consistency(base, &recent),
            recent.state,
            &LIST_TABLES,
            recent.used,
        )?;
        Ok(TraceTail {
            data,
            new_count,
            has_more,
            head_cursor: HeadCursor::from_parts(
                scope.project_id(),
                query.window,
                read.now,
                self.wal_epoch,
                applied,
            ),
            meta,
        })
    }
}

/// The `trace_heads` rows that a head cursor of WAL `epoch` at `lsn` and `as_of` had not reflected. Of that WAL:
/// the rows above `lsn`, whenever they arrived, and the rows that arrived after `as_of`, which its response left
/// out (on one clock they are above `lsn` too; this keeps them when the wall clock stepped back). Of any other
/// WAL: the rows that arrived at or after `as_of`, since those can only be told apart by arrival.
fn unseen_heads(epoch: WalEpoch, lsn: Lsn, as_of: Timestamp) -> Pred {
    use TraceHeadsCol as C;
    Pred::Or(vec![
        Pred::And(vec![
            Pred::eq(C::WalEpoch, epoch.uuid()),
            Pred::Or(vec![
                Pred::cmp(C::Lsn, CmpOp::Gt, lsn.get()),
                Pred::cmp(C::ReceivedAt, CmpOp::Gt, as_of),
            ]),
        ]),
        Pred::And(vec![
            Pred::cmp(C::WalEpoch, CmpOp::Ne, epoch.uuid()),
            Pred::cmp(C::ReceivedAt, CmpOp::Ge, as_of),
        ]),
    ])
}
