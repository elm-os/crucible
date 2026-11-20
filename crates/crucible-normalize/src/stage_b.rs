//! Stage B (contract 3.2 and 5.3): sequential, in LSN order. It flags usage aggregates through the usage
//! descendant set, drops exact repeats through the duplicate window, and marks provisional trace heads through
//! the first-seen set.
//!
//! **Usage aggregates.** A span's usage and cost count toward trace, session, user and metrics totals unless it
//! has `usage_aggregate`: at least one descendant that carries usage or cost, seen in the same frame or earlier
//! in the LSN stream. The usage descendant set holds the span ids that have such a descendant. Every span that
//! carries usage or cost, or is itself in the set, adds its parent id to the set. Within one frame this runs to a
//! fixed point over every span of the frame before any is flagged and before the duplicate drop, so wire order
//! inside a frame does not matter, and an exact repeat that the window then drops still passes a mark on to its
//! parent. Across frames it relies on children ending, and so being exported, no later than their parents. A
//! span is flagged when its id is in the set, whatever its type. Known limit: a descendant that arrives after its
//! ancestor, or whose frame the WAL has collected, or after the set evicted the ancestor's id, is counted
//! beside the ancestor.
//!
//! All three are defined over the LSN stream, never over wall time: the state after a sequence of frames is a function
//! of that sequence alone. Replaying the WAL's retained prefix in LSN order through a fresh [`StageB`] (and
//! discarding the output) therefore rebuilds it, which is recovery step 6 (contract 4.6).
//!
//! The retained prefix gets shorter as the WAL collects garbage, so the state must not remember more than that
//! prefix holds, or a restart would decide otherwise than the process it replaces, and the pairs of one batch
//! that land before and after the restart would disagree. Three rules keep the live state equal to the rebuilt
//! one. Every sighting of a key moves it to the newest position, so the window and the first-seen set hold the
//! keys sighted most recently. [`StageB::forget_below`] drops what was last sighted below the first retained
//! LSN. And [`Admission::relies_on`] names the oldest earlier frame a decision used, which the WAL keeps until
//! the batch of the deciding frame is checkpointed.
//!
//! The usage descendant set follows the same rules through the *support* of each id: the LSN of the frame
//! whose span carries the usage that the mark comes from. A mark that passes through spans of later frames
//! keeps the support of the frame it started in, and an id with several marks keeps the highest. The set
//! forgets an id whose support is below the first retained LSN, evicts the id with the lowest support first,
//! and a span flagged on a support below its own frame relies on that LSN.
//!
//! **MCP operations.** The operation and the tool inventory entries of a span follow their span: stage B drops
//! them with an exact repeat, so the window decides alike for every table (invariant I2). It then hashes the user
//! subject of each remaining operation with the salt of the frame's project and discards the subject (contract
//! 9.3). That needs no state: the hash is a function of the frame and the salt, which never changes.
//!
//! **Deletions.** [`StageB::admit_with`] also drops what the deletions of its context select (contract 13.3, step
//! 3; [`crate::Tombstones`]): the spans, with their operations and inventory entries, and the operations that a
//! deletion by user names by their own identity. A drop is not part of the state: every span is sighted by the
//! duplicate window and the first-seen set and passes its usage mark on whether it is dropped or not, so the state
//! stays a function of the LSN stream, and a rebuild from the retained prefix decides as the process before it did.
//! The provisional heads are those of the spans that remain.

use core::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, VecDeque};

use crucible_records::{DetMap, DetSet, Lsn, SpanCore, SpanId, det_map, det_set};
use xxhash_rust::xxh3::xxh3_64;

use crate::{IdentityCtx, NormCtx, NormalizedFrame, Tombstones};

/// Keys the duplicate window holds (contract 5.3).
pub const DUPLICATE_WINDOW_KEYS: usize = 4_000_000;

/// Traces the first-seen set holds. The contract bounds only the duplicate window; the first-seen set takes the
/// same bound so that its memory is bounded too.
pub const FIRST_SEEN_TRACES: usize = 4_000_000;

/// Span ids the usage descendant set holds (contract 5.3).
pub const USAGE_PARENT_SPANS: usize = 4_000_000;

/// The usage descendant key of a span's parent: as [`span_key`], with the parent's span id.
fn parent_key(core: &SpanCore) -> u64 {
    let mut bytes = [0u8; 40];
    bytes[..16].copy_from_slice(&core.project_id.to_bytes());
    bytes[16..32].copy_from_slice(core.trace_id.as_bytes());
    bytes[32..].copy_from_slice(core.parent_span_id.as_bytes());
    xxh3_64(&bytes)
}

/// The duplicate-window key: xxh3-64 of `project_id`, `trace_id` and `span_id` (16, 16 and 8 bytes).
fn span_key(core: &SpanCore) -> u64 {
    let mut bytes = [0u8; 40];
    bytes[..16].copy_from_slice(&core.project_id.to_bytes());
    bytes[16..32].copy_from_slice(core.trace_id.as_bytes());
    bytes[32..].copy_from_slice(core.span_id.as_bytes());
    xxh3_64(&bytes)
}

/// The first-seen key: xxh3-64 of `project_id` and `trace_id`.
fn trace_key(core: &SpanCore) -> u64 {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&core.project_id.to_bytes());
    bytes[16..].copy_from_slice(core.trace_id.as_bytes());
    xxh3_64(&bytes)
}

/// Keys with a value each and the LSN of the frame that sighted each last, for at most `capacity` keys: the
/// duplicate window (span key to `row_digest`) and the first-seen set (trace key).
///
/// Every sighting moves its key to the newest position, so the keys held are the `capacity` most recently
/// sighted ones, and [`Sightings::forget_below`] drops the keys last sighted below a floor. The state is
/// therefore a function of the frames from that floor on: replaying them through fresh state gives the same
/// keys, values and order.
#[derive(Debug)]
struct Sightings<V> {
    capacity: usize,
    /// Key to (value, LSN of its last sighting, sequence number of its position).
    entries: DetMap<u64, (V, Lsn, u64)>,
    /// (sequence number, key), oldest sighting first. A position whose key has since moved is stale and
    /// skipped.
    order: VecDeque<(u64, u64)>,
    next: u64,
}

impl<V: Copy + PartialEq> Sightings<V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: det_map(),
            order: VecDeque::new(),
            next: 0,
        }
    }

    /// Records a sighting of `key` with `value` in the frame at `lsn`, and returns what the key held before:
    /// its value and the LSN of its last sighting. When a new key takes the set above its capacity, the key
    /// sighted longest ago is evicted (rows of one frame in wire order).
    fn sight(&mut self, key: u64, value: V, lsn: Lsn) -> Option<(V, Lsn)> {
        let before = self.entries.get(&key).map(|&(v, at, _)| (v, at));
        if before == Some((value, lsn)) {
            // Sighted in this frame already: the key keeps its position.
            return before;
        }
        let seq = self.next;
        self.next += 1;
        self.entries.insert(key, (value, lsn, seq));
        self.order.push_back((seq, key));
        if before.is_some() {
            self.compact();
        } else {
            while self.entries.len() > self.capacity {
                self.evict_oldest();
            }
        }
        before
    }

    fn live(entries: &DetMap<u64, (V, Lsn, u64)>, seq: u64, key: u64) -> bool {
        entries.get(&key).is_some_and(|&(_, _, s)| s == seq)
    }

    fn evict_oldest(&mut self) {
        while let Some((seq, key)) = self.order.pop_front() {
            if Self::live(&self.entries, seq, key) {
                self.entries.remove(&key);
                return;
            }
        }
    }

    /// Drops every key whose last sighting is below `floor`.
    fn forget_below(&mut self, floor: Lsn) {
        while let Some(&(seq, key)) = self.order.front() {
            if Self::live(&self.entries, seq, key) {
                if self
                    .entries
                    .get(&key)
                    .is_some_and(|&(_, at, _)| at >= floor)
                {
                    return;
                }
                self.entries.remove(&key);
            }
            self.order.pop_front();
        }
    }

    /// Drops stale positions once they outnumber the live ones, so the order stays within twice the set.
    fn compact(&mut self) {
        if self.order.len() > 2 * self.entries.len() + 16 {
            let entries = &self.entries;
            self.order
                .retain(|&(seq, key)| Self::live(entries, seq, key));
        }
    }

    /// The (key, value, LSN of the last sighting) triples, oldest sighting first.
    #[cfg(test)]
    fn snapshot(&self) -> Vec<(u64, V, Lsn)> {
        self.order
            .iter()
            .filter(|&&(seq, key)| Self::live(&self.entries, seq, key))
            .map(|&(_, key)| (key, self.entries[&key].0, self.entries[&key].1))
            .collect()
    }
}

/// The usage descendant set: span keys that have a descendant carrying usage or cost, each with its support, the
/// LSN of the frame whose span carries that usage (the highest, when several descendants do), for at most
/// `capacity` keys.
///
/// A key is forgotten when its support is below a floor, and evicted lowest support first (then lowest key), so
/// the set is a function of the frames from that floor on: every mark with a support at or above the floor
/// comes from a span in the frame of that support and passes through later frames only.
#[derive(Debug)]
struct UsageParents {
    capacity: usize,
    support: DetMap<u64, Lsn>,
    /// (support, key), lowest support first.
    order: BTreeSet<(Lsn, u64)>,
}

impl UsageParents {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            support: det_map(),
            order: BTreeSet::new(),
        }
    }

    fn get(&self, key: u64) -> Option<Lsn> {
        self.support.get(&key).copied()
    }

    /// Gives `key` the support `at` unless it holds a higher one, then evicts down to the capacity.
    fn raise(&mut self, key: u64, at: Lsn) {
        match self.support.get(&key).copied() {
            Some(before) if before >= at => return,
            Some(before) => {
                self.order.remove(&(before, key));
            }
            None => {}
        }
        self.support.insert(key, at);
        self.order.insert((at, key));
        while self.order.len() > self.capacity {
            if let Some((_, evicted)) = self.order.pop_first() {
                self.support.remove(&evicted);
            }
        }
    }

    /// Drops every key whose support is below `floor`.
    fn forget_below(&mut self, floor: Lsn) {
        while let Some(&(at, key)) = self.order.first() {
            if at >= floor {
                return;
            }
            self.order.pop_first();
            self.support.remove(&key);
        }
    }

    /// The (support, key) pairs, lowest support first.
    #[cfg(test)]
    fn snapshot(&self) -> Vec<(Lsn, u64)> {
        self.order.iter().copied().collect()
    }
}

/// What stage B did with one frame.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Admission {
    /// Spans dropped as exact repeats.
    pub duplicates: u32,
    /// Spans dropped because a deletion selects them, with their MCP operations and inventory entries.
    pub deleted: u32,
    /// MCP operations of remaining spans dropped because a deletion by user names them by their own identity.
    pub deleted_ops: u32,
    /// For each remaining span, its position among the spans stage A made of the frame. A re-read of the frame
    /// through [`crate::normalize`] finds the span there, whatever stage B dropped before it, so this is the
    /// ordinal of the recent-span index's WAL locator (contract 8.6).
    pub stage_a_ordinals: Vec<u32>,
    /// Positions in the frame's remaining spans of the provisional heads: for each trace first seen in this frame
    /// with no root span in it, the trace's first span in wire order. Root heads need no marking: every span with
    /// `is_root` heads its trace (contract 3.9).
    pub provisional_heads: Vec<usize>,
    /// The lowest LSN of an earlier frame whose sighting a decision about this frame used: a span dropped as an
    /// exact repeat used the last sighting of its key, and a trace that is not new used the last sighting of
    /// the trace, and a span flagged as a usage aggregate on a descendant of an earlier frame used the frame that
    /// carries that descendant's usage. Stage B rebuilt from a prefix that starts above this LSN could decide otherwise, so the WAL
    /// must keep the prefix from here on until the frame's batch is checkpointed.
    pub relies_on: Option<Lsn>,
}

/// A frame offered to stage B whose LSN does not follow the previous frame's.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error(
    "stage B received LSN {frame} after LSN {last}; frames must arrive in increasing LSN order"
)]
pub struct OutOfOrder {
    /// The LSN of the previous frame.
    pub last: Lsn,
    /// The LSN of the refused frame.
    pub frame: Lsn,
}

/// Stage B state: the duplicate window, the first-seen set, the usage descendant set and the last LSN admitted.
#[derive(Debug)]
pub struct StageB {
    /// Span key to the `row_digest` of its last sighting. A span whose key holds an equal digest is an exact
    /// repeat; any other span is kept.
    window: Sightings<u64>,
    /// The trace keys sighted, by every span of a frame, dropped repeats included.
    first_seen: Sightings<()>,
    usage_parents: UsageParents,
    last: Option<Lsn>,
    /// The seeded defect that flags usage aggregates after the duplicate drop; never set outside the
    /// simulation suite.
    marks_after_drop: bool,
}

impl Default for StageB {
    /// The contract's bounds: [`DUPLICATE_WINDOW_KEYS`], [`FIRST_SEEN_TRACES`] and [`USAGE_PARENT_SPANS`].
    fn default() -> Self {
        Self::new(DUPLICATE_WINDOW_KEYS, FIRST_SEEN_TRACES)
    }
}

impl StageB {
    /// Empty state with the given bounds, and [`USAGE_PARENT_SPANS`] for the usage descendant set.
    pub fn new(window_keys: usize, first_seen_traces: usize) -> Self {
        Self {
            window: Sightings::new(window_keys),
            first_seen: Sightings::new(first_seen_traces),
            usage_parents: UsageParents::new(USAGE_PARENT_SPANS),
            last: None,
            marks_after_drop: false,
        }
    }

    /// The state with a seeded defect switched on.
    #[cfg(feature = "test-mutants")]
    #[must_use]
    pub fn with_mutant(mut self, mutant: Option<crate::Mutant>) -> Self {
        self.marks_after_drop = mutant == Some(crate::Mutant::MarkAfterDuplicateDrop);
        self
    }

    /// The state with another bound on the usage descendant set.
    #[must_use]
    pub fn with_usage_parents(mut self, spans: usize) -> Self {
        self.usage_parents = UsageParents::new(spans);
        self
    }

    /// Records the usage parents of a frame and flags its usage aggregates. It runs before the duplicate drop, over
    /// every span of the frame: an exact repeat that the window is about to drop still passes a mark on to its
    /// parent. `keys` holds the span key of each span. Returns, for each span, the support of its flag when
    /// that is an earlier frame; `None` when no span of the frame is flagged.
    fn mark_usage_aggregates(
        &mut self,
        frame: &mut NormalizedFrame,
        keys: &[u64],
    ) -> Option<Vec<Option<Lsn>>> {
        const NONE: u32 = u32::MAX;
        let lsn = frame.lsn;
        let spans = &mut frame.spans;
        // First the spans that carry usage or cost, in wire order: their marks have this frame's support, the
        // highest there is. Each span passes a mark on once, so the work is linear and a parent cycle ends.
        let mut done = vec![false; spans.len()];
        let mut work: Vec<usize> = Vec::new();
        // Then the spans that are in the set on an earlier frame's support, highest support first, so that
        // each passes on the highest support it has.
        let mut earlier: BinaryHeap<(Lsn, Reverse<usize>)> = BinaryHeap::new();
        for (i, span) in spans.iter().enumerate() {
            if span.core.carries_usage() {
                done[i] = true;
                work.push(i);
            } else if let Some(at) = self.usage_parents.get(keys[i]) {
                earlier.push((at, Reverse(i)));
            }
        }
        if work.is_empty() && earlier.is_empty() {
            // No span of the frame is in the set, and none joins it.
            for span in spans.iter_mut() {
                span.core.usage_aggregate = false;
            }
            return None;
        }
        // Every occurrence of each span key: the first by key, each naming the next.
        let mut first: DetMap<u64, u32> = det_map();
        let mut next = vec![NONE; spans.len()];
        for (i, &key) in keys.iter().enumerate().rev() {
            let at = u32::try_from(i).unwrap_or(NONE);
            next[i] = first.insert(key, at).unwrap_or(NONE);
        }
        // Passes the mark of span `i` on to its parent with `support`, and returns the first occurrence of the
        // parent in the frame, which passes it on in its turn with the occurrences chained from it. The first
        // mark to reach a key is its highest, so taking the key out walks its chain once.
        let set = &mut self.usage_parents;
        let mut pass = |i: usize, support: Lsn| -> u32 {
            let core = &spans[i].core;
            // A span that names itself as its parent is not its own descendant.
            if core.parent_span_id == SpanId::ZERO || core.parent_span_id == core.span_id {
                return NONE;
            }
            let parent = parent_key(core);
            set.raise(parent, support);
            first.remove(&parent).unwrap_or(NONE)
        };
        let mut at = 0;
        while let Some(&i) = work.get(at) {
            at += 1;
            let mut occurrence = pass(i, lsn);
            while let Some(&following) = next.get(occurrence as usize) {
                let j = occurrence as usize;
                if !done[j] {
                    done[j] = true;
                    work.push(j);
                }
                occurrence = following;
            }
        }
        while let Some((support, Reverse(i))) = earlier.pop() {
            if done[i] {
                continue;
            }
            done[i] = true;
            let mut occurrence = pass(i, support);
            while let Some(&following) = next.get(occurrence as usize) {
                let j = occurrence as usize;
                if !done[j] {
                    earlier.push((support, Reverse(j)));
                }
                occurrence = following;
            }
        }
        let mut supports = Vec::with_capacity(spans.len());
        for (span, &key) in spans.iter_mut().zip(keys) {
            let support = self.usage_parents.get(key);
            span.core.usage_aggregate = support.is_some();
            supports.push(support.filter(|&at| at < lsn));
        }
        Some(supports)
    }

    /// Admits the next frame of the LSN stream: records its usage parents and flags its usage aggregates, drops
    /// its exact repeats in place, with their MCP operations and inventory entries, marks its provisional heads,
    /// and hashes the user subjects of its MCP operations with what `identity` knows of the project's salt
    /// (contract 9.3).
    /// A frame whose LSN is not above the previous one is refused and leaves the state unchanged.
    pub fn admit(
        &mut self,
        frame: &mut NormalizedFrame,
        identity: &IdentityCtx<'_>,
    ) -> Result<Admission, OutOfOrder> {
        self.admit_dropping(frame, identity, None, false)
    }

    /// Admits the next frame as [`StageB::admit`] does with the identity of `ctx`, and drops what the deletions of
    /// `ctx` select (contract 13.3, step 3): the spans received before a deletion of their trace, user or session
    /// (or of their whole project), with their operations and inventory entries, and the operations that a
    /// deletion by user names by their `user_id` or subject hash. The deletions leave the state as it would be
    /// without them.
    pub fn admit_with(
        &mut self,
        frame: &mut NormalizedFrame,
        ctx: &NormCtx<'_>,
    ) -> Result<Admission, OutOfOrder> {
        #[cfg(feature = "test-mutants")]
        let every_issuer = ctx.mutant == Some(crate::Mutant::IgnoreTombstoneIssuer);
        #[cfg(not(feature = "test-mutants"))]
        let every_issuer = false;
        self.admit_dropping(frame, &ctx.identity, ctx.tombstones, every_issuer)
    }

    fn admit_dropping(
        &mut self,
        frame: &mut NormalizedFrame,
        identity: &IdentityCtx<'_>,
        tombstones: Option<&Tombstones>,
        every_issuer: bool,
    ) -> Result<Admission, OutOfOrder> {
        let tombstones = tombstones.filter(|t| !t.is_empty());
        if let Some(last) = self.last.filter(|&last| frame.lsn <= last) {
            return Err(OutOfOrder {
                last,
                frame: frame.lsn,
            });
        }
        self.last = Some(frame.lsn);
        let lsn = frame.lsn;
        let keys: Vec<u64> = frame
            .spans
            .iter()
            .map(|span| span_key(&span.core))
            .collect();
        let late = self.marks_after_drop;
        let supports = if late {
            None
        } else {
            self.mark_usage_aggregates(frame, &keys)
        };
        let mut relies_on: Option<Lsn> = None;
        let mut rely = |at: Lsn| {
            if at < lsn {
                relies_on = Some(relies_on.map_or(at, |lowest| lowest.min(at)));
            }
        };
        // The traces this frame sights first. A dropped repeat sights its trace too, so that the set is a
        // function of the frames alone, whatever the window dropped.
        let mut new: DetSet<u64> = det_set();
        for span in &frame.spans {
            let key = trace_key(&span.core);
            match self.first_seen.sight(key, (), lsn) {
                Some(((), at)) => rely(at),
                None => {
                    new.insert(key);
                }
            }
        }
        let mut duplicates = 0u32;
        let mut deleted = 0u32;
        let window = &mut self.window;
        let mut stage_a_ordinals = Vec::with_capacity(frame.spans.len());
        let mut ordinal = 0u32;
        frame.spans.retain(|span| {
            let digest = span.core.row_digest;
            let position = ordinal as usize;
            let key = keys
                .get(position)
                .copied()
                .unwrap_or_else(|| span_key(&span.core));
            // Every span is sighted, so that the window does not depend on the deletions.
            let repeat = match window.sight(key, digest, lsn) {
                Some((remembered, at)) if remembered == digest => {
                    rely(at);
                    true
                }
                _ => false,
            };
            let dropped = !repeat && tombstones.is_some_and(|t| t.selects_span(&span.core));
            duplicates += u32::from(repeat);
            deleted += u32::from(dropped);
            let kept = !repeat && !dropped;
            if kept {
                stage_a_ordinals.push(ordinal);
                // A kept span flagged on a descendant of an earlier frame relies on that frame.
                if let Some(support) = supports
                    .as_ref()
                    .and_then(|s| s.get(position).copied().flatten())
                {
                    rely(support);
                }
            }
            ordinal = ordinal.saturating_add(1);
            kept
        });
        if late {
            let kept: Vec<u64> = frame
                .spans
                .iter()
                .map(|span| span_key(&span.core))
                .collect();
            drop(self.mark_usage_aggregates(frame, &kept));
        }
        let mut deleted_ops = 0u32;
        if !frame.mcp_ops.is_empty() || !frame.inventory.is_empty() {
            // Where each span of stage A stands among the spans that remain.
            const DROPPED: u32 = u32::MAX;
            let mut position = vec![DROPPED; ordinal as usize];
            for (at, &before) in stage_a_ordinals.iter().enumerate() {
                if let Some(slot) = position.get_mut(before as usize) {
                    *slot = u32::try_from(at).unwrap_or(DROPPED);
                }
            }
            let now = |span: u32| {
                position
                    .get(span as usize)
                    .copied()
                    .filter(|&at| at != DROPPED)
            };
            let salt = match identity {
                IdentityCtx::Salted(Some(salt)) => Some(*salt),
                _ => None,
            };
            frame.mcp_ops.retain_mut(|op| {
                let Some(at) = now(op.span) else {
                    return false;
                };
                op.span = at;
                op.seal_identity(identity);
                // A deletion by user also names an operation by its own identity, whatever its span carries.
                let named =
                    tombstones.is_some_and(|t| t.op_selected(&op.record, salt, every_issuer));
                deleted_ops += u32::from(named);
                !named
            });
            frame.inventory.retain_mut(|entry| {
                let Some(at) = now(entry.span) else {
                    return false;
                };
                entry.span = at;
                true
            });
        }
        let mut first: Vec<(u64, usize)> = Vec::new();
        let mut noted: DetSet<u64> = det_set();
        let mut rooted: DetSet<u64> = det_set();
        for (i, span) in frame.spans.iter().enumerate() {
            let key = trace_key(&span.core);
            if span.core.is_root {
                rooted.insert(key);
            }
            if new.contains(&key) && noted.insert(key) {
                first.push((key, i));
            }
        }
        let provisional_heads = first
            .into_iter()
            .filter(|(key, _)| !rooted.contains(key))
            .map(|(_, i)| i)
            .collect();
        Ok(Admission {
            duplicates,
            deleted,
            deleted_ops,
            stage_a_ordinals,
            provisional_heads,
            relies_on,
        })
    }

    /// Forgets every sighting below `floor`, the first LSN the WAL retains. The state is then what replaying
    /// the retained prefix through fresh state gives (recovery step 6), so a restart decides as this state does.
    pub fn forget_below(&mut self, floor: Lsn) {
        self.window.forget_below(floor);
        self.first_seen.forget_below(floor);
        self.usage_parents.forget_below(floor);
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::{ProjectId, SpanId, SpanRecord, TraceId};
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;
    use crate::test_support::{op_record, record};
    use crate::{FrameSummary, McpOp, PayloadCaps, PriceTable, Tombstone, TombstoneSelector};

    /// A span as the tests describe it: (project, trace, span, digest, is_root), from small domains so that
    /// keys repeat.
    type Desc = (u8, u8, u8, u8, bool);

    fn span(&(p, t, s, d, root): &Desc) -> SpanRecord {
        let mut r = record();
        r.core.project_id = ProjectId::from_bytes([p + 1; 16]);
        r.core.trace_id = TraceId::from_bytes([t + 1; 16]);
        r.core.span_id = SpanId::from_bytes([s + 1; 8]);
        r.core.row_digest = u64::from(d);
        r.core.is_root = root;
        r
    }

    fn frame(lsn: u64, spans: &[Desc]) -> NormalizedFrame {
        NormalizedFrame {
            lsn: Lsn::new(lsn),
            spans: spans.iter().map(span).collect(),
            mcp_ops: Vec::new(),
            inventory: Vec::new(),
            summary: FrameSummary::default(),
        }
    }

    /// A frame whose spans arrived at the instant `lsn`, so that a deletion requested at `r` selects the frames
    /// below `r`.
    fn timed_frame(lsn: u64, spans: &[Desc]) -> NormalizedFrame {
        let mut f = frame(lsn, spans);
        for span in &mut f.spans {
            span.core.ingestion.received_at = crucible_records::Timestamp::from_micros(lsn as i64);
        }
        f
    }

    /// A deletion of the traces `traces` of project `p`, requested at the instant `at`.
    fn trace_deletion(p: u8, traces: &[u8], at: u64) -> Tombstone {
        Tombstone {
            request_id: crucible_records::DeletionRequestId::from_bytes([at as u8 + 1; 16]),
            project_id: ProjectId::from_bytes([p + 1; 16]),
            requested_at: crucible_records::Timestamp::from_micros(at as i64),
            deletion_seq: at,
            selector: TombstoneSelector::Traces(
                traces
                    .iter()
                    .map(|&t| TraceId::from_bytes([t + 1; 16]))
                    .collect(),
            ),
        }
    }

    /// A linear model of stage B over the descriptions themselves: no hashing, no maps, no stale positions.
    struct Oracle {
        window_cap: usize,
        seen_cap: usize,
        /// ((project, trace, span), digest, LSN of the last sighting), oldest sighting first.
        window: Vec<((u8, u8, u8), u8, u64)>,
        /// ((project, trace), LSN of the last sighting), oldest sighting first.
        seen: Vec<((u8, u8), u64)>,
    }

    impl Oracle {
        /// The kept spans (as indexes into `spans`), the provisional heads (as positions among the kept) and
        /// the lowest earlier LSN a decision used.
        fn admit(&mut self, lsn: u64, spans: &[Desc]) -> (Vec<usize>, Vec<usize>, Option<u64>) {
            self.admit_dropping(lsn, spans, |_| false)
        }

        /// As [`Oracle::admit`], with the spans at the indexes `deleted` selects dropped after the window saw
        /// them.
        fn admit_dropping(
            &mut self,
            lsn: u64,
            spans: &[Desc],
            deleted: impl Fn(usize) -> bool,
        ) -> (Vec<usize>, Vec<usize>, Option<u64>) {
            let mut relied: Vec<u64> = Vec::new();
            let mut new: Vec<(u8, u8)> = Vec::new();
            for &(p, t, ..) in spans {
                match self.seen.iter().position(|(k, _)| *k == (p, t)) {
                    Some(pos) if self.seen[pos].1 == lsn => {}
                    Some(pos) => {
                        relied.push(self.seen.remove(pos).1);
                        self.seen.push(((p, t), lsn));
                    }
                    None => {
                        if !new.contains(&(p, t)) {
                            new.push((p, t));
                        }
                        self.seen.push(((p, t), lsn));
                        if self.seen.len() > self.seen_cap {
                            self.seen.remove(0);
                        }
                    }
                }
            }
            let mut kept = Vec::new();
            for (i, &(p, t, s, d, _)) in spans.iter().enumerate() {
                let key = (p, t, s);
                match self.window.iter().position(|(k, ..)| *k == key) {
                    Some(pos) => {
                        let (_, digest, at) = self.window[pos];
                        if (digest, at) != (d, lsn) {
                            self.window.remove(pos);
                            self.window.push((key, d, lsn));
                        }
                        if digest == d {
                            relied.push(at);
                            continue;
                        }
                    }
                    None => {
                        self.window.push((key, d, lsn));
                        if self.window.len() > self.window_cap {
                            self.window.remove(0);
                        }
                    }
                }
                if deleted(i) {
                    continue;
                }
                kept.push(i);
            }
            let mut heads = Vec::new();
            for trace in new {
                let of_trace = |i: &usize| (spans[*i].0, spans[*i].1) == trace;
                if kept.iter().any(|i| of_trace(i) && spans[*i].4) {
                    continue;
                }
                if let Some(pos) = kept.iter().position(of_trace) {
                    heads.push(pos);
                }
            }
            heads.sort_unstable();
            let relies_on = relied.into_iter().filter(|&at| at < lsn).min();
            (kept, heads, relies_on)
        }

        fn forget_below(&mut self, floor: u64) {
            self.window.retain(|&(.., at)| at >= floor);
            self.seen.retain(|&(_, at)| at >= floor);
        }
    }

    fn desc() -> impl Strategy<Value = Desc> {
        (0u8..2, 0u8..4, 0u8..4, 0u8..3, any::<bool>())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// Deletions drop what they select, after the window has seen it: the state is the one stage B holds
        /// without them, the remaining spans are the others in wire order, and the provisional heads are those of
        /// the remaining spans, as the linear model says.
        #[test]
        fn deletions_drop_what_they_select_and_leave_the_state_alone(
            frames in vec(vec(desc(), 0..10), 1..12),
            deletions in vec((0u8..2, vec(0u8..4, 1..3), 1u64..14), 0..4),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
        ) {
            let mut tombstones = Tombstones::new();
            for (p, traces, at) in &deletions {
                tombstones.insert(&trace_deletion(*p, traces, *at));
            }
            let prices = PriceTable::bundled().unwrap();
            let ctx = NormCtx::new(prices, PayloadCaps::DEFAULT).with_tombstones(&tombstones);
            let selected = |lsn: u64, &(p, t, ..): &Desc| {
                deletions.iter().any(|(dp, traces, at)| *dp == p && traces.contains(&t) && lsn < *at)
            };
            let mut plain = StageB::new(window_cap, seen_cap);
            let mut dropping = StageB::new(window_cap, seen_cap);
            let mut oracle = Oracle { window_cap, seen_cap, window: Vec::new(), seen: Vec::new() };
            for (n, spans) in frames.iter().enumerate() {
                let lsn = n as u64 + 1;
                let mut whole = timed_frame(lsn, spans);
                let mut left = timed_frame(lsn, spans);
                let all = plain.admit(&mut whole, &IdentityCtx::Suppressed).unwrap();
                let admission = dropping.admit_with(&mut left, &ctx).unwrap();
                let (kept, heads, _) =
                    oracle.admit_dropping(lsn, spans, |i| selected(lsn, &spans[i]));
                let expected: Vec<SpanRecord> = kept.iter().map(|&i| timed_frame(lsn, spans).spans[i].clone()).collect();
                prop_assert_eq!(&left.spans, &expected);
                prop_assert_eq!(admission.duplicates, all.duplicates);
                prop_assert_eq!(admission.deleted as usize, whole.spans.len() - left.spans.len());
                prop_assert_eq!(&admission.provisional_heads, &heads);
                let ordinals: Vec<u32> = kept.iter().map(|&i| i as u32).collect();
                prop_assert_eq!(&admission.stage_a_ordinals, &ordinals);
                prop_assert_eq!(dropping.window.snapshot(), plain.window.snapshot());
                prop_assert_eq!(dropping.first_seen.snapshot(), plain.first_seen.snapshot());
                prop_assert_eq!(dropping.usage_parents.snapshot(), plain.usage_parents.snapshot());
            }
        }
    }

    /// A deletion by user names an operation by its own `user_id` although its span carries none: the operation
    /// goes and its span and inventory entry stay. The operation of a span that a deletion drops goes with it.
    #[test]
    fn a_deletion_by_user_drops_the_operations_it_names_and_their_spans_stay() {
        let mut f = timed_frame(
            3,
            &[(0, 0, 0, 0, true), (0, 0, 1, 1, false), (0, 1, 2, 2, true)],
        );
        // The third span is of a deleted trace.
        let mut deletions = Tombstones::new();
        deletions.insert(&trace_deletion(0, &[1], 9));
        deletions.insert(&Tombstone {
            request_id: crucible_records::DeletionRequestId::from_bytes([7; 16]),
            project_id: ProjectId::from_bytes([1; 16]),
            requested_at: crucible_records::Timestamp::from_micros(i64::MAX),
            deletion_seq: 2,
            selector: TombstoneSelector::User {
                user_id: "ada".into(),
                issuer: None,
            },
        });
        for (at, span) in f.spans.clone().iter().enumerate() {
            let mut record = op_record(&span.core);
            if at == 1 {
                record.user_id = "ada".into();
            }
            f.mcp_ops.push(McpOp {
                span: at as u32,
                record,
                subject: "".into(),
            });
            f.inventory.push(crate::McpInventory {
                span: at as u32,
                record: crucible_records::McpToolInventoryRecord {
                    project_id: span.core.project_id,
                    server_name: "server".into(),
                    deploy_id: "".into(),
                    tool_name: format!("tool-{at}").into(),
                    schema_hash: 0,
                    capture_source: crucible_records::CaptureSource::Sdk,
                    first_seen: span.core.start_time,
                    last_seen: span.core.start_time,
                },
            });
        }
        let prices = PriceTable::bundled().unwrap();
        let ctx = NormCtx::new(prices, PayloadCaps::DEFAULT)
            .with_identity(IdentityCtx::Salted(None))
            .with_tombstones(&deletions);
        let admission = StageB::default().admit_with(&mut f, &ctx).unwrap();
        assert_eq!((admission.deleted, admission.deleted_ops), (1, 1));
        let spans: Vec<SpanId> = f.spans.iter().map(|s| s.core.span_id).collect();
        assert_eq!(
            spans,
            [SpanId::from_bytes([1; 8]), SpanId::from_bytes([2; 8])]
        );
        let ops: Vec<(u32, SpanId)> = f
            .mcp_ops
            .iter()
            .map(|op| (op.span, op.record.span_id))
            .collect();
        assert_eq!(ops, [(0, SpanId::from_bytes([1; 8]))]);
        let tools: Vec<(u32, &str)> = f
            .inventory
            .iter()
            .map(|entry| (entry.span, &*entry.record.tool_name))
            .collect();
        assert_eq!(tools, [(0, "tool-0"), (1, "tool-1")]);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// Stage B agrees with the linear model on every frame: which spans it keeps, the duplicate count, the
        /// provisional heads, the LSN it relies on, and both sets in eviction order, also across forgetting.
        #[test]
        fn stage_b_matches_the_oracle(
            frames in vec((vec(desc(), 0..10), prop::option::of(0u64..14)), 1..12),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
        ) {
            let mut stage = StageB::new(window_cap, seen_cap);
            let mut oracle = Oracle { window_cap, seen_cap, window: Vec::new(), seen: Vec::new() };
            for (n, (spans, floor)) in frames.iter().enumerate() {
                let lsn = n as u64 + 1;
                let mut f = frame(lsn, spans);
                let admission = stage.admit(&mut f, &IdentityCtx::Suppressed).unwrap();
                let (kept, heads, relies_on) = oracle.admit(lsn, spans);
                let expected: Vec<SpanRecord> = kept.iter().map(|&i| span(&spans[i])).collect();
                prop_assert_eq!(&f.spans, &expected);
                prop_assert_eq!(admission.duplicates as usize, spans.len() - kept.len());
                // Each kept span names its position in the frame as stage A made it.
                let ordinals: Vec<u32> = kept.iter().map(|&i| i as u32).collect();
                prop_assert_eq!(&admission.stage_a_ordinals, &ordinals);
                prop_assert_eq!(&admission.provisional_heads, &heads);
                prop_assert_eq!(admission.relies_on, relies_on.map(Lsn::new));
                if let Some(floor) = *floor {
                    stage.forget_below(Lsn::new(floor));
                    oracle.forget_below(floor);
                }
                let window: Vec<(u64, u64, Lsn)> = oracle
                    .window
                    .iter()
                    .map(|&((p, t, s), d, at)| {
                        (span_key(&span(&(p, t, s, d, false)).core), u64::from(d), Lsn::new(at))
                    })
                    .collect();
                prop_assert_eq!(stage.window.snapshot(), window);
                let seen: Vec<(u64, (), Lsn)> = oracle
                    .seen
                    .iter()
                    .map(|&((p, t), at)| (trace_key(&span(&(p, t, 0, 0, false)).core), (), Lsn::new(at)))
                    .collect();
                prop_assert_eq!(stage.first_seen.snapshot(), seen);
                prop_assert!(stage.window.order.len() <= 2 * stage.window.entries.len() + 17);
                prop_assert!(stage.first_seen.order.len() <= 2 * stage.first_seen.entries.len() + 17);
            }
        }

        /// Replaying a prefix of the stream through fresh state rebuilds it: the rest of the stream is admitted
        /// exactly as by the state that saw the whole stream.
        #[test]
        fn replaying_a_prefix_rebuilds_the_state(
            frames in vec(vec(desc(), 0..10), 1..12),
            cut in any::<prop::sample::Index>(),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
        ) {
            let cut = cut.index(frames.len() + 1);
            let run = |stage: &mut StageB, range: core::ops::Range<usize>| -> Vec<(Vec<SpanRecord>, Admission)> {
                frames[range.clone()]
                    .iter()
                    .zip(range)
                    .map(|(spans, n)| {
                        let mut f = frame(n as u64 + 1, spans);
                        let a = stage.admit(&mut f, &IdentityCtx::Suppressed).unwrap();
                        (f.spans, a)
                    })
                    .collect()
            };
            let mut whole = StageB::new(window_cap, seen_cap);
            let all = run(&mut whole, 0..frames.len());
            let mut rebuilt = StageB::new(window_cap, seen_cap);
            run(&mut rebuilt, 0..cut);
            let rest = run(&mut rebuilt, cut..frames.len());
            prop_assert_eq!(&rest[..], &all[cut..]);
        }

        /// The live state never remembers more than the retained prefix holds: after any forgetting, with the
        /// last floor at `floor`, both sets equal what a replay of the frames from `floor` on gives fresh
        /// state, in eviction order, so both admit the rest of the stream alike.
        #[test]
        fn the_live_state_equals_a_rebuild_from_the_retained_prefix(
            frames in vec(vec(desc(), 0..10), 1..14),
            floors in vec(any::<prop::sample::Index>(), 0..4),
            rest in vec(vec(desc(), 0..10), 0..4),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
        ) {
            // Each floor is forgotten after the frame below it at the earliest, as garbage collection only
            // removes frames that were admitted; floors never go back.
            let mut floors: Vec<usize> = floors.iter().map(|f| f.index(frames.len()) + 1).collect();
            floors.sort_unstable();
            let floor = floors.last().copied().unwrap_or(1);
            let mut live = StageB::new(window_cap, seen_cap);
            for (n, spans) in frames.iter().enumerate() {
                live.admit(&mut frame(n as u64 + 1, spans), &IdentityCtx::Suppressed).unwrap();
                for at in floors.iter().filter(|&&at| at == n + 1) {
                    live.forget_below(Lsn::new(*at as u64));
                }
            }
            live.forget_below(Lsn::new(floor as u64));
            let mut rebuilt = StageB::new(window_cap, seen_cap);
            for (n, spans) in frames.iter().enumerate().skip(floor - 1) {
                rebuilt.admit(&mut frame(n as u64 + 1, spans), &IdentityCtx::Suppressed).unwrap();
            }
            prop_assert_eq!(live.window.snapshot(), rebuilt.window.snapshot());
            prop_assert_eq!(live.first_seen.snapshot(), rebuilt.first_seen.snapshot());
            for (n, spans) in rest.iter().enumerate() {
                let lsn = (frames.len() + n) as u64 + 1;
                let (mut a, mut b) = (frame(lsn, spans), frame(lsn, spans));
                prop_assert_eq!(live.admit(&mut a, &IdentityCtx::Suppressed).unwrap(), rebuilt.admit(&mut b, &IdentityCtx::Suppressed).unwrap());
                prop_assert_eq!(a.spans, b.spans);
            }
        }

        /// A restart decides as the process it replaces did: the frames from `cut` on are admitted alike by
        /// the state that saw the whole stream and by fresh state that replayed a prefix starting at any LSN
        /// up to the lowest one those frames rely on.
        #[test]
        fn a_rebuild_from_what_a_frame_relies_on_decides_alike(
            frames in vec(vec(desc(), 0..10), 1..14),
            cut in any::<prop::sample::Index>(),
            start in any::<prop::sample::Index>(),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
        ) {
            let cut = cut.index(frames.len());
            let run = |stage: &mut StageB, from: usize| -> Vec<(Vec<SpanRecord>, Admission)> {
                frames
                    .iter()
                    .enumerate()
                    .skip(from)
                    .map(|(n, spans)| {
                        let mut f = frame(n as u64 + 1, spans);
                        let a = stage.admit(&mut f, &IdentityCtx::Suppressed).unwrap();
                        (f.spans, a)
                    })
                    .collect()
            };
            let all = run(&mut StageB::new(window_cap, seen_cap), 0);
            // As 0-based frame indexes: the lowest frame that the frames from `cut` on rely on, or `cut`.
            let lowest = all[cut..]
                .iter()
                .filter_map(|(_, a)| a.relies_on)
                .map(|at| at.get() as usize - 1)
                .min()
                .map_or(cut, |at| at.min(cut));
            let start = start.index(lowest + 1);
            let rebuilt = run(&mut StageB::new(window_cap, seen_cap), start);
            prop_assert_eq!(&rebuilt[cut - start..], &all[cut..]);
        }
    }

    /// A span with what the usage descendant set reads: its parent (a span id of the same small domain, or none)
    /// and whether it carries usage.
    type UsageDesc = (Desc, Option<u8>, bool);

    fn usage_frame(lsn: u64, spans: &[UsageDesc]) -> NormalizedFrame {
        let mut f = frame(
            lsn,
            &spans.iter().map(|(desc, ..)| *desc).collect::<Vec<_>>(),
        );
        for (record, &(_, parent, carries)) in f.spans.iter_mut().zip(spans) {
            record.core.parent_span_id =
                parent.map_or(SpanId::ZERO, |s| SpanId::from_bytes([s + 1; 8]));
            record.core.total_cost_nusd = crucible_records::NanoUsd::from_nanos(i64::from(carries));
        }
        f
    }

    fn usage_desc() -> impl Strategy<Value = UsageDesc> {
        (desc(), prop::option::of(0u8..4), any::<bool>())
    }

    /// The usage descendant set as a loop to a fixed point, with no bound, no hashing and no worklist: the
    /// support of each (project, trace, span), the highest when several marks reach it.
    fn usage_model(supports: &mut Vec<((u8, u8, u8), u64)>, lsn: u64, spans: &[UsageDesc]) {
        loop {
            let mut changed = false;
            for &((p, t, s, ..), parent, carries) in spans {
                let own = supports
                    .iter()
                    .find(|(k, _)| *k == (p, t, s))
                    .map(|&(_, at)| at);
                let Some(passed) = carries.then_some(lsn).or(own) else {
                    continue;
                };
                let Some(parent) = parent.filter(|&parent| parent != s) else {
                    continue;
                };
                match supports.iter_mut().find(|(k, _)| *k == (p, t, parent)) {
                    Some((_, at)) if *at >= passed => {}
                    Some((_, at)) => {
                        *at = passed;
                        changed = true;
                    }
                    None => {
                        supports.push(((p, t, parent), passed));
                        changed = true;
                    }
                }
            }
            if !changed {
                return;
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// The usage descendant set agrees with the fixed-point model on every frame, also across forgetting:
        /// the ids it holds, their supports, and the flags of the kept spans.
        #[test]
        fn the_usage_set_matches_the_fixed_point_model(
            frames in vec((vec(usage_desc(), 0..10), prop::option::of(0u64..14)), 1..12),
        ) {
            let mut stage = StageB::default();
            let mut supports: Vec<((u8, u8, u8), u64)> = Vec::new();
            for (n, (spans, floor)) in frames.iter().enumerate() {
                let lsn = n as u64 + 1;
                let mut f = usage_frame(lsn, spans);
                let ordinals = stage.admit(&mut f, &IdentityCtx::Suppressed).unwrap().stage_a_ordinals;
                usage_model(&mut supports, lsn, spans);
                for (record, &at) in f.spans.iter().zip(&ordinals) {
                    let ((p, t, s, ..), ..) = spans[at as usize];
                    let flagged = supports.iter().any(|(k, _)| *k == (p, t, s));
                    prop_assert_eq!(record.core.usage_aggregate, flagged);
                }
                if let Some(floor) = *floor {
                    stage.forget_below(Lsn::new(floor));
                    supports.retain(|&(_, at)| at >= floor);
                }
                let mut model: Vec<(Lsn, u64)> = supports
                    .iter()
                    .map(|&((p, t, s), at)| (Lsn::new(at), span_key(&span(&(p, t, s, 0, false)).core)))
                    .collect();
                model.sort_unstable();
                prop_assert_eq!(stage.usage_parents.snapshot(), model);
            }
        }

        /// As `the_live_state_equals_a_rebuild_from_the_retained_prefix`, with spans that name parents and
        /// carry usage and with a small bound on the usage descendant set: after any forgetting the set equals
        /// what a replay of the retained frames gives fresh state, so both flag the rest of the stream alike.
        #[test]
        fn the_usage_set_equals_a_rebuild_from_the_retained_prefix(
            frames in vec(vec(usage_desc(), 0..10), 1..14),
            floors in vec(any::<prop::sample::Index>(), 0..4),
            rest in vec(vec(usage_desc(), 0..10), 0..4),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
            usage_cap in 0usize..8,
        ) {
            let mut floors: Vec<usize> = floors.iter().map(|f| f.index(frames.len()) + 1).collect();
            floors.sort_unstable();
            let floor = floors.last().copied().unwrap_or(1);
            let fresh = || StageB::new(window_cap, seen_cap).with_usage_parents(usage_cap);
            let mut live = fresh();
            for (n, spans) in frames.iter().enumerate() {
                live.admit(&mut usage_frame(n as u64 + 1, spans), &IdentityCtx::Suppressed).unwrap();
                for at in floors.iter().filter(|&&at| at == n + 1) {
                    live.forget_below(Lsn::new(*at as u64));
                }
            }
            live.forget_below(Lsn::new(floor as u64));
            let mut rebuilt = fresh();
            for (n, spans) in frames.iter().enumerate().skip(floor - 1) {
                rebuilt.admit(&mut usage_frame(n as u64 + 1, spans), &IdentityCtx::Suppressed).unwrap();
            }
            prop_assert_eq!(live.usage_parents.snapshot(), rebuilt.usage_parents.snapshot());
            prop_assert_eq!(live.window.snapshot(), rebuilt.window.snapshot());
            prop_assert_eq!(live.first_seen.snapshot(), rebuilt.first_seen.snapshot());
            for (n, spans) in rest.iter().enumerate() {
                let lsn = (frames.len() + n) as u64 + 1;
                let (mut a, mut b) = (usage_frame(lsn, spans), usage_frame(lsn, spans));
                prop_assert_eq!(live.admit(&mut a, &IdentityCtx::Suppressed).unwrap(), rebuilt.admit(&mut b, &IdentityCtx::Suppressed).unwrap());
                prop_assert_eq!(a.spans, b.spans);
            }
        }

        /// As `a_rebuild_from_what_a_frame_relies_on_decides_alike`, with usage: fresh state that replayed a
        /// prefix starting at any LSN up to the lowest one the frames from `cut` on rely on keeps, drops and
        /// flags their spans as the state that saw the whole stream did.
        #[test]
        fn a_rebuild_from_what_a_frame_relies_on_flags_alike(
            frames in vec(vec(usage_desc(), 0..10), 1..14),
            cut in any::<prop::sample::Index>(),
            start in any::<prop::sample::Index>(),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
            usage_cap in 0usize..8,
        ) {
            let cut = cut.index(frames.len());
            let run = |from: usize| -> Vec<(Vec<SpanRecord>, Admission)> {
                let mut stage = StageB::new(window_cap, seen_cap).with_usage_parents(usage_cap);
                frames
                    .iter()
                    .enumerate()
                    .skip(from)
                    .map(|(n, spans)| {
                        let mut f = usage_frame(n as u64 + 1, spans);
                        let a = stage.admit(&mut f, &IdentityCtx::Suppressed).unwrap();
                        (f.spans, a)
                    })
                    .collect()
            };
            let all = run(0);
            let lowest = all[cut..]
                .iter()
                .filter_map(|(_, a)| a.relies_on)
                .map(|at| at.get() as usize - 1)
                .min()
                .map_or(cut, |at| at.min(cut));
            let start = start.index(lowest + 1);
            let rebuilt = run(start);
            prop_assert_eq!(&rebuilt[cut - start..], &all[cut..]);
        }
    }

    /// A mark keeps the support of the frame whose span carries the usage, through every later frame it passes,
    /// so the flagged span relies on that frame, and the set forgets the mark with it.
    #[test]
    fn a_mark_relies_on_the_frame_that_carries_the_usage() {
        // C (span 3, usage) in frame 1, P (span 2) in frame 2, G (span 1) in frame 3; one trace.
        let c: UsageDesc = ((0, 0, 3, 0, false), Some(2), true);
        let p: UsageDesc = ((0, 0, 2, 0, false), Some(1), false);
        let g: UsageDesc = ((0, 0, 1, 0, false), None, false);
        let admit = |stage: &mut StageB, lsn: u64, desc: UsageDesc| {
            let mut f = usage_frame(lsn, &[desc]);
            let admission = stage.admit(&mut f, &IdentityCtx::Suppressed).unwrap();
            (f.spans[0].core.usage_aggregate, admission.relies_on)
        };
        let mut stage = StageB::default();
        assert_eq!(admit(&mut stage, 1, c), (false, None));
        // Frame 2 also relies on frame 1 for the trace, which is not new; the flag relies on it as well.
        assert_eq!(admit(&mut stage, 2, p), (true, Some(Lsn::new(1))));
        // G is flagged through P, on the usage of frame 1, not on frame 2.
        assert_eq!(admit(&mut stage, 3, g), (true, Some(Lsn::new(1))));

        // Once the WAL has collected frame 1, the marks that came from it are gone, as after a rebuild from
        // frame 2 on: P passes nothing on.
        let mut stage = StageB::default();
        admit(&mut stage, 1, c);
        admit(&mut stage, 2, p);
        stage.forget_below(Lsn::new(2));
        assert!(stage.usage_parents.snapshot().is_empty());
        assert!(!admit(&mut stage, 3, g).0);
    }

    #[test]
    fn exact_repeats_drop_and_changed_repeats_stay() {
        let mut stage = StageB::default();
        let a = (0, 0, 0, 1, true);
        let mut f1 = frame(1, &[a, a]);
        assert_eq!(
            stage
                .admit(&mut f1, &IdentityCtx::Suppressed)
                .unwrap()
                .duplicates,
            1
        );
        let mut f2 = frame(5, &[(0, 0, 0, 2, true), a]);
        let admission = stage.admit(&mut f2, &IdentityCtx::Suppressed).unwrap();
        // The changed repeat is kept and becomes the remembered row, so the old digest is new again.
        assert_eq!(admission.duplicates, 0);
        assert_eq!(f2.spans.len(), 2);
        // The same span id in another project is another key.
        let mut f3 = frame(6, &[(1, 0, 0, 1, true)]);
        assert_eq!(
            stage
                .admit(&mut f3, &IdentityCtx::Suppressed)
                .unwrap()
                .duplicates,
            0
        );
    }

    #[test]
    fn provisional_heads_mark_rootless_traces_once() {
        let mut stage = StageB::default();
        // Trace 0 arrives children-first but its root is in the same frame; trace 1 has no root yet.
        let mut f1 = frame(
            1,
            &[
                (0, 0, 1, 0, false),
                (0, 1, 2, 0, false),
                (0, 0, 3, 0, true),
                (0, 1, 4, 0, false),
            ],
        );
        assert_eq!(
            stage
                .admit(&mut f1, &IdentityCtx::Suppressed)
                .unwrap()
                .provisional_heads,
            vec![1]
        );
        // Trace 1 is no longer new, and its late root needs no provisional head.
        let mut f2 = frame(2, &[(0, 1, 5, 0, false), (0, 1, 6, 0, true)]);
        assert!(
            stage
                .admit(&mut f2, &IdentityCtx::Suppressed)
                .unwrap()
                .provisional_heads
                .is_empty()
        );
    }

    #[test]
    fn frames_must_arrive_in_lsn_order() {
        let mut stage = StageB::default();
        stage
            .admit(&mut frame(3, &[]), &IdentityCtx::Suppressed)
            .unwrap();
        let err = stage
            .admit(
                &mut frame(3, &[(0, 0, 0, 0, true)]),
                &IdentityCtx::Suppressed,
            )
            .unwrap_err();
        assert_eq!(
            err,
            OutOfOrder {
                last: Lsn::new(3),
                frame: Lsn::new(3)
            }
        );
        // The refused frame left the state as it was.
        let mut f = frame(4, &[(0, 0, 0, 0, true)]);
        assert_eq!(
            stage
                .admit(&mut f, &IdentityCtx::Suppressed)
                .unwrap()
                .duplicates,
            0
        );
        assert!(
            stage
                .admit(&mut frame(2, &[]), &IdentityCtx::Suppressed)
                .is_err()
        );
    }
}
