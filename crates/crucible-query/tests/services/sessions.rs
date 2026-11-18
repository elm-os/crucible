//! Sessions and users: the rollups merged over the window's days, their paging, the sessions of a user, and
//! their `meta` when the store is down, stage B lags or retention expires them.

use std::sync::Arc;

use crucible_plan::Caller;
use crucible_query::{
    Consistency, IdCursor, QueryServiceError, ReadContext, RetentionSettings, SessionListPage,
    SessionListQuery, UserListPage, UserListQuery, UserSessionsQuery,
};
use crucible_recent::SpanEntry;
use crucible_records::{Level, TimeWindow, Timestamp};

use crate::world::{DAY, NOW, World, ctx, run, scoped, span};

const MS: i64 = 1_000;

fn at(ms: i64) -> Timestamp {
    NOW.saturating_add_micros(ms * MS)
}

/// The day that holds [`NOW`], plus a day either side.
fn window() -> TimeWindow {
    TimeWindow::new(
        NOW.saturating_add_micros(-DAY),
        NOW.saturating_add_micros(DAY),
    )
    .unwrap()
}

/// A span of `session` and `user` (empty for none).
fn of(mut entry: SpanEntry, session: &str, user: &str) -> SpanEntry {
    entry.summary.session_id = Arc::from(session);
    entry.summary.user_id = Arc::from(user);
    entry
}

fn sessions(world: &World, limit: u32, cursor: Option<IdCursor>) -> SessionListPage {
    let query = SessionListQuery {
        window: window(),
        limit: Some(limit),
        cursor,
    };
    run(world.service.session_list(&ctx(1), query)).unwrap()
}

fn users(world: &World, limit: u32, cursor: Option<IdCursor>) -> UserListPage {
    let query = UserListQuery {
        window: window(),
        limit: Some(limit),
        cursor,
    };
    run(world.service.user_list(&ctx(1), query)).unwrap()
}

/// Three sessions: s1 (two traces of ada, one with an error), s2 (one trace of bob), s3 (one trace of ada).
fn three_sessions() -> World {
    let mut world = World::new(100);
    let mut failed = of(span(2, 21, 20, at(-50)), "s1", "ada");
    failed.summary.level = Level::Error;
    world.ack(vec![
        of(span(1, 10, 0, at(-60)), "s1", "ada"),
        of(span(1, 11, 10, at(-59)), "s1", "ada"),
    ]);
    world.ack(vec![of(span(2, 20, 0, at(-51)), "s1", "ada"), failed]);
    world.ack(vec![of(span(3, 30, 0, at(-40)), "s2", "bob")]);
    world.ack(vec![of(span(4, 40, 0, at(-30)), "s3", "ada")]);
    world.settle();
    world
}

#[test]
fn sessions_merge_their_partials_and_page_by_last_seen() {
    let world = three_sessions();
    let first = sessions(&world, 2, None);
    let ids: Vec<&str> = first.data.iter().map(|s| &*s.session_id).collect();
    assert_eq!(ids, ["s3", "s2"]);
    assert!(first.has_more);
    assert_eq!(first.meta.consistency, Consistency::Strong);
    assert!(!first.meta.partial);
    let second = sessions(&world, 2, first.next_cursor.clone());
    let ids: Vec<&str> = second.data.iter().map(|s| &*s.session_id).collect();
    assert_eq!(ids, ["s1"]);
    assert!(!second.has_more && second.next_cursor.is_none());

    // s1 merges two frames: two root spans, four spans, one error, both of ada.
    let s1 = &second.data[0];
    assert_eq!((s1.trace_count, s1.span_count, s1.error_count), (2, 4, 1));
    assert_eq!((s1.first_seen, s1.last_seen), (at(-60), at(-49)));
    assert_eq!((s1.total_cost_nusd.nanos(), s1.total_tokens), (28, 12));
    let users: Vec<&str> = s1.user_ids.iter().map(|u| &**u).collect();
    assert_eq!(users, ["ada"]);

    // One session by id, and one that has no span in the window.
    let detail = run(world.service.session(&ctx(1), window(), "s1")).unwrap();
    assert_eq!(detail.session.as_ref(), Some(s1));
    let missing = run(world.service.session(&ctx(1), window(), "s9"));
    assert_eq!(missing, Err(QueryServiceError::NotFound));
    // Another project sees none of them.
    let other = run(world.service.session(&ctx(2), window(), "s1"));
    assert_eq!(other, Err(QueryServiceError::NotFound));
}

#[test]
fn a_cursor_is_for_its_own_project_and_window() {
    let world = three_sessions();
    let first = sessions(&world, 1, None);
    let cursor = first.next_cursor.unwrap();
    let moved = SessionListQuery {
        window: TimeWindow::new(NOW.saturating_add_micros(-DAY), NOW).unwrap(),
        limit: Some(1),
        cursor: Some(cursor.clone()),
    };
    let refused = run(world.service.session_list(&ctx(1), moved));
    assert_eq!(refused, Err(QueryServiceError::CursorInvalid));
    let elsewhere = SessionListQuery {
        window: window(),
        limit: Some(1),
        cursor: Some(cursor),
    };
    let refused = run(world.service.session_list(&ctx(2), elsewhere));
    assert_eq!(refused, Err(QueryServiceError::CursorInvalid));
}

#[test]
fn spans_the_store_may_not_hold_yet_make_rollup_reads_lagging() {
    let mut world = three_sessions();
    world.ack(vec![of(span(5, 50, 0, at(-10)), "s4", "cy")]);
    // The rollups do not hold s4 yet, so the answers say they may trail.
    let listed = sessions(&world, 10, None);
    assert_eq!(listed.data.len(), 3);
    assert_eq!(listed.meta.consistency, Consistency::Lagging);
    assert_eq!(
        users(&world, 10, None).meta.consistency,
        Consistency::Lagging
    );
    let pending = run(world.service.session(&ctx(1), window(), "s4")).unwrap();
    assert_eq!(pending.session, None);
    assert_eq!(pending.meta.consistency, Consistency::Lagging);
    // A session the pending spans do not touch is whole.
    let settled = run(world.service.session(&ctx(1), window(), "s2")).unwrap();
    assert_eq!(settled.meta.consistency, Consistency::Strong);

    world.settle();
    let listed = sessions(&world, 10, None);
    assert_eq!(listed.data.len(), 4);
    assert_eq!(listed.meta.consistency, Consistency::Strong);
    let found = run(world.service.session(&ctx(1), window(), "s4")).unwrap();
    assert_eq!(found.session.map(|s| s.span_count), Some(1));
}

#[test]
fn a_store_outage_fails_rollup_reads_that_the_index_cannot_answer() {
    let world = three_sessions();
    world.store.set_unavailable(true);
    let down = |result: Result<(), QueryServiceError>| {
        assert!(
            matches!(result, Err(QueryServiceError::Store(_))),
            "{result:?}"
        );
    };
    let query = SessionListQuery {
        window: window(),
        limit: None,
        cursor: None,
    };
    down(run(world.service.session_list(&ctx(1), query)).map(|_| ()));
    down(run(world.service.session(&ctx(1), window(), "s1")).map(|_| ()));
    let query = UserListQuery {
        window: window(),
        limit: None,
        cursor: None,
    };
    down(run(world.service.user_list(&ctx(1), query)).map(|_| ()));
    down(run(world.service.user(&ctx(1), window(), "ada")).map(|_| ()));
}

#[test]
fn retention_hides_expired_sessions_and_users() {
    let mut world = World::new(100);
    let old = NOW.saturating_add_micros(-3 * DAY);
    world.ack(vec![of(span(1, 10, 0, old), "s-old", "old")]);
    world.ack(vec![of(span(2, 20, 0, at(-10)), "s-new", "new")]);
    world.settle();
    let wide = TimeWindow::new(NOW.saturating_add_micros(-5 * DAY), NOW).unwrap();
    let keep_one_day = ReadContext::new(
        scoped(1),
        RetentionSettings {
            retention_days: Some(1),
            ..RetentionSettings::default()
        },
        Caller::Session,
    );
    for (ctx, expected) in [(ctx(1), 2), (keep_one_day, 1)] {
        let query = SessionListQuery {
            window: wide,
            limit: None,
            cursor: None,
        };
        let listed = run(world.service.session_list(&ctx, query)).unwrap();
        assert_eq!(listed.data.len(), expected);
        let query = UserListQuery {
            window: wide,
            limit: None,
            cursor: None,
        };
        assert_eq!(
            run(world.service.user_list(&ctx, query))
                .unwrap()
                .data
                .len(),
            expected
        );
    }
    let expired = run(world.service.session(&keep_one_day, wide, "s-old"));
    assert_eq!(expired, Err(QueryServiceError::NotFound));
}

#[test]
fn users_merge_their_partials_with_their_distinct_session_counts() {
    let world = three_sessions();
    let first = users(&world, 1, None);
    let ids: Vec<&str> = first.data.iter().map(|u| &*u.user_id).collect();
    assert_eq!(ids, ["ada"]);
    let ada = &first.data[0];
    // Three root spans in s1 and s3, five spans, one error; two sessions among its trace heads.
    assert_eq!(
        (
            ada.trace_count,
            ada.span_count,
            ada.error_count,
            ada.session_count
        ),
        (3, 5, 1, 2)
    );
    assert_eq!((ada.first_seen, ada.last_seen), (at(-60), at(-29)));
    let second = users(&world, 1, first.next_cursor.clone());
    let ids: Vec<&str> = second.data.iter().map(|u| &*u.user_id).collect();
    assert_eq!(ids, ["bob"]);
    assert_eq!(second.data[0].session_count, 1);
    assert!(!second.has_more);

    let one = run(world.service.user(&ctx(1), window(), "ada")).unwrap();
    assert_eq!(one.user.as_ref(), Some(ada));
    assert_eq!(
        run(world.service.user(&ctx(1), window(), "zed")),
        Err(QueryServiceError::NotFound)
    );
}

#[test]
fn the_sessions_of_a_user_come_from_its_trace_heads_latest_first() {
    let mut world = three_sessions();
    let query = |cursor| UserSessionsQuery {
        window: window(),
        user_id: "ada".to_owned(),
        limit: Some(1),
        cursor,
    };
    let first = run(world.service.user_sessions(&ctx(1), query(None))).unwrap();
    let ids: Vec<&str> = first.data.iter().map(|s| &*s.session_id).collect();
    assert_eq!(ids, ["s3"]);
    assert!(first.has_more);
    assert_eq!(first.meta.consistency, Consistency::Strong);
    let second = run(world
        .service
        .user_sessions(&ctx(1), query(first.next_cursor)))
    .unwrap();
    let s1 = &second.data[0];
    assert_eq!(&*s1.session_id, "s1");
    assert_eq!(
        (s1.trace_count, s1.first_trace_at, s1.last_trace_at),
        (2, at(-60), at(-51))
    );
    assert!(!second.has_more);

    // A head of the user that the store may not hold yet makes the answer lagging; other users are whole.
    world.ack(vec![of(span(6, 60, 0, at(-5)), "s5", "ada")]);
    let pending = run(world.service.user_sessions(&ctx(1), query(None))).unwrap();
    assert_eq!(pending.meta.consistency, Consistency::Lagging);
    let bob = UserSessionsQuery {
        user_id: "bob".to_owned(),
        ..query(None)
    };
    let whole = run(world.service.user_sessions(&ctx(1), bob)).unwrap();
    assert_eq!(whole.meta.consistency, Consistency::Strong);
}
