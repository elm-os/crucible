//! Stage B (contract 3.2 and 5.3): sequential, in LSN order. It drops exact repeats through the duplicate window,
//! marks provisional trace heads through the first-seen set, and flags usage aggregates through the usage-parent
//! set.
//!
//! **Usage aggregates.** A span's usage and cost count toward trace, session, user and metrics totals unless it
//! has `usage_aggregate`: at least one descendant that carries usage or cost, seen in the same frame or earlier
//! in the LSN stream. The usage-parent set holds the span ids that have such a descendant. For every kept span
//! that carries usage or cost, or is itself in the set, its parent id joins the set; within one frame this runs
//! over all spans before any is flagged, so wire order inside a frame does not matter. Across frames it relies
//! on children ending, and so being exported, no later than their parents. A span is flagged when its id is in
//! the set, whatever its type. Known limit: a descendant that arrives after its ancestor, or after the set
//! evicted the ancestor's id, is counted beside the ancestor.
//!
//! Both are defined over the LSN stream, never over wall time: the state after a sequence of frames is a function
//! of that sequence alone. Replaying the WAL's retained prefix in LSN order through a fresh [`StageB`] (and
//! discarding the output) therefore rebuilds it, which is recovery step 6 (contract 4.6).

use std::collections::VecDeque;

use crucible_records::{DetMap, DetSet, Lsn, SpanCore, SpanId, det_map, det_set};
use xxhash_rust::xxh3::xxh3_64;

use crate::NormalizedFrame;

/// Keys the duplicate window holds (contract 5.3).
pub const DUPLICATE_WINDOW_KEYS: usize = 4_000_000;

/// Traces the first-seen set holds. The contract bounds only the duplicate window; the first-seen set takes the
/// same bound so that its memory is bounded too.
pub const FIRST_SEEN_TRACES: usize = 4_000_000;

/// Span ids the usage-parent set holds; when full, the id that joined earliest in the LSN stream is evicted.
pub const USAGE_PARENT_SPANS: usize = 4_000_000;

/// The usage-parent key of a span's parent: as [`span_key`], with the parent's span id.
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

/// The latest kept `row_digest` of each span key, for at most `capacity` keys.
///
/// A span whose key is present with an equal digest is a duplicate. Any other span is kept and becomes its key's
/// remembered row, at the newest position of the eviction order: a repeat with a different digest moves its key
/// there. When a new key takes the window above its capacity, the key whose remembered row is oldest in the LSN
/// stream is evicted (rows of one frame in wire order).
#[derive(Debug)]
struct DuplicateWindow {
    capacity: usize,
    /// Key to (digest, sequence number of its remembered row).
    entries: DetMap<u64, (u64, u64)>,
    /// (sequence number, key) in eviction order. A position whose key has since moved is stale and skipped.
    order: VecDeque<(u64, u64)>,
    next: u64,
}

impl DuplicateWindow {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: det_map(),
            order: VecDeque::new(),
            next: 0,
        }
    }

    /// Whether the span is kept (`false` for a duplicate). A kept span is remembered.
    fn admit(&mut self, key: u64, digest: u64) -> bool {
        if self.entries.get(&key).is_some_and(|&(d, _)| d == digest) {
            return false;
        }
        let seq = self.next;
        self.next += 1;
        let moved = self.entries.insert(key, (digest, seq)).is_some();
        self.order.push_back((seq, key));
        if moved {
            self.compact();
        } else {
            while self.entries.len() > self.capacity {
                self.evict_oldest();
            }
        }
        true
    }

    fn live(entries: &DetMap<u64, (u64, u64)>, seq: u64, key: u64) -> bool {
        entries.get(&key).is_some_and(|&(_, s)| s == seq)
    }

    fn evict_oldest(&mut self) {
        while let Some((seq, key)) = self.order.pop_front() {
            if Self::live(&self.entries, seq, key) {
                self.entries.remove(&key);
                return;
            }
        }
    }

    /// Drops stale positions once they outnumber the live ones, so the order stays within twice the window.
    fn compact(&mut self) {
        if self.order.len() > 2 * self.entries.len() + 16 {
            let entries = &self.entries;
            self.order
                .retain(|&(seq, key)| Self::live(entries, seq, key));
        }
    }

    /// The remembered (key, digest) pairs, oldest first.
    #[cfg(test)]
    fn snapshot(&self) -> Vec<(u64, u64)> {
        self.order
            .iter()
            .filter(|&&(seq, key)| Self::live(&self.entries, seq, key))
            .map(|&(_, key)| (key, self.entries[&key].0))
            .collect()
    }
}

/// A bounded set of keys in arrival order: the trace keys stage B has seen, or the span keys that have a
/// descendant carrying usage. When full, the key that joined earliest is evicted.
#[derive(Debug)]
struct FirstSeen {
    capacity: usize,
    keys: DetSet<u64>,
    order: VecDeque<u64>,
}

impl FirstSeen {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            keys: det_set(),
            order: VecDeque::new(),
        }
    }

    /// Records the trace; `true` when it was not in the set.
    fn insert(&mut self, key: u64) -> bool {
        if !self.keys.insert(key) {
            return false;
        }
        self.order.push_back(key);
        while self.keys.len() > self.capacity {
            match self.order.pop_front() {
                Some(oldest) => self.keys.remove(&oldest),
                None => break,
            };
        }
        true
    }

    fn contains(&self, key: u64) -> bool {
        self.keys.contains(&key)
    }
}

/// What stage B did with one frame.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Admission {
    /// Spans dropped as exact repeats.
    pub duplicates: u32,
    /// For each remaining span, its position among the spans stage A made of the frame. A re-read of the frame
    /// through [`crate::normalize`] finds the span there, whatever stage B dropped before it, so this is the
    /// ordinal of the recent-span index's WAL locator (contract 8.6).
    pub stage_a_ordinals: Vec<u32>,
    /// Positions in the frame's remaining spans of the provisional heads: for each trace first seen in this frame
    /// with no root span in it, the trace's first span in wire order. Root heads need no marking: every span with
    /// `is_root` heads its trace (contract 3.9).
    pub provisional_heads: Vec<usize>,
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

/// Stage B state: the duplicate window, the first-seen set, the usage-parent set and the last LSN admitted.
#[derive(Debug)]
pub struct StageB {
    window: DuplicateWindow,
    first_seen: FirstSeen,
    usage_parents: FirstSeen,
    last: Option<Lsn>,
}

impl Default for StageB {
    /// The contract's bounds: [`DUPLICATE_WINDOW_KEYS`], [`FIRST_SEEN_TRACES`] and [`USAGE_PARENT_SPANS`].
    fn default() -> Self {
        Self::new(DUPLICATE_WINDOW_KEYS, FIRST_SEEN_TRACES)
    }
}

impl StageB {
    /// Empty state with the given bounds, and [`USAGE_PARENT_SPANS`] for the usage-parent set.
    pub fn new(window_keys: usize, first_seen_traces: usize) -> Self {
        Self {
            window: DuplicateWindow::new(window_keys),
            first_seen: FirstSeen::new(first_seen_traces),
            usage_parents: FirstSeen::new(USAGE_PARENT_SPANS),
            last: None,
        }
    }

    /// The state with another bound on the usage-parent set.
    #[must_use]
    pub fn with_usage_parents(mut self, spans: usize) -> Self {
        self.usage_parents = FirstSeen::new(spans);
        self
    }

    /// Flags the usage aggregates of a frame's kept spans and records their parents.
    fn mark_usage_aggregates(&mut self, frame: &mut NormalizedFrame) {
        // The frame's spans by their own key; a span id that repeats resolves to its first occurrence.
        let mut by_key: DetMap<u64, usize> = det_map();
        for (i, span) in frame.spans.iter().enumerate() {
            by_key.entry(span_key(&span.core)).or_insert(i);
        }
        for start in 0..frame.spans.len() {
            let core = &frame.spans[start].core;
            if !core.carries_usage() && !self.usage_parents.contains(span_key(core)) {
                continue;
            }
            // Climb while a parent newly joins the set: a parent that was in it already has been climbed from,
            // or is a start of its own, so the work is linear and a parent cycle ends.
            let mut at = start;
            loop {
                let core = &frame.spans[at].core;
                // A span that names itself as its parent is not its own descendant.
                if core.parent_span_id == SpanId::ZERO || core.parent_span_id == core.span_id {
                    break;
                }
                let parent = parent_key(core);
                if !self.usage_parents.insert(parent) {
                    break;
                }
                match by_key.get(&parent) {
                    Some(&next) => at = next,
                    None => break,
                }
            }
        }
        for span in &mut frame.spans {
            span.core.usage_aggregate = self.usage_parents.contains(span_key(&span.core));
        }
    }

    /// Admits the next frame of the LSN stream: drops its exact repeats in place, flags its usage aggregates and
    /// marks its provisional heads.
    /// A frame whose LSN is not above the previous one is refused and leaves the state unchanged.
    pub fn admit(&mut self, frame: &mut NormalizedFrame) -> Result<Admission, OutOfOrder> {
        if let Some(last) = self.last.filter(|&last| frame.lsn <= last) {
            return Err(OutOfOrder {
                last,
                frame: frame.lsn,
            });
        }
        self.last = Some(frame.lsn);
        let mut duplicates = 0u32;
        let window = &mut self.window;
        let mut stage_a_ordinals = Vec::with_capacity(frame.spans.len());
        let mut ordinal = 0u32;
        frame.spans.retain(|span| {
            let kept = window.admit(span_key(&span.core), span.core.row_digest);
            duplicates += u32::from(!kept);
            if kept {
                stage_a_ordinals.push(ordinal);
            }
            ordinal = ordinal.saturating_add(1);
            kept
        });
        self.mark_usage_aggregates(frame);
        let mut first: Vec<(u64, usize)> = Vec::new();
        let mut noted: DetSet<u64> = det_set();
        let mut rooted: DetSet<u64> = det_set();
        for (i, span) in frame.spans.iter().enumerate() {
            let key = trace_key(&span.core);
            if span.core.is_root {
                rooted.insert(key);
            }
            if self.first_seen.insert(key) && noted.insert(key) {
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
            stage_a_ordinals,
            provisional_heads,
        })
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::{ProjectId, SpanId, SpanRecord, TraceId};
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;
    use crate::FrameSummary;
    use crate::test_support::record;

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
            summary: FrameSummary::default(),
        }
    }

    /// A linear model of stage B over the descriptions themselves: no hashing, no maps, no stale positions.
    struct Oracle {
        window_cap: usize,
        seen_cap: usize,
        /// ((project, trace, span), digest), oldest first.
        window: Vec<((u8, u8, u8), u8)>,
        /// (project, trace), oldest first.
        seen: Vec<(u8, u8)>,
    }

    impl Oracle {
        /// The kept spans (as indexes into `spans`) and the provisional heads (as positions among the kept).
        fn admit(&mut self, spans: &[Desc]) -> (Vec<usize>, Vec<usize>) {
            let mut kept = Vec::new();
            for (i, &(p, t, s, d, _)) in spans.iter().enumerate() {
                let key = (p, t, s);
                if let Some(pos) = self.window.iter().position(|(k, _)| *k == key) {
                    if self.window[pos].1 == d {
                        continue;
                    }
                    self.window.remove(pos);
                    self.window.push((key, d));
                } else {
                    self.window.push((key, d));
                    if self.window.len() > self.window_cap {
                        self.window.remove(0);
                    }
                }
                kept.push(i);
            }
            let mut new: Vec<((u8, u8), usize)> = Vec::new();
            for (pos, &i) in kept.iter().enumerate() {
                let (p, t, ..) = spans[i];
                if !self.seen.contains(&(p, t)) {
                    self.seen.push((p, t));
                    if self.seen.len() > self.seen_cap {
                        self.seen.remove(0);
                    }
                    if !new.iter().any(|(trace, _)| *trace == (p, t)) {
                        new.push(((p, t), pos));
                    }
                }
            }
            let heads = new
                .into_iter()
                .filter(|&((p, t), _)| {
                    !kept
                        .iter()
                        .any(|&i| spans[i].0 == p && spans[i].1 == t && spans[i].4)
                })
                .map(|(_, pos)| pos)
                .collect();
            (kept, heads)
        }
    }

    fn desc() -> impl Strategy<Value = Desc> {
        (0u8..2, 0u8..4, 0u8..4, 0u8..3, any::<bool>())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// Stage B agrees with the linear model on every frame: which spans it keeps, the duplicate count, the
        /// provisional heads, and the window's remembered rows in eviction order.
        #[test]
        fn stage_b_matches_the_oracle(
            frames in vec(vec(desc(), 0..10), 1..12),
            window_cap in 0usize..8,
            seen_cap in 0usize..8,
        ) {
            let mut stage = StageB::new(window_cap, seen_cap);
            let mut oracle = Oracle { window_cap, seen_cap, window: Vec::new(), seen: Vec::new() };
            for (n, spans) in frames.iter().enumerate() {
                let mut f = frame(n as u64 + 1, spans);
                let admission = stage.admit(&mut f).unwrap();
                let (kept, heads) = oracle.admit(spans);
                let expected: Vec<SpanRecord> = kept.iter().map(|&i| span(&spans[i])).collect();
                prop_assert_eq!(&f.spans, &expected);
                prop_assert_eq!(admission.duplicates as usize, spans.len() - kept.len());
                // Each kept span names its position in the frame as stage A made it.
                let ordinals: Vec<u32> = kept.iter().map(|&i| i as u32).collect();
                prop_assert_eq!(&admission.stage_a_ordinals, &ordinals);
                prop_assert_eq!(&admission.provisional_heads, &heads);
                let model: Vec<(u64, u64)> = oracle
                    .window
                    .iter()
                    .map(|&((p, t, s), d)| (span_key(&span(&(p, t, s, d, false)).core), u64::from(d)))
                    .collect();
                prop_assert_eq!(stage.window.snapshot(), model);
                prop_assert!(stage.window.order.len() <= 2 * stage.window.entries.len() + 17);
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
                        let a = stage.admit(&mut f).unwrap();
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
    }

    #[test]
    fn exact_repeats_drop_and_changed_repeats_stay() {
        let mut stage = StageB::default();
        let a = (0, 0, 0, 1, true);
        let mut f1 = frame(1, &[a, a]);
        assert_eq!(stage.admit(&mut f1).unwrap().duplicates, 1);
        let mut f2 = frame(5, &[(0, 0, 0, 2, true), a]);
        let admission = stage.admit(&mut f2).unwrap();
        // The changed repeat is kept and becomes the remembered row, so the old digest is new again.
        assert_eq!(admission.duplicates, 0);
        assert_eq!(f2.spans.len(), 2);
        // The same span id in another project is another key.
        let mut f3 = frame(6, &[(1, 0, 0, 1, true)]);
        assert_eq!(stage.admit(&mut f3).unwrap().duplicates, 0);
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
        assert_eq!(stage.admit(&mut f1).unwrap().provisional_heads, vec![1]);
        // Trace 1 is no longer new, and its late root needs no provisional head.
        let mut f2 = frame(2, &[(0, 1, 5, 0, false), (0, 1, 6, 0, true)]);
        assert!(stage.admit(&mut f2).unwrap().provisional_heads.is_empty());
    }

    #[test]
    fn frames_must_arrive_in_lsn_order() {
        let mut stage = StageB::default();
        stage.admit(&mut frame(3, &[])).unwrap();
        let err = stage
            .admit(&mut frame(3, &[(0, 0, 0, 0, true)]))
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
        assert_eq!(stage.admit(&mut f).unwrap().duplicates, 0);
        assert!(stage.admit(&mut frame(2, &[])).is_err());
    }
}
