//! The trace list and its live tail (contract 8.7 and 8.8).

use std::collections::VecDeque;
use std::sync::Arc;

use crucible_plan::{
    CmpOp, Pred, TRACE_LIST_MAX_LIMIT, TRACE_LIST_OVER_READ, TraceListRequest, trace_list,
};
use crucible_recent::{Consistency, ReadState, RecentSpan};
use crucible_records::{HeadKind, Lsn, TableId, TimeWindow, Timestamp, TraceId, WalEpoch};
use crucible_schema::TraceHeadsCol;

use crate::context::{Meta, QueryServiceError, ReadContext};
use crate::cursor::{HeadCursor, HeadKey, PageCursor};
use crate::decode::{self, TraceHeadView};
use crate::service::{QueryService, Read, Stats, TraceSummary};

/// The default page of the trace list (contract 12.4).
pub const TRACE_LIST_DEFAULT_LIMIT: u32 = 50;
/// `new_count` of a live-tail response saturates here.
pub const NEW_COUNT_MAX: u32 = 100;
/// A page reads the store at most this many times. Past it, a page of mostly hidden provisional heads comes
/// back short, with a cursor at the last head read.
const MAX_ROUNDS: usize = 4;

/// The tables the trace list depends on, for the quarantine rule.
const LIST_TABLES: [TableId; 2] = [TableId::TraceHeads, TableId::TraceStats];

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

/// One trace of the list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceListItem {
    /// The head that places the trace in the list.
    pub head: TraceHeadView,
    /// The trace's merged aggregates and resolved picks.
    pub summary: TraceSummary,
    /// Whether the head came from the recent index: the store does not serve it yet.
    pub recent: bool,
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

/// A candidate row of the list, from either source.
struct Candidate {
    head: TraceHeadView,
    recent: bool,
}

fn recent_head(span: &RecentSpan) -> Option<Candidate> {
    let s = &span.summary;
    let empty = || Arc::from("");
    Some(Candidate {
        head: TraceHeadView {
            trace_id: s.trace_id,
            span_id: s.span_id,
            head_kind: span.head?,
            start_time: s.start_time,
            end_time: s.end_time,
            name: Arc::clone(&s.name),
            level: s.level,
            status_code: s.status_code,
            user_id: Arc::clone(&s.user_id),
            session_id: Arc::clone(&s.session_id),
            tags: Vec::new(),
            environment: empty(),
            release: empty(),
            version: empty(),
            input_preview: empty(),
            output_preview: empty(),
            lsn: span.lsn,
            received_at: span.received_at,
        },
        recent: true,
    })
}

/// Orders candidates newest first and keeps one per sort tuple: the keyset cursor cannot tell two heads with
/// one tuple apart, so they are one row. A root wins over a provisional head (which the root hides anyway), and
/// then the store's row over the index's.
fn newest_first(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        let rank = |c: &Candidate| (c.head.head_kind != HeadKind::Root, c.recent);
        (b.head.key().cmp(&a.head.key())).then(rank(a).cmp(&rank(b)))
    });
    candidates.dedup_by(|later, first| later.head.key() == first.head.key());
    candidates
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

fn page_limit(limit: Option<u32>) -> Result<u32, QueryServiceError> {
    let limit = limit.unwrap_or(TRACE_LIST_DEFAULT_LIMIT);
    if limit == 0 || limit > TRACE_LIST_MAX_LIMIT {
        return Err(QueryServiceError::Validation("limit"));
    }
    Ok(limit)
}

/// What the recent index contributes to one list read.
struct RecentHeads {
    /// The heads to merge, newest first.
    heads: VecDeque<Candidate>,
    state: ReadState,
    /// Whether the index gave the read anything it could use.
    used: bool,
    /// Whether heads the store may not serve yet were left out, because the request has a filter that the
    /// index cannot evaluate (it keeps no tags, environment, release or version).
    left_out: bool,
}

impl Read<'_> {
    /// Turns the index's entries into the head candidates a list read merges.
    fn recent_heads(
        &self,
        entries: Vec<RecentSpan>,
        state: ReadState,
        filter: &Pred,
        keep: impl Fn(&RecentSpan) -> bool,
    ) -> Result<RecentHeads, QueryServiceError> {
        let entries: Vec<RecentSpan> = self
            .retained(entries)?
            .into_iter()
            .filter(|span| span.head.is_some() && keep(span))
            .collect();
        if *filter != Pred::all() {
            return Ok(RecentHeads {
                heads: VecDeque::new(),
                state,
                used: false,
                left_out: entries.iter().any(|span| span.lsn > state.watermark),
            });
        }
        let heads = newest_first(entries.iter().filter_map(recent_head).collect());
        Ok(RecentHeads {
            used: !heads.is_empty(),
            heads: heads.into(),
            state,
            left_out: false,
        })
    }

    /// The post-operators of contract 8.2 for one chunk of candidates: resolves each trace's merged
    /// aggregates, drops hidden provisional heads (contract 3.9: a provisional head is shown only while the
    /// trace has no root and the head is its earliest), and appends the rest.
    async fn visible(
        &mut self,
        chunk: Vec<Candidate>,
        out: &mut Vec<TraceListItem>,
    ) -> Result<(), QueryServiceError> {
        if chunk.is_empty() {
            return Ok(());
        }
        let mut ids: Vec<TraceId> = chunk.iter().map(|c| c.head.trace_id).collect();
        ids.sort_unstable();
        ids.dedup();
        let stats = self.stats(&ids).await?;
        for Candidate { head, recent } in chunk {
            // A head whose stats have not landed is shown: nothing says it has a root.
            let stats = stats
                .get(&head.trace_id)
                .cloned()
                .unwrap_or_else(|| Stats::of_head(head.start_time, head.end_time));
            let shown = match head.head_kind {
                HeadKind::Root => true,
                HeadKind::Provisional => !stats.has_root && head.start_time == stats.min_head_start,
            };
            if shown {
                out.push(TraceListItem {
                    head,
                    summary: stats.resolve(),
                    recent,
                });
            }
        }
        Ok(())
    }

    async fn store_heads(
        &mut self,
        window: TimeWindow,
        request: TraceListRequest,
    ) -> Result<Vec<Candidate>, QueryServiceError> {
        let plan = trace_list(
            self.ctx.scope,
            window,
            &self.clamp,
            self.ctx.caller,
            request,
        );
        let rows = self.rows(plan).await?;
        rows.iter()
            .map(|row| {
                Ok(Candidate {
                    head: decode::head(row)?,
                    recent: false,
                })
            })
            .collect()
    }
}

fn consistency(base: Consistency, recent: &RecentHeads) -> Consistency {
    if recent.left_out {
        Consistency::Lagging
    } else {
        base
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
