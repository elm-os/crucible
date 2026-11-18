//! The trace heads that the trace list and live tail read (contract 8.7 and 8.8): the candidates from the store
//! and from the recent index, merged newest first, and the post-operators that resolve each trace's aggregates
//! and drop hidden provisional heads.

use std::collections::VecDeque;
use std::sync::Arc;

use crucible_plan::{Pred, TRACE_LIST_MAX_LIMIT, TraceListRequest, trace_list};
use crucible_recent::{Consistency, ReadState, RecentSpan};
use crucible_records::{HeadKind, TableId, TimeWindow, TraceId};

use crate::context::QueryServiceError;
use crate::decode::{self, TraceHeadView};
use crate::service::{Read, Stats, TraceSummary};

/// The default page of the trace list (contract 12.4).
pub const TRACE_LIST_DEFAULT_LIMIT: u32 = 50;

/// The tables the trace list depends on, for the quarantine rule.
pub(crate) const LIST_TABLES: [TableId; 2] = [TableId::TraceHeads, TableId::TraceStats];

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

/// A candidate row of the list, from either source.
pub(crate) struct Candidate {
    pub(crate) head: TraceHeadView,
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
pub(crate) fn newest_first(mut candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        let rank = |c: &Candidate| (c.head.head_kind != HeadKind::Root, c.recent);
        (b.head.key().cmp(&a.head.key())).then(rank(a).cmp(&rank(b)))
    });
    candidates.dedup_by(|later, first| later.head.key() == first.head.key());
    candidates
}

pub(crate) fn page_limit(limit: Option<u32>) -> Result<u32, QueryServiceError> {
    let limit = limit.unwrap_or(TRACE_LIST_DEFAULT_LIMIT);
    if limit == 0 || limit > TRACE_LIST_MAX_LIMIT {
        return Err(QueryServiceError::Validation("limit"));
    }
    Ok(limit)
}

/// What the recent index contributes to one list read.
pub(crate) struct RecentHeads {
    /// The heads to merge, newest first.
    pub(crate) heads: VecDeque<Candidate>,
    pub(crate) state: ReadState,
    /// Whether the index gave the read anything it could use.
    pub(crate) used: bool,
    /// Whether heads the store may not serve yet were left out, because the request has a filter that the
    /// index cannot evaluate (it keeps no tags, environment, release or version).
    left_out: bool,
}

impl Read<'_> {
    /// Turns the index's entries into the head candidates a list read merges.
    pub(crate) fn recent_heads(
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
    pub(crate) async fn visible(
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

    pub(crate) async fn store_heads(
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

pub(crate) fn consistency(base: Consistency, recent: &RecentHeads) -> Consistency {
    if recent.left_out {
        Consistency::Lagging
    } else {
        base
    }
}
