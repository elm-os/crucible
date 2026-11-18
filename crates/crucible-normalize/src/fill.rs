//! The request-local forward-fill of trace-level attributes (contract 3.3).
//!
//! Among the spans of one `trace_id` in one request, a span whose value is empty takes the value of the request's
//! root span for that trace, else of the earliest span in wire order that has one. With several roots, the first
//! root in wire order that has a value donates. Each attribute is filled on its own. Only spans that became drafts
//! take part, so a span rejected before the ack never donates. Nothing outside the request is read or written:
//! values are never backfilled into spans of earlier requests.

use bytes::Bytes;
use crucible_records::{SpanId, TraceId, TracePublic};
use smallvec::SmallVec;

use crate::mapper::SpanDraft;

/// A trace-level value that can be empty.
trait Blank: Clone {
    fn blank(&self) -> bool;
}

impl Blank for Box<str> {
    fn blank(&self) -> bool {
        self.is_empty()
    }
}

impl Blank for Vec<Box<str>> {
    fn blank(&self) -> bool {
        self.is_empty()
    }
}

impl Blank for Bytes {
    fn blank(&self) -> bool {
        self.is_empty()
    }
}

impl Blank for TracePublic {
    fn blank(&self) -> bool {
        *self == TracePublic::Unset
    }
}

/// Fills the trace-level attributes of `drafts` (one request, in wire order).
pub(crate) fn forward_fill(drafts: &mut [SpanDraft]) {
    if drafts.len() < 2 {
        return;
    }
    // Group by trace id, keeping wire order inside each group.
    let mut order: Vec<(TraceId, usize)> = drafts
        .iter()
        .enumerate()
        .map(|(i, d)| (d.record.core.trace_id, i))
        .collect();
    order.sort_unstable();
    for chunk in order.chunk_by(|a, b| a.0 == b.0) {
        if chunk.len() < 2 {
            continue;
        }
        let group: SmallVec<[usize; 16]> = chunk.iter().map(|&(_, i)| i).collect();
        fill(drafts, &group, |d| &mut d.record.core.trace_name);
        fill(drafts, &group, |d| &mut d.record.core.user_id);
        fill(drafts, &group, |d| &mut d.record.core.session_id);
        fill(drafts, &group, |d| &mut d.record.core.trace_tags);
        fill(drafts, &group, |d| &mut d.record.payload.trace_metadata);
        fill(drafts, &group, |d| &mut d.record.core.trace_public);
        fill(drafts, &group, |d| &mut d.record.core.environment);
        fill(drafts, &group, |d| &mut d.record.core.release);
        fill(drafts, &group, |d| &mut d.record.core.version);
    }
}

/// Fills one attribute in one trace group.
fn fill<T: Blank>(drafts: &mut [SpanDraft], group: &[usize], field: fn(&mut SpanDraft) -> &mut T) {
    let root = group
        .iter()
        .copied()
        .find(|&i| drafts[i].record.core.is_root && !field(&mut drafts[i]).blank());
    let donor = root.or_else(|| {
        group
            .iter()
            .copied()
            .find(|&i| !field(&mut drafts[i]).blank())
    });
    let Some(donor) = donor else {
        return;
    };
    let value = field(&mut drafts[donor]).clone();
    for &i in group {
        let slot = field(&mut drafts[i]);
        if slot.blank() {
            *slot = value.clone();
        }
    }
}

/// Flags the spans that repeat usage a descendant also carries (`usage_aggregate`, contract 3.1).
///
/// A span carries usage when it provides a count above zero. A span is an aggregate when it carries usage and a
/// descendant of it in the same request and trace carries usage too: the descendant (the leaf, which names the
/// model the usage is priced with) is what token and cost sums count. Only spans that became drafts take part.
/// The walk from each usage-carrying span to the top stops at the first span it has already climbed, so the work
/// is linear and a parent cycle ends.
pub(crate) fn usage_aggregates(drafts: &mut [SpanDraft]) {
    let carries = |d: &SpanDraft| d.record.payload.usage_provided.iter().any(|(_, n)| *n > 0);
    if drafts.iter().filter(|d| carries(d)).count() < 2 {
        return;
    }
    let mut by_id: Vec<(TraceId, SpanId, usize)> = drafts
        .iter()
        .enumerate()
        .map(|(i, d)| (d.record.core.trace_id, d.record.core.span_id, i))
        .collect();
    by_id.sort_unstable();
    // A span id that repeats within the request resolves to its first occurrence in wire order.
    let parent_of = |d: &SpanDraft| -> Option<usize> {
        let core = &d.record.core;
        if core.parent_span_id == SpanId::ZERO {
            return None;
        }
        let key = (core.trace_id, core.parent_span_id);
        let at = by_id.partition_point(|&(t, s, _)| (t, s) < key);
        by_id
            .get(at)
            .filter(|&&(t, s, _)| (t, s) == key)
            .map(|&(.., i)| i)
    };
    let mut climbed = vec![false; drafts.len()];
    let mut above_usage = vec![false; drafts.len()];
    for start in 0..drafts.len() {
        if !carries(&drafts[start]) {
            continue;
        }
        let mut at = start;
        while let Some(parent) = parent_of(&drafts[at]) {
            if parent == start {
                break;
            }
            let seen = climbed[parent];
            climbed[parent] = true;
            above_usage[parent] = true;
            if seen {
                break;
            }
            at = parent;
        }
    }
    for (draft, above) in drafts.iter_mut().zip(above_usage) {
        draft.record.core.usage_aggregate = above && carries(draft);
    }
}
