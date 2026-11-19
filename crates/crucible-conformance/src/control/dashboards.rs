//! Dashboards and their widgets (contract 11.1): one statement per write, the dashboard's version covering its
//! widgets, widget sets rewritten in place, and widgets going with their dashboard.

use anyhow::{Context as _, Result};
use crucible_control::{Dashboard, JsonDoc, Widget};
use crucible_records::{DashboardId, ProjectId, WidgetId};

use super::{Ctl, Transcript, at, id, project};

fn doc(text: &str) -> Result<JsonDoc> {
    JsonDoc::parse(text).context("the fixture document")
}

fn dashboard(n: u64, project_id: ProjectId) -> Dashboard {
    Dashboard {
        id: DashboardId::from_uuid(id(n)),
        project_id,
        name: format!("Dashboard {n}"),
        description: String::new(),
        version: 1,
        created_at: at(0),
        updated_at: at(0),
    }
}

fn widget(n: u64, x: u32) -> Result<Widget> {
    Ok(Widget {
        id: WidgetId::from_uuid(id(n)),
        query: Some(doc(
            r#"{"source":"spans","measures":[{"measure":"count","aggregation":"sum"}]}"#,
        )?),
        display: doc(r#"{"kind":"timeseries","title":"Spans"}"#)?,
        layout: doc(&format!(r#"{{"x":{x},"y":0,"w":6,"h":3}}"#))?,
    })
}

fn text(n: u64) -> Result<Widget> {
    Ok(Widget {
        id: WidgetId::from_uuid(id(n)),
        query: None,
        display: doc(r#"{"kind":"markdown","title":"Notes","text":"Read me"}"#)?,
        layout: doc(r#"{"x":0,"y":3,"w":12,"h":1}"#)?,
    })
}

pub(super) async fn run(ctl: &Ctl, t: &mut Transcript) -> Result<()> {
    let s = ctl.store.as_ref();
    let repo = s.dashboards();
    let (one, two) = (project(1), project(2));
    s.projects().insert(&one).await?;
    s.projects().insert(&two).await?;

    let first = dashboard(100, one.id);
    let (a, b) = (widget(110, 0)?, widget(111, 6)?);
    t.outcome(
        "insert with two widgets",
        repo.insert(&first, &[a.clone(), b.clone()]).await,
    );
    t.outcome(
        "insert without widgets",
        repo.insert(&dashboard(101, one.id), &[]).await,
    );
    t.outcome(
        "insert in the other project",
        repo.insert(&dashboard(102, two.id), &[widget(112, 0)?])
            .await,
    );
    t.outcome("insert the same id", repo.insert(&first, &[]).await);
    t.outcome(
        "insert for an unknown project",
        repo.insert(&dashboard(103, ProjectId::from_uuid(id(999))), &[])
            .await,
    );
    t.outcome("get", repo.get(one.id, first.id).await);
    t.outcome(
        "get through the other project",
        repo.get(two.id, first.id).await,
    );
    t.outcome(
        "get an unknown dashboard",
        repo.get(one.id, DashboardId::from_uuid(id(199))).await,
    );
    t.outcome("list a project", repo.list(one.id).await);
    t.outcome("list the other project", repo.list(two.id).await);

    // One write renames the dashboard, rewrites one widget, drops another and adds a third.
    let mut renamed = first.clone();
    renamed.name = "Overview".to_owned();
    renamed.description = "What the agents did".to_owned();
    renamed.updated_at = at(60);
    let mut moved = a.clone();
    moved.layout = doc(r#"{"x":0,"y":4,"w":12,"h":2}"#)?;
    let added = text(113)?;
    t.outcome(
        "update under a stale version",
        repo.update_if_version(&renamed, &[moved.clone()], 7).await,
    );
    t.outcome(
        "update through the other project",
        repo.update_if_version(
            &Dashboard {
                project_id: two.id,
                ..renamed.clone()
            },
            &[],
            1,
        )
        .await,
    );
    t.outcome(
        "update",
        repo.update_if_version(&renamed, &[moved.clone(), added.clone()], 1)
            .await,
    );
    t.outcome("get after the update", repo.get(one.id, first.id).await);
    t.outcome(
        "update the same version again",
        repo.update_if_version(&renamed, &[], 1).await,
    );
    // The stored creation time is kept, whatever the write carries.
    let mut emptied = renamed.clone();
    emptied.created_at = at(999);
    emptied.updated_at = at(120);
    t.outcome(
        "update to no widgets",
        repo.update_if_version(&emptied, &[], 2).await,
    );
    t.outcome("get after emptying", repo.get(one.id, first.id).await);
    t.outcome(
        "the widgets of the other project are untouched",
        repo.get(two.id, DashboardId::from_uuid(id(102))).await,
    );

    t.outcome(
        "delete under a stale version",
        repo.delete_if_version(one.id, first.id, 2).await,
    );
    t.outcome(
        "delete through the other project",
        repo.delete_if_version(two.id, first.id, 3).await,
    );
    t.outcome(
        "delete",
        repo.delete_if_version(one.id, first.id, 3).await,
    );
    t.outcome(
        "delete again",
        repo.delete_if_version(one.id, first.id, 3).await,
    );
    t.outcome("list after the delete", repo.list(one.id).await);
    // A widget id is unique within its dashboard only: another dashboard may carry the same one.
    t.outcome(
        "insert with a widget id another dashboard has",
        repo.insert(&dashboard(104, one.id), &[a, widget(112, 6)?])
            .await,
    );
    t.outcome(
        "get the new dashboard",
        repo.get(one.id, DashboardId::from_uuid(id(104))).await,
    );
    Ok(())
}
