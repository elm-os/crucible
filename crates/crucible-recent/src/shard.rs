//! One shard of the index: the entries of the traces that hash to it, and the maps over them. A shard is plain
//! data; the index wraps each one in a read-write lock.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem::size_of;

use crucible_records::{Lsn, ProjectId, SpanId, TimeWindow, Timestamp, TraceId};

use crate::entry::{Entry, Interner, RecentSpan, SpanEntry, SpanSummary};

type TraceKey = (ProjectId, TraceId);
type TimeKey = (ProjectId, Timestamp, SpanId, TraceId);
type LsnKey = (ProjectId, Lsn, TraceId, SpanId);
type OrderKey = (Lsn, ProjectId, TraceId, SpanId);

/// The bytes charged for one entry: the stored form, its key in each map, its place in the eviction queue and
/// an allowance for the tree nodes. Interned strings are charged separately, once per distinct string.
pub const ENTRY_BYTES: u64 = (size_of::<Entry>()
    + size_of::<SpanId>()
    + size_of::<TimeKey>()
    + size_of::<LsnKey>()
    + size_of::<OrderKey>()
    + 32) as u64;

const FIRST_SPAN: SpanId = SpanId::ZERO;
const LAST_SPAN: SpanId = SpanId::from_bytes([0xff; 8]);
const FIRST_TRACE: TraceId = TraceId::ZERO;
const LAST_TRACE: TraceId = TraceId::from_bytes([0xff; 16]);

/// What a tombstone selects inside one project (contract 13.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DeletionSelector {
    /// These traces.
    Traces(Vec<TraceId>),
    /// Spans of this user.
    User(Box<str>),
    /// Spans of this session.
    Session(Box<str>),
    /// Everything.
    Project,
}

#[derive(Debug)]
pub(crate) struct Shard {
    /// By (project, trace): the per-trace summary, rootless traces included.
    traces: BTreeMap<TraceKey, BTreeMap<SpanId, Entry>>,
    /// By (project, start_time, span_id).
    by_time: BTreeSet<TimeKey>,
    /// By (project, lsn), for live tail.
    by_lsn: BTreeSet<LsnKey>,
    /// Insertion order, which is LSN order because there is one writer. A record whose entry was replaced or
    /// removed since is stale and is skipped.
    order: VecDeque<OrderKey>,
    strings: Interner,
    bytes: u64,
}

impl Shard {
    pub(crate) fn new() -> Self {
        Self {
            traces: BTreeMap::new(),
            by_time: BTreeSet::new(),
            by_lsn: BTreeSet::new(),
            order: VecDeque::new(),
            strings: Interner::new(),
            bytes: 0,
        }
    }

    /// The bytes charged to this shard.
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The number of entries.
    pub(crate) fn len(&self) -> usize {
        self.by_lsn.len()
    }

    /// Adds a span, replacing an entry with the same (project, trace, span).
    pub(crate) fn insert(&mut self, lsn: Lsn, received_at: Timestamp, span: &SpanEntry) {
        let s = &span.summary;
        let (project, trace, id) = (span.project_id, s.trace_id, s.span_id);
        self.unlink(project, trace, id);
        let mut strings = [0u32; 6];
        let texts = [
            &s.name,
            &s.trace_name,
            &s.user_id,
            &s.session_id,
            &s.model,
            &s.environment,
        ];
        for (slot, text) in strings.iter_mut().zip(texts) {
            let (interned, added) = self.strings.intern(text);
            *slot = interned;
            self.bytes += added;
        }
        let entry = Entry {
            parent_span_id: s.parent_span_id,
            start_time: s.start_time,
            end_time: s.end_time,
            lsn,
            received_at,
            total_cost_nusd: s.total_cost_nusd,
            input_tokens: s.input_tokens,
            output_tokens: s.output_tokens,
            total_tokens: s.total_tokens,
            locator: span.locator,
            strings,
            observation_type: s.observation_type,
            level: s.level,
            status_code: s.status_code,
            is_root: s.is_root,
            head: span.head,
        };
        self.traces
            .entry((project, trace))
            .or_default()
            .insert(id, entry);
        self.by_time.insert((project, s.start_time, id, trace));
        self.by_lsn.insert((project, lsn, trace, id));
        self.order.push_back((lsn, project, trace, id));
        self.bytes += ENTRY_BYTES;
        self.trim();
    }

    /// Removes one entry from every map.
    fn unlink(&mut self, project: ProjectId, trace: TraceId, id: SpanId) -> bool {
        let Some(spans) = self.traces.get_mut(&(project, trace)) else {
            return false;
        };
        let Some(entry) = spans.remove(&id) else {
            return false;
        };
        if spans.is_empty() {
            self.traces.remove(&(project, trace));
        }
        self.by_time.remove(&(project, entry.start_time, id, trace));
        self.by_lsn.remove(&(project, entry.lsn, trace, id));
        let mut freed = ENTRY_BYTES;
        for interned in entry.strings {
            freed += self.strings.release(interned);
        }
        self.bytes = self.bytes.saturating_sub(freed);
        true
    }

    fn is_live(&self, (lsn, project, trace, id): OrderKey) -> bool {
        self.traces
            .get(&(project, trace))
            .and_then(|spans| spans.get(&id))
            .is_some_and(|entry| entry.lsn == lsn)
    }

    /// Drops stale records from the front of the eviction queue, so that its front is the oldest entry.
    fn trim(&mut self) {
        while let Some(&front) = self.order.front() {
            if self.is_live(front) {
                break;
            }
            self.order.pop_front();
        }
    }

    /// The LSN of the oldest entry.
    pub(crate) fn front_lsn(&self) -> Option<Lsn> {
        self.order.front().map(|record| record.0)
    }

    /// Adds the projects that have an entry with an LSN at or below `lsn`.
    pub(crate) fn projects_through(&self, lsn: Lsn, projects: &mut BTreeSet<ProjectId>) {
        for &record in self.order.iter().take_while(|record| record.0 <= lsn) {
            if self.is_live(record) {
                projects.insert(record.1);
            }
        }
    }

    /// Removes every entry with an LSN at or below `lsn`, oldest first.
    pub(crate) fn remove_through(&mut self, lsn: Lsn) {
        while let Some(&record) = self.order.front() {
            if record.0 > lsn {
                break;
            }
            self.order.pop_front();
            if self.is_live(record) {
                self.unlink(record.1, record.2, record.3);
            }
        }
        self.trim();
    }

    /// Removes the entries of a project that a tombstone selects and that arrived before `requested_at`.
    pub(crate) fn remove_selected(
        &mut self,
        project: ProjectId,
        requested_at: Timestamp,
        selector: &DeletionSelector,
    ) {
        let in_project = (project, FIRST_TRACE)..=(project, LAST_TRACE);
        let mut doomed = Vec::new();
        let mut select = |trace: TraceId,
                          spans: &BTreeMap<SpanId, Entry>,
                          slot: Option<(usize, u32)>| {
            for (id, entry) in spans {
                let selected = slot.is_none_or(|(slot, interned)| entry.strings[slot] == interned);
                if selected && entry.received_at < requested_at {
                    doomed.push((trace, *id));
                }
            }
        };
        match selector {
            DeletionSelector::Traces(ids) => {
                for trace in ids {
                    if let Some(spans) = self.traces.get(&(project, *trace)) {
                        select(*trace, spans, None);
                    }
                }
            }
            DeletionSelector::Project => {
                for ((_, trace), spans) in self.traces.range(in_project) {
                    select(*trace, spans, None);
                }
            }
            DeletionSelector::User(text) | DeletionSelector::Session(text) => {
                let slot = if matches!(selector, DeletionSelector::User(_)) {
                    2
                } else {
                    3
                };
                // A string the shard never interned matches no entry.
                if let Some(interned) = self.strings.id(text) {
                    for ((_, trace), spans) in self.traces.range(in_project) {
                        select(*trace, spans, Some((slot, interned)));
                    }
                }
            }
        }
        for (trace, id) in doomed {
            self.unlink(project, trace, id);
        }
        self.trim();
    }

    fn resolve(&self, trace: TraceId, id: SpanId, entry: &Entry) -> RecentSpan {
        let [name, trace_name, user_id, session_id, model, environment] =
            entry.strings.map(|interned| self.strings.get(interned));
        RecentSpan {
            summary: SpanSummary {
                trace_id: trace,
                span_id: id,
                parent_span_id: entry.parent_span_id,
                is_root: entry.is_root,
                start_time: entry.start_time,
                end_time: entry.end_time,
                observation_type: entry.observation_type,
                level: entry.level,
                status_code: entry.status_code,
                name,
                trace_name,
                user_id,
                session_id,
                model,
                environment,
                total_cost_nusd: entry.total_cost_nusd,
                input_tokens: entry.input_tokens,
                output_tokens: entry.output_tokens,
                total_tokens: entry.total_tokens,
            },
            head: entry.head,
            locator: entry.locator,
            lsn: entry.lsn,
            received_at: entry.received_at,
        }
    }

    fn lookup(&self, project: ProjectId, trace: TraceId, id: SpanId) -> Option<RecentSpan> {
        let entry = self.traces.get(&(project, trace))?.get(&id)?;
        Some(self.resolve(trace, id, entry))
    }

    /// The entries of one trace, in `span_id` order.
    pub(crate) fn trace(&self, project: ProjectId, trace: TraceId) -> Vec<RecentSpan> {
        self.traces
            .get(&(project, trace))
            .map_or_else(Vec::new, |spans| {
                spans
                    .iter()
                    .map(|(id, entry)| self.resolve(trace, *id, entry))
                    .collect()
            })
    }

    /// One entry.
    pub(crate) fn span(
        &self,
        project: ProjectId,
        trace: TraceId,
        id: SpanId,
    ) -> Option<RecentSpan> {
        self.lookup(project, trace, id)
    }

    /// The trace heads of a project whose `start_time` is inside the window, in `(start_time, span_id)` order.
    pub(crate) fn heads(&self, project: ProjectId, window: TimeWindow, out: &mut Vec<RecentSpan>) {
        let range = (project, window.from(), FIRST_SPAN, FIRST_TRACE)
            ..(project, window.to(), FIRST_SPAN, FIRST_TRACE);
        out.extend(
            self.by_time
                .range(range)
                .filter_map(|&(_, _, id, trace)| self.lookup(project, trace, id))
                .filter(|span| span.head.is_some()),
        );
    }

    /// The entries of a project with an LSN above `after`, in LSN order.
    pub(crate) fn since(&self, project: ProjectId, after: Lsn, out: &mut Vec<RecentSpan>) {
        let Some(first) = after.checked_next() else {
            return;
        };
        let range = (project, first, FIRST_TRACE, FIRST_SPAN)
            ..=(project, Lsn::new(u64::MAX), LAST_TRACE, LAST_SPAN);
        out.extend(
            self.by_lsn
                .range(range)
                .filter_map(|&(_, _, trace, id)| self.lookup(project, trace, id)),
        );
    }

    /// The earliest and latest `start_time` of a project's entries.
    pub(crate) fn time_bounds(&self, project: ProjectId) -> Option<(Timestamp, Timestamp)> {
        let range = (
            project,
            Timestamp::from_micros(i64::MIN),
            FIRST_SPAN,
            FIRST_TRACE,
        )
            ..=(
                project,
                Timestamp::from_micros(i64::MAX),
                LAST_SPAN,
                LAST_TRACE,
            );
        let mut keys = self.by_time.range(range);
        let first = keys.next()?.1;
        Some((first, keys.next_back().map_or(first, |key| key.1)))
    }

    /// Checks that the maps agree with each other and with the byte count.
    #[cfg(test)]
    pub(crate) fn check(&self) {
        let entries: usize = self.traces.values().map(BTreeMap::len).sum();
        assert_eq!(entries, self.by_time.len());
        assert_eq!(entries, self.by_lsn.len());
        assert!(self.traces.values().all(|spans| !spans.is_empty()));
        let live = self.order.iter().filter(|&&r| self.is_live(r)).count();
        assert_eq!(live, entries);
        assert!(self.order.front().is_none_or(|&r| self.is_live(r)));
        assert!(
            self.order
                .iter()
                .zip(self.order.iter().skip(1))
                .all(|(a, b)| a.0 <= b.0)
        );
        if entries == 0 {
            assert_eq!(self.bytes, 0);
            assert_eq!(self.strings.len(), 0);
        } else {
            assert!(self.bytes >= entries as u64 * ENTRY_BYTES);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crucible_records::{HeadKind, Level, NanoUsd, ObservationType, StatusCode};

    use super::*;
    use crate::entry::WalLocator;

    fn project(n: u8) -> ProjectId {
        ProjectId::from_bytes([n; 16])
    }

    fn trace(n: u8) -> TraceId {
        TraceId::from_bytes([n; 16])
    }

    fn id(n: u8) -> SpanId {
        SpanId::from_bytes([n; 8])
    }

    fn at(micros: i64) -> Timestamp {
        Timestamp::from_micros(micros)
    }

    fn entry(p: u8, t: u8, s: u8, start: i64, user: &str, head: Option<HeadKind>) -> SpanEntry {
        SpanEntry {
            project_id: project(p),
            summary: SpanSummary {
                trace_id: trace(t),
                span_id: id(s),
                parent_span_id: SpanId::ZERO,
                is_root: head == Some(HeadKind::Root),
                start_time: at(start),
                end_time: at(start + 5),
                observation_type: ObservationType::Span,
                level: Level::Default,
                status_code: StatusCode::Ok,
                name: Arc::from("step"),
                trace_name: Arc::from(""),
                user_id: Arc::from(user),
                session_id: Arc::from("s1"),
                model: Arc::from(""),
                environment: Arc::from(""),
                total_cost_nusd: NanoUsd::ZERO,
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
            },
            head,
            locator: WalLocator {
                segment: 1,
                offset: u64::from(s),
                ordinal: 0,
            },
        }
    }

    fn ids(spans: &[RecentSpan]) -> Vec<SpanId> {
        spans.iter().map(|s| s.summary.span_id).collect()
    }

    fn window(from: i64, to: i64) -> TimeWindow {
        TimeWindow::new(at(from), at(to)).unwrap()
    }

    #[test]
    fn every_map_finds_an_entry_and_only_inside_its_project() {
        let mut shard = Shard::new();
        shard.insert(
            Lsn::new(1),
            at(900),
            &entry(1, 1, 1, 100, "ada", Some(HeadKind::Root)),
        );
        shard.insert(Lsn::new(2), at(901), &entry(1, 1, 2, 110, "ada", None));
        shard.insert(
            Lsn::new(3),
            at(902),
            &entry(1, 2, 3, 90, "bob", Some(HeadKind::Provisional)),
        );
        shard.insert(
            Lsn::new(4),
            at(903),
            &entry(2, 1, 4, 95, "eve", Some(HeadKind::Root)),
        );
        shard.check();
        assert_eq!(shard.len(), 4);

        let found = shard.trace(project(1), trace(1));
        assert_eq!(ids(&found), [id(1), id(2)]);
        assert_eq!((found[0].lsn, found[0].received_at), (Lsn::new(1), at(900)));
        assert_eq!(&*found[0].summary.user_id, "ada");
        assert_eq!(
            found[0].summary,
            entry(1, 1, 1, 100, "ada", Some(HeadKind::Root)).summary
        );
        assert!(shard.trace(project(3), trace(1)).is_empty());
        assert_eq!(ids(&shard.trace(project(2), trace(1))), [id(4)]);
        assert_eq!(
            shard
                .span(project(1), trace(1), id(2))
                .unwrap()
                .locator
                .offset,
            2
        );
        assert!(shard.span(project(2), trace(1), id(2)).is_none());

        // Heads only, inside the half-open window, for one project.
        let mut heads = Vec::new();
        shard.heads(project(1), window(90, 110), &mut heads);
        assert_eq!(ids(&heads), [id(3), id(1)]);
        heads.clear();
        shard.heads(project(1), window(91, 100), &mut heads);
        assert!(heads.is_empty());

        let mut tail = Vec::new();
        shard.since(project(1), Lsn::new(1), &mut tail);
        assert_eq!(ids(&tail), [id(2), id(3)]);
        tail.clear();
        shard.since(project(1), Lsn::new(u64::MAX), &mut tail);
        assert!(tail.is_empty());

        assert_eq!(shard.time_bounds(project(1)), Some((at(90), at(110))));
        assert_eq!(shard.time_bounds(project(2)), Some((at(95), at(95))));
        assert_eq!(shard.time_bounds(project(3)), None);
    }

    #[test]
    fn eviction_is_oldest_lsn_first_and_returns_every_byte() {
        let mut shard = Shard::new();
        for n in 1..=6u8 {
            shard.insert(
                Lsn::new(u64::from(n)),
                at(0),
                &entry(n % 2 + 1, n, n, 100, "ada", None),
            );
        }
        assert!(shard.bytes() > 6 * ENTRY_BYTES);
        assert_eq!(shard.front_lsn(), Some(Lsn::new(1)));
        let mut projects = BTreeSet::new();
        shard.projects_through(Lsn::new(1), &mut projects);
        assert_eq!(projects, BTreeSet::from([project(2)]));
        shard.projects_through(Lsn::new(2), &mut projects);
        assert_eq!(projects.len(), 2);

        shard.remove_through(Lsn::new(4));
        shard.check();
        assert_eq!(shard.len(), 2);
        assert_eq!(shard.front_lsn(), Some(Lsn::new(5)));
        assert!(shard.trace(project(1), trace(4)).is_empty());
        shard.remove_through(Lsn::new(9));
        shard.check();
        assert_eq!(
            (shard.len(), shard.bytes(), shard.front_lsn()),
            (0, 0, None)
        );
    }

    #[test]
    fn a_span_applied_twice_is_one_entry_with_the_later_frame() {
        let mut shard = Shard::new();
        shard.insert(Lsn::new(1), at(0), &entry(1, 1, 1, 100, "ada", None));
        shard.insert(Lsn::new(2), at(0), &entry(1, 1, 2, 100, "ada", None));
        let one = shard.bytes();
        shard.insert(Lsn::new(3), at(7), &entry(1, 1, 1, 120, "bob", None));
        shard.check();
        assert_eq!(shard.len(), 2);
        assert_eq!(shard.bytes(), one + 3 + crate::entry::STRING_OVERHEAD);
        let found = shard.span(project(1), trace(1), id(1)).unwrap();
        assert_eq!(
            (found.lsn, found.summary.start_time),
            (Lsn::new(3), at(120))
        );
        assert_eq!(shard.time_bounds(project(1)), Some((at(100), at(120))));
        // The stale record of LSN 1 is gone from the front, and evicting through it removes nothing live.
        assert_eq!(shard.front_lsn(), Some(Lsn::new(2)));
        shard.remove_through(Lsn::new(2));
        shard.check();
        assert_eq!(ids(&shard.trace(project(1), trace(1))), [id(1)]);
    }

    #[test]
    fn a_tombstone_removes_what_it_selects_and_only_what_arrived_before_it() {
        let filled = || {
            let mut shard = Shard::new();
            shard.insert(Lsn::new(1), at(10), &entry(1, 1, 1, 100, "ada", None));
            shard.insert(Lsn::new(2), at(20), &entry(1, 1, 2, 100, "bob", None));
            shard.insert(Lsn::new(3), at(30), &entry(1, 2, 3, 100, "ada", None));
            shard.insert(Lsn::new(4), at(10), &entry(2, 1, 4, 100, "ada", None));
            shard
        };
        let left = |selector: DeletionSelector, requested_at: i64| {
            let mut shard = filled();
            shard.remove_selected(project(1), at(requested_at), &selector);
            shard.check();
            let mut all = shard.trace(project(1), trace(1));
            all.extend(shard.trace(project(1), trace(2)));
            assert_eq!(
                shard.trace(project(2), trace(1)).len(),
                1,
                "another project is untouched"
            );
            ids(&all)
        };
        assert_eq!(
            left(DeletionSelector::Traces(vec![trace(1), trace(9)]), 99),
            [id(3)]
        );
        assert_eq!(
            left(DeletionSelector::Traces(vec![trace(1)]), 20),
            [id(2), id(3)]
        );
        assert_eq!(left(DeletionSelector::User("ada".into()), 99), [id(2)]);
        assert_eq!(
            left(DeletionSelector::User("ada".into()), 30),
            [id(2), id(3)]
        );
        assert_eq!(
            left(DeletionSelector::User("nobody".into()), 99),
            [id(1), id(2), id(3)]
        );
        assert_eq!(
            left(DeletionSelector::User("".into()), 99),
            [id(1), id(2), id(3)]
        );
        assert_eq!(left(DeletionSelector::Session("s1".into()), 99), []);
        assert_eq!(left(DeletionSelector::Project, 21), [id(3)]);
    }
}
