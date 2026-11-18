//! What the index keeps of one span, and the interner behind its strings.

use std::sync::Arc;

use crucible_records::{
    DetMap, HeadKind, Level, Lsn, NanoUsd, ObservationType, ProjectId, SpanCore, SpanId,
    StatusCode, Timestamp, TraceId, det_map,
};

/// Where a span's frame lives in the WAL: the payload of a recent span is re-read from here and re-normalized
/// (contract 8.6).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WalLocator {
    /// The segment number.
    pub segment: u64,
    /// The byte offset of the frame in the segment.
    pub offset: u64,
    /// The ordinal of the span in the frame, in wire order.
    pub ordinal: u32,
}

/// The list-and-tree view of a span: ids, times, type, level, model, cost, tokens, name, user and session. It
/// holds no payload and no preview. A span read from the store decodes to the same type, so both sources merge.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanSummary {
    /// The trace.
    pub trace_id: TraceId,
    /// The span.
    pub span_id: SpanId,
    /// The parent; [`SpanId::ZERO`] when absent.
    pub parent_span_id: SpanId,
    /// No parent, or the dialect's root flag.
    pub is_root: bool,
    /// Start of the span.
    pub start_time: Timestamp,
    /// End of the span.
    pub end_time: Timestamp,
    /// The observation type.
    pub observation_type: ObservationType,
    /// The level.
    pub level: Level,
    /// The status.
    pub status_code: StatusCode,
    /// The span name.
    pub name: Arc<str>,
    /// The trace-level name (contract 3.3).
    pub trace_name: Arc<str>,
    /// The trace-level user.
    pub user_id: Arc<str>,
    /// The trace-level session.
    pub session_id: Arc<str>,
    /// The response model.
    pub model: Arc<str>,
    /// The total cost.
    pub total_cost_nusd: NanoUsd,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
}

impl SpanSummary {
    /// The summary of a normalized span.
    pub fn of(core: &SpanCore) -> Self {
        Self {
            trace_id: core.trace_id,
            span_id: core.span_id,
            parent_span_id: core.parent_span_id,
            is_root: core.is_root,
            start_time: core.start_time,
            end_time: core.end_time,
            observation_type: core.observation_type,
            level: core.level,
            status_code: core.status_code,
            name: Arc::from(&*core.name),
            trace_name: Arc::from(&*core.trace_name),
            user_id: Arc::from(&*core.user_id),
            session_id: Arc::from(&*core.session_id),
            model: Arc::from(&*core.model_response),
            total_cost_nusd: core.total_cost_nusd,
            input_tokens: core.input_tokens,
            output_tokens: core.output_tokens,
            total_tokens: core.total_tokens,
        }
    }
}

/// One span as stage B hands it to the index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanEntry {
    /// The project, from the frame's envelope.
    pub project_id: ProjectId,
    /// The span.
    pub summary: SpanSummary,
    /// The trace head stage B emits for this span, if any (contract 3.9): `root` for a root span, `provisional`
    /// the first time the trace is seen without a root.
    pub head: Option<HeadKind>,
    /// Where the span's frame lives.
    pub locator: WalLocator,
}

/// One span as the index returns it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecentSpan {
    /// The span.
    pub summary: SpanSummary,
    /// The trace head stage B emitted for this span, if any.
    pub head: Option<HeadKind>,
    /// Where the span's frame lives.
    pub locator: WalLocator,
    /// The LSN of the frame that carried the span.
    pub lsn: Lsn,
    /// When the WAL accepted the frame.
    pub received_at: Timestamp,
}

/// The stored form: the keys live in the maps, the strings in the interner.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub(crate) parent_span_id: SpanId,
    pub(crate) start_time: Timestamp,
    pub(crate) end_time: Timestamp,
    pub(crate) lsn: Lsn,
    pub(crate) received_at: Timestamp,
    pub(crate) total_cost_nusd: NanoUsd,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) locator: WalLocator,
    pub(crate) strings: [u32; 5],
    pub(crate) observation_type: ObservationType,
    pub(crate) level: Level,
    pub(crate) status_code: StatusCode,
    pub(crate) is_root: bool,
    pub(crate) head: Option<HeadKind>,
}

/// The bytes charged for an interned string besides its text: the slot, the map entry and the allocation header.
pub(crate) const STRING_OVERHEAD: u64 = 64;

#[derive(Debug)]
struct Slot {
    text: Arc<str>,
    refs: u32,
}

/// Reference-counted strings: one copy of each distinct name, user, session and model per shard.
#[derive(Debug)]
pub(crate) struct Interner {
    ids: DetMap<Arc<str>, u32>,
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl Interner {
    /// An interner whose id 0 is the empty string, which is never released.
    pub(crate) fn new() -> Self {
        let empty: Arc<str> = Arc::from("");
        Self {
            ids: det_map(),
            slots: vec![Slot {
                text: empty,
                refs: 0,
            }],
            free: Vec::new(),
        }
    }

    /// The id of `text`, taking one reference, and the bytes this added.
    pub(crate) fn intern(&mut self, text: &Arc<str>) -> (u32, u64) {
        if text.is_empty() {
            return (0, 0);
        }
        if let Some(&id) = self.ids.get(text) {
            if let Some(slot) = self.slots.get_mut(id as usize) {
                slot.refs += 1;
            }
            return (id, 0);
        }
        let slot = Slot {
            text: Arc::clone(text),
            refs: 1,
        };
        let id = match self.free.pop() {
            Some(id) => {
                if let Some(place) = self.slots.get_mut(id as usize) {
                    *place = slot;
                }
                id
            }
            None => {
                self.slots.push(slot);
                (self.slots.len() - 1) as u32
            }
        };
        self.ids.insert(Arc::clone(text), id);
        (id, text.len() as u64 + STRING_OVERHEAD)
    }

    /// Drops one reference and returns the bytes this freed.
    pub(crate) fn release(&mut self, id: u32) -> u64 {
        let Some(slot) = self.slots.get_mut(id as usize).filter(|_| id != 0) else {
            return 0;
        };
        slot.refs = slot.refs.saturating_sub(1);
        if slot.refs > 0 {
            return 0;
        }
        let text = std::mem::replace(&mut slot.text, Arc::from(""));
        self.ids.remove(&text);
        self.free.push(id);
        text.len() as u64 + STRING_OVERHEAD
    }

    /// The string of an id.
    pub(crate) fn get(&self, id: u32) -> Arc<str> {
        self.slots
            .get(id as usize)
            .map_or_else(|| Arc::from(""), |slot| Arc::clone(&slot.text))
    }

    /// The id of a string that is held.
    pub(crate) fn id(&self, text: &str) -> Option<u32> {
        self.ids.get(text).copied()
    }

    /// The number of distinct strings held, without the empty one.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_shared_counted_and_released() {
        let mut strings = Interner::new();
        let (a, b): (Arc<str>, Arc<str>) = (Arc::from("checkout"), Arc::from("search"));
        assert_eq!(strings.intern(&Arc::from("")), (0, 0));
        let (id_a, added) = strings.intern(&a);
        assert_eq!(added, 8 + STRING_OVERHEAD);
        assert_eq!(strings.intern(&a), (id_a, 0));
        let (id_b, _) = strings.intern(&b);
        assert_ne!(id_a, id_b);
        assert_eq!(&*strings.get(id_b), "search");
        assert_eq!(strings.release(id_a), 0);
        assert_eq!(strings.release(id_a), 8 + STRING_OVERHEAD);
        assert_eq!(strings.len(), 1);
        // The freed slot is reused, and the empty string is never released.
        let (id_c, _) = strings.intern(&Arc::from("tool"));
        assert_eq!(id_c, id_a);
        assert_eq!(strings.release(0), 0);
        assert_eq!(&*strings.get(0), "");
        assert_eq!(&*strings.get(99), "");
    }
}
