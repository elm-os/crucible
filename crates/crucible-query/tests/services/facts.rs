//! Facts (contract 3.8): latest-wins resolution in the Rust tier through `fold_facts`, the decoration of trace
//! detail and of the trace list, the facts of one target, and the bounds a target's facts lie in.

use std::sync::Arc;

use crucible_plan::Value;
use crucible_query::{Consistency, FactTarget, PartialReason, QueryServiceError};
use crucible_records::{
    FactTargetKind, Lsn, ScoreDataType, SpanId, TableId, TagOp, TimeWindow, Timestamp, TraceId,
    Uuid,
};

use crate::list::window;
use crate::store::stored_row;
use crate::world::{NOW, World, ctx, project, run, span, span_core_row, trace};

const SECOND: i64 = 1_000_000;

fn at(seconds: i64) -> Timestamp {
    NOW.saturating_add_micros(seconds * SECOND)
}

/// A UUIDv7 whose first half grows with `n` while its second half shrinks: byte order and the order of the
/// swapped halves (ClickHouse's) disagree on every pair.
fn fact_id(n: u8) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&[0x01, 0x93, 0, 0, 0, n]);
    bytes[6] = 0x70;
    bytes[8] = 0x80 | (0x3f - n.min(0x3f));
    bytes[15] = 0xff - n;
    Uuid::from_bytes(bytes)
}

struct Fact<'a> {
    table: TableId,
    kind: FactTargetKind,
    trace: TraceId,
    span: SpanId,
    session: &'a str,
    key: &'a str,
    version: u64,
    id: u8,
    retracted: bool,
    body: Vec<(&'a str, Value)>,
}

fn on_trace<'a>(table: TableId, key: &'a str, version: u64, id: u8) -> Fact<'a> {
    Fact {
        table,
        kind: FactTargetKind::Trace,
        trace: trace(1),
        span: SpanId::ZERO,
        session: "",
        key,
        version,
        id,
        retracted: false,
        body: Vec::new(),
    }
}

fn put(world: &World, fact: Fact<'_>) {
    let mut set = vec![
        ("project_id", Value::Uuid(project(1).uuid())),
        ("key", Value::String(fact.key.to_owned())),
        ("target_day", Value::Date(at(-60).date())),
        ("target_time", Value::Time(at(-60))),
        ("trace_id", Value::Bytes(fact.trace.to_bytes().to_vec())),
        ("span_id", Value::Bytes(fact.span.to_bytes().to_vec())),
        ("session_id", Value::String(fact.session.to_owned())),
        ("version", Value::UInt(fact.version)),
        ("fact_id", Value::Uuid(fact_id(fact.id))),
        ("target_kind", Value::Enum(fact.kind.value())),
        ("retracted", Value::UInt(u64::from(fact.retracted))),
        ("author_kind", Value::Enum(2)),
        ("created_at", Value::Time(at(-30))),
    ];
    set.extend(fact.body);
    world.store.insert(fact.table, stored_row(fact.table, &set));
}

fn text(value: &str) -> Value {
    Value::String(value.to_owned())
}

/// Trace 1 in the store, whose root was sent with the tags `ingested` and `stale`, and its facts.
fn world_with_facts() -> World {
    let mut world = World::new(100);
    // The root's row as the shipper writes it, with its trace tags; it lands before the rows `settle` ships,
    // so the store returns it first.
    let mut root = span_core_row(&span(1, 10, 0, at(-60)), Lsn::new(1), NOW);
    let tags = crucible_schema::table(TableId::SpanCore)
        .columns
        .iter()
        .position(|c| c.name == "trace_tags")
        .unwrap();
    root[tags] = Value::Array(vec![text("ingested"), text("stale")]);
    world.store.insert(TableId::SpanCore, root);
    world.ack(vec![span(1, 10, 0, at(-60)), span(1, 11, 10, at(-59))]);
    world.settle();

    let op = |op: TagOp| vec![("op", Value::Enum(op.value()))];
    // `urgent` was added, then removed; `new` added; the ingest tag `stale` removed. Of two edits of `pair`
    // with one version, the id that is greater in byte order wins although ClickHouse orders it first.
    for (key, version, id, edit) in [
        ("urgent", 1, 1, TagOp::Add),
        ("urgent", 2, 2, TagOp::Remove),
        ("new", 3, 3, TagOp::Add),
        ("stale", 4, 4, TagOp::Remove),
        ("pair", 5, 6, TagOp::Add),
        ("pair", 5, 5, TagOp::Remove),
    ] {
        put(
            &world,
            Fact {
                body: op(edit),
                ..on_trace(TableId::FactTags, key, version, id)
            },
        );
    }
    let flag = |value: bool| vec![("value", Value::UInt(u64::from(value)))];
    put(
        &world,
        Fact {
            body: flag(false),
            ..on_trace(TableId::FactFlags, "bookmarked", 1, 7)
        },
    );
    put(
        &world,
        Fact {
            body: flag(true),
            ..on_trace(TableId::FactFlags, "bookmarked", 2, 8)
        },
    );
    let score = |value: f64, comment: &str| {
        vec![
            ("data_type", Value::Enum(ScoreDataType::Numeric.value())),
            ("value_num", Value::Float(value)),
            ("comment", text(comment)),
            ("source", Value::Enum(1)),
            ("metadata", text("{\"k\":1}")),
        ]
    };
    put(
        &world,
        Fact {
            body: score(1.0, "great"),
            ..on_trace(TableId::FactScores, "helpful#a", 1, 9)
        },
    );
    put(
        &world,
        Fact {
            body: score(0.0, "meh"),
            ..on_trace(TableId::FactScores, "helpful#b", 1, 10)
        },
    );
    // A score that was retracted has no value.
    put(
        &world,
        Fact {
            body: score(0.5, ""),
            ..on_trace(TableId::FactScores, "quality", 1, 11)
        },
    );
    put(
        &world,
        Fact {
            retracted: true,
            body: score(0.0, ""),
            ..on_trace(TableId::FactScores, "quality", 2, 12)
        },
    );
    let correction = vec![("corrected", text("\"fixed\"")), ("note", text("typo"))];
    put(
        &world,
        Fact {
            body: correction,
            ..on_trace(TableId::FactCorrections, "output", 1, 13)
        },
    );
    world
}

fn names(tags: &[Arc<str>]) -> Vec<&str> {
    tags.iter().map(|t| &**t).collect()
}

#[test]
fn trace_detail_and_the_list_carry_the_latest_facts() {
    let world = world_with_facts();
    let detail = run(world.service.trace_by_id(&ctx(1), trace(1))).unwrap();
    let facts = &detail.facts;
    assert_eq!(names(&facts.tags), ["ingested", "new", "pair"]);
    let tag_ops: Vec<(&str, TagOp)> = facts
        .facts
        .tags
        .iter()
        .map(|edit| (&*edit.tag, edit.op))
        .collect();
    assert_eq!(
        tag_ops,
        [
            ("new", TagOp::Add),
            ("pair", TagOp::Add),
            ("stale", TagOp::Remove),
            ("urgent", TagOp::Remove)
        ]
    );
    let pair = facts.facts.tags.iter().find(|e| &*e.tag == "pair").unwrap();
    assert_eq!(pair.resolved.fact_id.uuid(), fact_id(6));
    assert!(facts.facts.bookmarked.unwrap().value);
    let scores: Vec<(&str, Option<&str>, f64, &str)> = facts
        .facts
        .scores
        .iter()
        .map(|s| (&*s.name, s.score_id.as_deref(), s.value_num, &*s.comment))
        .collect();
    assert_eq!(
        scores,
        [
            ("helpful", Some("a"), 1.0, "great"),
            ("helpful", Some("b"), 0.0, "meh")
        ]
    );
    assert_eq!(&*facts.facts.scores[0].metadata, "{\"k\":1}");
    assert_eq!(
        &*facts.facts.correction.as_ref().unwrap().corrected,
        "\"fixed\""
    );

    // The list reads the same facts without comments, metadata or the correction.
    let query = crucible_query::TraceListQuery {
        window: window(),
        filter: crucible_plan::Pred::all(),
        limit: Some(10),
        cursor: None,
    };
    let page = run(world.service.trace_list(&ctx(1), query)).unwrap();
    let item = &page.data[0];
    assert!(item.facts.facts.bookmarked.unwrap().value);
    assert_eq!(item.facts.facts.scores.len(), 2);
    assert!(
        item.facts
            .facts
            .scores
            .iter()
            .all(|s| s.comment.is_empty() && s.metadata.is_empty())
    );
    assert!(item.facts.facts.correction.is_none());
    // The list's heads carry no ingest tags here, so its effective tags are the added ones.
    assert_eq!(names(&item.facts.tags), ["new", "pair"]);
    // Another project sees none of them.
    let other = run(world.service.trace_by_id(&ctx(2), trace(1)));
    assert!(matches!(other, Err(QueryServiceError::NotFound)));
}

#[test]
fn the_facts_of_one_target_lie_in_its_bounds() {
    let world = world_with_facts();
    let bounds = run(world
        .service
        .fact_target_bounds(&ctx(1), &FactTarget::Trace(trace(1))))
    .unwrap();
    assert_eq!(bounds.start, Some(at(-60)));
    let window = bounds.window.unwrap();
    assert!(window.contains(at(-60)) && window.contains(at(-59)));
    let facts = run(world
        .service
        .facts(&ctx(1), &FactTarget::Trace(trace(1)), window))
    .unwrap();
    assert_eq!(facts.facts.scores.len(), 2);
    assert!(facts.meta.partial_reason.is_none());

    // A span's and a session's facts are their own.
    let span_target = FactTarget::Span(trace(1), crate::world::span_id(11));
    put(
        &world,
        Fact {
            kind: FactTargetKind::Span,
            span: crate::world::span_id(11),
            body: vec![("value", Value::UInt(1))],
            ..on_trace(TableId::FactFlags, "bookmarked", 1, 20)
        },
    );
    let facts = run(world.service.facts(&ctx(1), &span_target, window)).unwrap();
    assert!(facts.facts.bookmarked.unwrap().value && facts.facts.scores.is_empty());
    let session = FactTarget::Session(Arc::from("s1"));
    put(
        &world,
        Fact {
            kind: FactTargetKind::Session,
            trace: TraceId::ZERO,
            session: "s1",
            body: vec![("op", Value::Enum(TagOp::Add.value()))],
            ..on_trace(TableId::FactTags, "reviewed", 1, 21)
        },
    );
    let facts = run(world.service.facts(&ctx(1), &session, window)).unwrap();
    assert_eq!(&*facts.facts.tags[0].tag, "reviewed");
    // A session is looked up in its rollup: one that is not stored is not known.
    let unknown = run(world.service.fact_target_bounds(&ctx(1), &session)).unwrap();
    assert_eq!((unknown.start, unknown.window), (None, None));
    world.store.insert(
        TableId::SessionRollup,
        stored_row(
            TableId::SessionRollup,
            &[
                ("project_id", Value::Uuid(project(1).uuid())),
                ("day", Value::Date(at(-60).date())),
                ("session_id", text("s1")),
                ("first_seen", Value::Time(at(-60))),
                ("last_seen", Value::Time(at(-10))),
            ],
        ),
    );
    let known = run(world.service.fact_target_bounds(&ctx(1), &session)).unwrap();
    assert_eq!(known.start, Some(at(-60)));
}

#[test]
fn an_unknown_target_is_known_absent_only_when_the_lookup_is_whole() {
    let mut world = World::new(100);
    let unknown = FactTarget::Trace(trace(9));
    let bounds = run(world.service.fact_target_bounds(&ctx(1), &unknown)).unwrap();
    assert_eq!(bounds.start, None);
    assert!(!bounds.meta.partial && bounds.meta.consistency == Consistency::Strong);

    // While the store is down a trace in the recent index is still known, and the answer says partial.
    world.ack(vec![span(2, 20, 0, at(-5))]);
    world.store.set_unavailable(true);
    let bounds = run(world
        .service
        .fact_target_bounds(&ctx(1), &FactTarget::Trace(trace(2))))
    .unwrap();
    assert_eq!(bounds.start, Some(at(-5)));
    assert_eq!(
        bounds.meta.partial_reason,
        Some(PartialReason::StoreUnavailable)
    );
    // A trace neither source knows cannot be told apart from one the store would know.
    let down = run(world.service.fact_target_bounds(&ctx(1), &unknown));
    assert!(matches!(down, Err(QueryServiceError::Store(_))));
    let window = TimeWindow::new(at(-60), at(60)).unwrap();
    let facts = run(world.service.facts(&ctx(1), &unknown, window));
    assert!(matches!(facts, Err(QueryServiceError::Store(_))));
}
