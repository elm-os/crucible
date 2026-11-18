//! Stage B (contract 3.2 and 5.3): sequential, in LSN order. It drops exact repeats through the duplicate window
//! and marks provisional trace heads through the first-seen set.
//!
//! Both are defined over the LSN stream, never over wall time: the state after a sequence of frames is a function
//! of that sequence alone. Replaying the WAL's retained prefix in LSN order through a fresh [`StageB`] (and
//! discarding the output) therefore rebuilds it, which is recovery step 6 (contract 4.6).
//!
//! The retained prefix gets shorter as the WAL collects garbage, so the state must not remember more than that
//! prefix holds, or a restart would decide otherwise than the process it replaces, and the pairs of one batch
//! that land before and after the restart would disagree. Three rules keep the live state equal to the rebuilt
//! one. Every sighting of a key moves it to the newest position, so both sets hold the keys sighted most
//! recently. [`StageB::forget_below`] drops what was last sighted below the first retained LSN. And
//! [`Admission::relies_on`] names the oldest earlier frame a decision used, which the WAL keeps until the batch
//! of the deciding frame is checkpointed.

use std::collections::VecDeque;

use crucible_records::{DetMap, DetSet, Lsn, SpanCore, det_map, det_set};
use xxhash_rust::xxh3::xxh3_64;

use crate::NormalizedFrame;

/// Keys the duplicate window holds (contract 5.3).
pub const DUPLICATE_WINDOW_KEYS: usize = 4_000_000;

/// Traces the first-seen set holds. The contract bounds only the duplicate window; the first-seen set takes the
/// same bound so that its memory is bounded too.
pub const FIRST_SEEN_TRACES: usize = 4_000_000;

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
    /// The lowest LSN of an earlier frame whose sighting a decision about this frame used: a span dropped as an
    /// exact repeat used the last sighting of its key, and a trace that is not new used the last sighting of
    /// the trace. Stage B rebuilt from a prefix that starts above this LSN could decide otherwise, so the WAL
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

/// Stage B state: the duplicate window, the first-seen set and the last LSN admitted.
#[derive(Debug)]
pub struct StageB {
    /// Span key to the `row_digest` of its last sighting. A span whose key holds an equal digest is an exact
    /// repeat; any other span is kept.
    window: Sightings<u64>,
    /// The trace keys sighted, by every span of a frame, dropped repeats included.
    first_seen: Sightings<()>,
    last: Option<Lsn>,
}

impl Default for StageB {
    /// The contract's bounds: [`DUPLICATE_WINDOW_KEYS`] and [`FIRST_SEEN_TRACES`].
    fn default() -> Self {
        Self::new(DUPLICATE_WINDOW_KEYS, FIRST_SEEN_TRACES)
    }
}

impl StageB {
    /// Empty state with the given bounds.
    pub fn new(window_keys: usize, first_seen_traces: usize) -> Self {
        Self {
            window: Sightings::new(window_keys),
            first_seen: Sightings::new(first_seen_traces),
            last: None,
        }
    }

    /// Admits the next frame of the LSN stream: drops its exact repeats in place and marks its provisional heads.
    /// A frame whose LSN is not above the previous one is refused and leaves the state unchanged.
    pub fn admit(&mut self, frame: &mut NormalizedFrame) -> Result<Admission, OutOfOrder> {
        if let Some(last) = self.last.filter(|&last| frame.lsn <= last) {
            return Err(OutOfOrder {
                last,
                frame: frame.lsn,
            });
        }
        self.last = Some(frame.lsn);
        let lsn = frame.lsn;
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
        let window = &mut self.window;
        let mut stage_a_ordinals = Vec::with_capacity(frame.spans.len());
        let mut ordinal = 0u32;
        frame.spans.retain(|span| {
            let digest = span.core.row_digest;
            let kept = match window.sight(span_key(&span.core), digest, lsn) {
                Some((remembered, at)) if remembered == digest => {
                    rely(at);
                    false
                }
                _ => true,
            };
            duplicates += u32::from(!kept);
            if kept {
                stage_a_ordinals.push(ordinal);
            }
            ordinal = ordinal.saturating_add(1);
            kept
        });
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
        /// ((project, trace, span), digest, LSN of the last sighting), oldest sighting first.
        window: Vec<((u8, u8, u8), u8, u64)>,
        /// ((project, trace), LSN of the last sighting), oldest sighting first.
        seen: Vec<((u8, u8), u64)>,
    }

    impl Oracle {
        /// The kept spans (as indexes into `spans`), the provisional heads (as positions among the kept) and
        /// the lowest earlier LSN a decision used.
        fn admit(&mut self, lsn: u64, spans: &[Desc]) -> (Vec<usize>, Vec<usize>, Option<u64>) {
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
                kept.push(i);
            }
            let mut heads = Vec::new();
            for trace in new {
                let of_trace = |i: &usize| (spans[*i].0, spans[*i].1) == trace;
                if kept.iter().any(|i| of_trace(i) && spans[*i].4) {
                    continue;
                }
                if let Some(pos) = kept.iter().position(|i| of_trace(i)) {
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
                let admission = stage.admit(&mut f).unwrap();
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
                live.admit(&mut frame(n as u64 + 1, spans)).unwrap();
                for at in floors.iter().filter(|&&at| at == n + 1) {
                    live.forget_below(Lsn::new(*at as u64));
                }
            }
            live.forget_below(Lsn::new(floor as u64));
            let mut rebuilt = StageB::new(window_cap, seen_cap);
            for (n, spans) in frames.iter().enumerate().skip(floor - 1) {
                rebuilt.admit(&mut frame(n as u64 + 1, spans)).unwrap();
            }
            prop_assert_eq!(live.window.snapshot(), rebuilt.window.snapshot());
            prop_assert_eq!(live.first_seen.snapshot(), rebuilt.first_seen.snapshot());
            for (n, spans) in rest.iter().enumerate() {
                let lsn = (frames.len() + n) as u64 + 1;
                let (mut a, mut b) = (frame(lsn, spans), frame(lsn, spans));
                prop_assert_eq!(live.admit(&mut a).unwrap(), rebuilt.admit(&mut b).unwrap());
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
                        let a = stage.admit(&mut f).unwrap();
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
