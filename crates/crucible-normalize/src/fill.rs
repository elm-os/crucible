//! The request-local forward-fill of trace-level attributes (contract 3.3).
//!
//! Among the spans of one `trace_id` in one request, a span whose value is empty takes the value of the request's
//! root span for that trace, else of the earliest span in wire order that has one. With several roots, the first
//! root in wire order that has a value donates. Each attribute is filled on its own. Only spans that became drafts
//! take part, so a span rejected before the ack never donates. Nothing outside the request is read or written:
//! values are never backfilled into spans of earlier requests.

use bytes::Bytes;
use crucible_records::{TraceId, TracePublic};
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
