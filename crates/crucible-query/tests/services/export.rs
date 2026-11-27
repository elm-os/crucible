//! Export pages (contracts 8.7 and 12.8): every selected span exactly once across the pages, whatever the row
//! and byte limits and however many copies of a span the store holds; the copy with the greatest LSN, with the
//! payload row of that copy; the effective facts of the span and of its trace; a page that fails when the store
//! is lost between its reads; and the wait for spans that are still on their way to the store.

use std::collections::{BTreeMap, BTreeSet};

use crucible_env::QueryError;
use crucible_plan::{FactTable, MAX_LIST, Value};
use crucible_query::{ExportLimits, ExportQuery, ExportWait, QueryServiceError};
use crucible_records::{
    FactTargetKind, Lsn, SpanId, TableId, TimeWindow, Timestamp, TraceId, Uuid,
};
use proptest::prelude::*;

use crate::store::stored_row;
use crate::world::{NOW, World, ctx, project, run, span, span_id, span_payload_row, trace};

const MINUTE: i64 = 60_000_000;
const DAY: i64 = 86_400_000_000;

/// One stored copy of a span of `project`: the core row and its payload row, whose input names its LSN.
#[derive(Clone, Copy, Debug)]
struct Copy {
    project: u8,
    trace: u64,
    span: u64,
    start: Timestamp,
    lsn: u64,
    event_bytes: u32,
}

fn bytes(value: impl AsRef<[u8]>) -> Value {
    Value::Bytes(value.as_ref().to_vec())
}

fn received(lsn: u64) -> Timestamp {
    NOW.saturating_add_micros(i64::try_from(lsn).unwrap())
}

fn input(lsn: u64) -> String {
    format!("copy-{lsn}")
}

fn store(world: &World, copy: &Copy) {
    let (trace_id, id) = (trace(copy.trace), span_id(copy.span));
    world.store.insert(
        TableId::SpanCore,
        stored_row(
            TableId::SpanCore,
            &[
                ("project_id", Value::Uuid(project(copy.project).uuid())),
                ("start_minute", Value::Time(copy.start.floor_minute())),
                ("trace_hash", Value::UInt(u64::from(trace_id.trace_hash()))),
                ("start_time", Value::Time(copy.start)),
                ("end_time", Value::Time(copy.start.saturating_add_micros(5))),
                ("span_id", bytes(id.as_bytes())),
                ("trace_id", bytes(trace_id.as_bytes())),
                ("name", Value::String(format!("span-{}", copy.span))),
                ("event_bytes", Value::UInt(u64::from(copy.event_bytes))),
                ("lsn", Value::UInt(copy.lsn)),
                ("received_at", Value::Time(received(copy.lsn))),
            ],
        ),
    );
    let mut entry = span(copy.trace, copy.span, 0, copy.start);
    entry.project_id = project(copy.project);
    let payload = span_payload_row(
        &entry,
        Lsn::new(copy.lsn),
        received(copy.lsn),
        &input(copy.lsn),
        &[],
    );
    world.store.insert(TableId::SpanPayload, payload);
}

fn query(as_of: Timestamp) -> ExportQuery {
    ExportQuery {
        window: TimeWindow::new(
            NOW.saturating_add_micros(-DAY),
            NOW.saturating_add_micros(DAY),
        )
        .unwrap(),
        trace_ids: Vec::new(),
        user_id: None,
        session_id: None,
        as_of,
    }
}

/// What every page of an export holds, in order: `(trace, span, kept lsn, payload input)`.
fn export_all(
    world: &World,
    query: &ExportQuery,
    limits: ExportLimits,
) -> Result<Vec<(TraceId, SpanId, u64, String)>, QueryServiceError> {
    let mut out = Vec::new();
    let mut after = None;
    for _ in 0..10_000 {
        let page = run(world.service.export_page(&ctx(1), query, after, limits))?;
        for record in page.records {
            let core = &record.span.core;
            let payload = String::from_utf8(record.span.payload.input.to_vec()).unwrap();
            out.push((
                core.trace_id,
                core.span_id,
                core.ingestion.lsn.get(),
                payload,
            ));
        }
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(out),
        }
    }
    panic!("the export did not end");
}

/// A span's position in the key order of `span_core`.
type Key = (Timestamp, u32, Timestamp, SpanId);

/// A span's position in the key order of `span_core`.
fn key_of(copy: &Copy) -> Key {
    let trace_id = trace(copy.trace);
    (
        copy.start.floor_minute(),
        trace_id.trace_hash(),
        copy.start,
        span_id(copy.span),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn every_selected_span_is_exported_once_with_its_newest_copy(
        spans in prop::collection::btree_map(
            (0u64..4, 1u64..8),
            (0i64..3, 0i64..2, 1usize..4, 0u32..3_000),
            1..24,
        ),
        other_project in prop::collection::vec((0u64..4, 1u64..8), 0..4),
        rows in 1u32..7,
        bytes in 1u64..6_000,
        late in 0usize..3,
    ) {
        let world = World::new(16);
        let mut copies = Vec::new();
        let mut lsn = 1;
        for (&(trace_n, span_n), &(minute, micros, count, event_bytes)) in &spans {
            let start = NOW.saturating_add_micros(minute * MINUTE + micros);
            for _ in 0..count {
                copies.push(Copy { project: 1, trace: trace_n, span: span_n, start, lsn, event_bytes });
                lsn += 1;
            }
        }
        // Copies land in an order unrelated to their LSNs.
        copies.reverse();
        for &(trace_n, span_n) in &other_project {
            copies.push(Copy { project: 2, trace: trace_n, span: span_n, start: NOW, lsn, event_bytes: 1 });
            lsn += 1;
        }
        for copy in &copies {
            store(&world, copy);
        }
        // The last `late` copies of project 1 arrived after the export's as_of.
        let mut by_lsn: Vec<&Copy> = copies.iter().filter(|c| c.project == 1).collect();
        by_lsn.sort_by_key(|c| c.lsn);
        let cut = by_lsn.len().saturating_sub(late);
        let as_of = received(by_lsn.get(cut.wrapping_sub(1)).map_or(0, |c| c.lsn));
        let visible = &by_lsn[..cut];

        let mut expected: BTreeMap<Key, (TraceId, SpanId, u64)> = BTreeMap::new();
        for copy in visible {
            let entry = expected.entry(key_of(copy)).or_insert((trace(copy.trace), span_id(copy.span), 0));
            entry.2 = entry.2.max(copy.lsn);
        }
        let expected: Vec<_> = expected
            .into_values()
            .map(|(t, s, kept)| (t, s, kept, input(kept)))
            .collect();

        let got = export_all(&world, &query(as_of), ExportLimits { rows, bytes }).unwrap();
        prop_assert_eq!(got, expected);
    }
}

/// A selection of more traces than one plan names is scanned one chunk of traces per plan, and the chunks' rows
/// merged in key order: every selected span once, in key order, whatever the page size, also when the copies of
/// one span fill a page.
#[test]
fn a_selection_of_more_traces_than_one_plan_names_is_exported_in_key_order() {
    let world = World::new(16);
    let traces = 2 * MAX_LIST as u64 + 3;
    let mut copies = Vec::new();
    for n in 0..traces {
        // Trace 1's span starts first; three copies of it fill the first page of two rows.
        let minutes = if n == 1 {
            -1
        } else {
            i64::try_from(n % 5).unwrap()
        };
        copies.push(Copy {
            project: 1,
            trace: n,
            span: n + 1,
            start: NOW.saturating_add_micros(minutes * MINUTE),
            lsn: n + 1,
            event_bytes: 1,
        });
    }
    for lsn in [traces + 1, traces + 2] {
        copies.push(Copy { lsn, ..copies[1] });
    }
    for copy in &copies {
        store(&world, copy);
    }
    // Every trace but the multiples of three, named in no order and one of them twice.
    let selected: Vec<u64> = (0..traces).rev().filter(|n| n % 3 != 0).collect();
    let mut q = query(received(traces + 9));
    q.trace_ids = selected.iter().map(|n| trace(*n)).collect();
    q.trace_ids.push(trace(selected[7]));
    assert!(q.trace_ids.len() > MAX_LIST);
    let mut expected: BTreeMap<Key, (TraceId, SpanId, u64)> = BTreeMap::new();
    for copy in copies.iter().filter(|c| c.trace % 3 != 0) {
        let entry =
            expected
                .entry(key_of(copy))
                .or_insert((trace(copy.trace), span_id(copy.span), 0));
        entry.2 = entry.2.max(copy.lsn);
    }
    let expected: Vec<_> = expected
        .into_values()
        .map(|(t, s, kept)| (t, s, kept, input(kept)))
        .collect();
    for rows in [1_000, 97] {
        let limits = ExportLimits {
            rows,
            ..ExportLimits::default()
        };
        assert_eq!(
            export_all(&world, &q, limits).unwrap(),
            expected,
            "{rows} rows"
        );
    }
    // The three copies fill a page of two: the page holds the span whole, and the next one starts after it. A
    // full page leaves out its last run, which may go on past the page.
    let two = ExportLimits {
        rows: 2,
        ..ExportLimits::default()
    };
    let first = run(world.service.export_page(&ctx(1), &q, None, two)).unwrap();
    let ids = |records: &[crucible_query::ExportRecord]| -> Vec<(SpanId, u64)> {
        records
            .iter()
            .map(|r| (r.span.core.span_id, r.span.core.ingestion.lsn.get()))
            .collect()
    };
    assert_eq!(ids(&first.records), [(span_id(2), traces + 2)]);
    assert_eq!(first.next, Some(key_of(&copies[1])));
    let second = run(world.service.export_page(&ctx(1), &q, first.next, two)).unwrap();
    let (_, next, kept, _) = &expected[1];
    assert_eq!(ids(&second.records), [(*next, *kept)]);
}

#[test]
fn a_page_whose_payload_read_loses_the_store_fails_instead_of_leaving_the_payload_out() {
    let world = World::new(16);
    for n in 1..=3 {
        let copy = Copy {
            project: 1,
            trace: 1,
            span: n,
            start: NOW,
            lsn: n,
            event_bytes: 10,
        };
        store(&world, &copy);
    }
    world
        .store
        .during_next_read(|store| store.set_unavailable(true));
    let result =
        run(world
            .service
            .export_page(&ctx(1), &query(received(9)), None, ExportLimits::default()));
    assert!(
        matches!(
            result,
            Err(QueryServiceError::Store(QueryError::Unavailable(_)))
        ),
        "{result:?}"
    );
    world.store.set_unavailable(false);
    let page =
        run(world
            .service
            .export_page(&ctx(1), &query(received(9)), None, ExportLimits::default()))
        .unwrap();
    assert_eq!(page.records.len(), 3);
    assert!(
        page.records
            .iter()
            .all(|r| r.payload_stored && !r.span.payload.input.is_empty())
    );
}

#[test]
fn the_selection_narrows_to_traces_users_and_sessions() {
    let world = World::new(16);
    for n in 1..=4 {
        let copy = Copy {
            project: 1,
            trace: n % 2,
            span: n,
            start: NOW,
            lsn: n,
            event_bytes: 1,
        };
        store(&world, &copy);
    }
    let mut q = query(received(9));
    q.trace_ids = vec![trace(1)];
    let spans = export_all(&world, &q, ExportLimits::default()).unwrap();
    let ids: BTreeSet<u64> = spans
        .iter()
        .map(|(_, s, ..)| u64::from_be_bytes(s.to_bytes()))
        .collect();
    assert_eq!(ids, BTreeSet::from([1, 3]));
    // No span of the fixture has a user or session.
    q.trace_ids.clear();
    q.user_id = Some("someone".to_owned());
    assert!(
        export_all(&world, &q, ExportLimits::default())
            .unwrap()
            .is_empty()
    );
    q.user_id = None;
    q.session_id = Some("a session".to_owned());
    assert!(
        export_all(&world, &q, ExportLimits::default())
            .unwrap()
            .is_empty()
    );
}

fn fact_id(n: u8) -> Uuid {
    let mut id = [0u8; 16];
    id[..6].copy_from_slice(&[0x01, 0x93, 0, 0, 0, n]);
    id[6] = 0x70;
    id[8] = 0x80;
    Uuid::from_bytes(id)
}

/// A tag edit about trace 1 (`span` zero) or about one of its spans.
fn tag(world: &World, span: u64, key: &str, version: u64, id: u8, retracted: bool) {
    let kind = if span == 0 {
        FactTargetKind::Trace
    } else {
        FactTargetKind::Span
    };
    let target = if span == 0 {
        SpanId::ZERO
    } else {
        span_id(span)
    };
    let set = vec![
        ("project_id", Value::Uuid(project(1).uuid())),
        ("key", Value::String(key.to_owned())),
        ("target_day", Value::Date(NOW.date())),
        ("target_time", Value::Time(NOW)),
        ("trace_id", bytes(trace(1).to_bytes())),
        ("span_id", bytes(target.to_bytes())),
        ("version", Value::UInt(version)),
        ("fact_id", Value::Uuid(fact_id(id))),
        ("target_kind", Value::Enum(kind.value())),
        ("retracted", Value::UInt(u64::from(retracted))),
        ("author_kind", Value::Enum(1)),
        ("created_at", Value::Time(NOW)),
        ("op", Value::Enum(1)),
    ];
    world
        .store
        .insert(TableId::FactTags, stored_row(TableId::FactTags, &set));
}

#[test]
fn each_span_carries_the_effective_facts_of_its_trace_and_its_own() {
    let world = World::new(16);
    for n in 1..=2 {
        store(
            &world,
            &Copy {
                project: 1,
                trace: 1,
                span: n,
                start: NOW,
                lsn: n,
                event_bytes: 1,
            },
        );
    }
    store(
        &world,
        &Copy {
            project: 1,
            trace: 2,
            span: 3,
            start: NOW,
            lsn: 3,
            event_bytes: 1,
        },
    );
    // The trace's tag, edited twice: the later version wins.
    tag(&world, 0, "reviewed", 1, 1, false);
    tag(&world, 0, "reviewed", 2, 2, false);
    // A span's tag, then retracted: no value.
    tag(&world, 2, "gone", 1, 3, false);
    tag(&world, 2, "gone", 2, 4, true);
    // Another span's tag.
    tag(&world, 1, "first", 1, 5, false);

    let page =
        run(world
            .service
            .export_page(&ctx(1), &query(received(9)), None, ExportLimits::default()))
        .unwrap();
    let facts: BTreeMap<u64, Vec<(FactTable, String, u64)>> = page
        .records
        .iter()
        .map(|record| {
            let id = u64::from_be_bytes(record.span.core.span_id.to_bytes());
            let facts = record
                .facts
                .iter()
                .map(|fact| {
                    let column = |name: &str| {
                        fact.columns
                            .iter()
                            .find(|c| c.name == name)
                            .unwrap()
                            .value
                            .clone()
                    };
                    let Value::String(key) = column("key") else {
                        panic!("key")
                    };
                    let Value::UInt(version) = column("version") else {
                        panic!("version")
                    };
                    (fact.table, key, version)
                })
                .collect();
            (id, facts)
        })
        .collect();
    assert_eq!(
        facts,
        BTreeMap::from([
            // The trace's facts come before the span's own (they resolve by table, target kind, span and key).
            (
                1,
                vec![
                    (FactTable::Tags, "reviewed".to_owned(), 2),
                    (FactTable::Tags, "first".to_owned(), 1)
                ]
            ),
            (2, vec![(FactTable::Tags, "reviewed".to_owned(), 2)]),
            (3, vec![]),
        ])
    );
}

#[test]
fn an_export_waits_while_acknowledged_spans_are_on_their_way_to_the_store() {
    let mut world = World::new(16);
    world.ack(vec![span(1, 1, 0, NOW)]);
    let q = query(NOW.saturating_add_micros(DAY));
    assert_eq!(
        world.service.export_wait(&ctx(1), &q),
        Some(ExportWait::Shipping)
    );
    // Another project's export does not wait for this one's spans.
    assert_eq!(world.service.export_wait(&crate::world::ctx(2), &q), None);
    world.settle();
    assert_eq!(world.service.export_wait(&ctx(1), &q), None);
}
