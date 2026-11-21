//! Project deletion over `PgModel` and the simulated object store (contracts 12.6 and 13.4): deleting a project
//! removes its dashboards with their widgets, its media rows and every object under its key prefix, and leaves
//! every other project's alone.

use axum::http::Method;
use bytes::Bytes;
use crucible_control::{MediaObject, MediaRef, MediaState, Project, Repos as _};
use crucible_media::rules::object_key;
use crucible_records::{DashboardId, MediaObjectId, ProjectId, TraceId, Uuid};
use crucible_sim::object_model::ObjectFaults;
use serde_json::json;

use crate::identity::{Platform, call, platform_with};
use crate::run;
use crate::world::{NOW, World, project};

/// A dashboard of `project` with one widget, made through the API; returns its id.
async fn new_dashboard(p: &Platform, project: ProjectId) -> String {
    let body = json!({
        "name": "Agents",
        "widgets": [{
            "query": {"source": "spans", "measures": [{"measure": "count", "aggregation": "sum"}]},
            "display": {"kind": "stat", "title": "Spans"},
            "layout": {"x": 0, "y": 0, "w": 3, "h": 2},
        }],
    });
    let path = format!("/api/v1/projects/{project}/dashboards");
    let created = call(&p.router, Method::POST, &path, &p.owner, Some(body), &[]).await;
    assert_eq!(created.status.as_u16(), 201, "{}", created.pretty());
    created.body["id"].as_str().unwrap().to_owned()
}

/// An uploaded media object of `project` with one use in `trace`, its row, its use and its stored bytes; returns
/// its id and key.
async fn new_media(
    p: &Platform,
    project: ProjectId,
    n: u8,
    trace: TraceId,
) -> (MediaObjectId, String) {
    let id = MediaObjectId::from_uuid(Uuid::from_v7_parts(1_795_046_400_000, u128::from(n)));
    let object = MediaObject {
        id,
        project_id: project,
        sha256: [n; 32],
        content_type: "image/png".to_owned(),
        len: 4,
        state: MediaState::Uploaded,
        created_at: NOW,
        uploaded_at: Some(NOW),
    };
    p.model.media().insert_object(&object).await.unwrap();
    let reference = MediaRef {
        project_id: project,
        media_id: id,
        trace_id: trace,
        span_id: None,
        field: "input".to_owned(),
        target_time: NOW,
    };
    assert!(p.model.media().add_ref(&reference).await.unwrap());
    let key = object_key("media", project, id);
    p.objects.plant(&key, Bytes::from_static(b"\x89PNG"), NOW);
    (id, key)
}

fn under(p: &Platform, project: ProjectId) -> Vec<String> {
    let prefix = format!("media/{project}/");
    p.objects
        .objects()
        .into_keys()
        .filter(|key| key.starts_with(&prefix))
        .collect()
}

/// Project 1 with a dashboard and two media objects (one of them with no row, as an upload a deletion may race
/// leaves), beside project 2 with the same.
struct Fixture {
    other: Project,
    kept_dashboard: String,
    dashboard: String,
    trace: TraceId,
    media: MediaObjectId,
    kept_media: MediaObjectId,
    kept_key: String,
}

async fn fixture(p: &Platform) -> Fixture {
    let other = p.identity.create_project("Two", "two").await.unwrap();
    let dashboard = new_dashboard(p, project(1)).await;
    let kept_dashboard = new_dashboard(p, other.id).await;
    let trace = TraceId::from_bytes([7; 16]);
    let (media, _) = new_media(p, project(1), 1, trace).await;
    let stray = object_key(
        "media",
        project(1),
        MediaObjectId::from_uuid(Uuid::from_v7_parts(1_795_046_400_000, 9)),
    );
    p.objects.plant(&stray, Bytes::from_static(b"left"), NOW);
    let (kept_media, kept_key) = new_media(p, other.id, 2, trace).await;
    assert_eq!(under(p, project(1)).len(), 2);
    Fixture {
        other,
        kept_dashboard,
        dashboard,
        trace,
        media,
        kept_media,
        kept_key,
    }
}

async fn delete_project(p: &Platform) -> u16 {
    let path = format!("/api/v1/projects/{}", project(1));
    let deleted = call(
        &p.router,
        Method::DELETE,
        &path,
        &p.owner,
        None,
        &[("if-match", "\"1\"")],
    )
    .await;
    assert!(deleted.body.is_null(), "{}", deleted.pretty());
    deleted.status.as_u16()
}

/// What the deletion leaves of the other project: everything.
async fn other_project_is_whole(p: &Platform, f: &Fixture) {
    let kept = p.model.dashboards().list(f.other.id).await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].dashboard.id.to_string(), f.kept_dashboard);
    assert_eq!(kept[0].widgets, 1);
    let object = p
        .model
        .media()
        .object(f.other.id, f.kept_media)
        .await
        .unwrap();
    assert!(object.is_some());
    let uses = p
        .model
        .media()
        .uses(f.other.id, f.trace, None)
        .await
        .unwrap();
    assert_eq!(uses.len(), 1);
    assert_eq!(under(p, f.other.id), std::slice::from_ref(&f.kept_key));
}

#[test]
fn deleting_a_project_removes_its_dashboards_media_rows_and_objects() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let f = fixture(&p).await;
        // More objects than one listing page holds.
        for n in 0..1_100u32 {
            let id = MediaObjectId::from_uuid(Uuid::from_v7_parts(
                1_795_046_400_000,
                1_000 + u128::from(n),
            ));
            let key = object_key("media", project(1), id);
            p.objects.plant(&key, Bytes::from_static(b"page"), NOW);
        }

        assert_eq!(delete_project(&p).await, 204);

        assert!(
            p.model
                .dashboards()
                .list(project(1))
                .await
                .unwrap()
                .is_empty()
        );
        let id = DashboardId::parse(&f.dashboard).unwrap();
        let gone = p.model.dashboards().get(project(1), id).await.unwrap();
        assert!(gone.is_none());
        let object = p.model.media().object(project(1), f.media).await.unwrap();
        assert!(object.is_none());
        let uses = p
            .model
            .media()
            .uses(project(1), f.trace, None)
            .await
            .unwrap();
        assert!(uses.is_empty());
        assert_eq!(under(&p, project(1)), Vec::<String>::new());
        other_project_is_whole(&p, &f).await;

        // The project is gone for its members too.
        let path = format!("/api/v1/projects/{}", project(1));
        let read = call(&p.router, Method::GET, &path, &p.owner, None, &[]).await;
        read.assert_problem(404, "not_found");
    });
}

/// An object store that does not answer leaves the objects to the media collector's orphan scan: the project is
/// deleted, and so are its rows, which the scan needs gone before it deletes an object.
#[test]
fn a_store_that_does_not_answer_leaves_the_objects_to_the_orphan_scan() {
    run(|env| async move {
        let world = World::new(10);
        let p = platform_with(&env, &world, false).await;
        let f = fixture(&p).await;
        p.objects.set_faults(ObjectFaults {
            throttle_ppm: 1_000_000,
            ..ObjectFaults::default()
        });

        assert_eq!(delete_project(&p).await, 204);

        assert!(
            p.model
                .dashboards()
                .list(project(1))
                .await
                .unwrap()
                .is_empty()
        );
        let object = p.model.media().object(project(1), f.media).await.unwrap();
        assert!(object.is_none());
        assert_eq!(under(&p, project(1)).len(), 2);
        p.objects.set_faults(ObjectFaults::default());
        other_project_is_whole(&p, &f).await;
    });
}
