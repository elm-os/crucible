//! The dashboard repository of the model (migration V9): dashboards, and their widgets keyed by dashboard and
//! widget id, which go with their dashboard.

use async_trait::async_trait;
use crucible_control::{Dashboard, DashboardEntry, DashboardRepo, DashboardRows, TxError, Widget};
use crucible_records::{DashboardId, ProjectId};

use super::{Key, Pool, Row, Runner, Tbl, uuid_key};

/// A stored widget with the dashboard the store files it under.
#[derive(Clone, Debug)]
pub(super) struct WidgetRow {
    pub(super) dashboard_id: DashboardId,
    pub(super) widget: Widget,
}

fn widget_key(dashboard_id: DashboardId, widget: &Widget) -> Key {
    [dashboard_id.to_bytes(), widget.id.to_bytes()].concat()
}

/// The stored dashboard of the project with this id.
fn find(cx: &super::Cx<'_>, project_id: ProjectId, id: DashboardId) -> Option<Dashboard> {
    one!(
        cx,
        Tbl::Dashboards,
        &uuid_key(id.to_bytes()),
        Row::Dashboard
    )
    .filter(|d| d.project_id == project_id)
}

/// The widgets of a dashboard, by key: ordered by widget id.
fn widgets_of(cx: &super::Cx<'_>, id: DashboardId) -> Vec<(Key, Widget)> {
    cx.scan(Tbl::Widgets)
        .into_iter()
        .filter_map(|(key, row)| match row {
            Row::Widget(w) if w.dashboard_id == id => Some((key, w.widget)),
            _ => None,
        })
        .collect()
}

fn put_widgets(
    cx: &mut super::Cx<'_>,
    dashboard: &Dashboard,
    widgets: &[Widget],
) -> super::Step<()> {
    for widget in widgets {
        let row = WidgetRow {
            dashboard_id: dashboard.id,
            widget: widget.clone(),
        };
        cx.put(
            Tbl::Widgets,
            widget_key(dashboard.id, widget),
            Some(Row::Widget(row)),
        )?;
    }
    Ok(())
}

#[async_trait]
impl DashboardRepo for Runner {
    async fn get(
        &self,
        project_id: ProjectId,
        id: DashboardId,
    ) -> Result<Option<DashboardRows>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(find(cx, project_id, id).map(|dashboard| DashboardRows {
                widgets: widgets_of(cx, dashboard.id)
                    .into_iter()
                    .map(|(_, w)| w)
                    .collect(),
                dashboard,
            }))
        })
        .await
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<DashboardEntry>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut dashboards = all!(cx, Tbl::Dashboards, Row::Dashboard);
            dashboards.retain(|d| d.project_id == project_id);
            Ok(dashboards
                .into_iter()
                .map(|dashboard| DashboardEntry {
                    widgets: u32::try_from(widgets_of(cx, dashboard.id).len()).unwrap_or(u32::MAX),
                    dashboard,
                })
                .collect())
        })
        .await
    }

    async fn insert(&self, dashboard: &Dashboard, widgets: &[Widget]) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Dashboards,
                uuid_key(dashboard.id.to_bytes()),
                Row::Dashboard(dashboard.clone()),
                "dashboards_pkey",
            )?;
            put_widgets(cx, dashboard, widgets)
        })
        .await
    }

    async fn update_if_version(
        &self,
        dashboard: &Dashboard,
        widgets: &[Widget],
        expected: u64,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let Some(stored) = find(cx, dashboard.project_id, dashboard.id) else {
                return Ok(false);
            };
            if stored.version != expected {
                return Ok(false);
            }
            let next = Dashboard {
                name: dashboard.name.clone(),
                description: dashboard.description.clone(),
                updated_at: dashboard.updated_at,
                version: expected + 1,
                ..stored
            };
            cx.put(
                Tbl::Dashboards,
                uuid_key(next.id.to_bytes()),
                Some(Row::Dashboard(next.clone())),
            )?;
            for (key, old) in widgets_of(cx, next.id) {
                if !widgets.iter().any(|w| w.id == old.id) {
                    cx.put(Tbl::Widgets, key, None)?;
                }
            }
            put_widgets(cx, &next, widgets)?;
            Ok(true)
        })
        .await
    }

    async fn delete_if_version(
        &self,
        project_id: ProjectId,
        id: DashboardId,
        expected: u64,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| match find(cx, project_id, id) {
            Some(stored) if stored.version == expected => {
                cx.put(Tbl::Dashboards, uuid_key(id.to_bytes()), None)?;
                Ok(true)
            }
            _ => Ok(false),
        })
        .await
    }

    async fn delete_project(&self, project_id: ProjectId) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut deleted = 0;
            for dashboard in all!(cx, Tbl::Dashboards, Row::Dashboard) {
                if dashboard.project_id == project_id {
                    // Its widgets go with it, as `widgets_dashboard_id_fkey` cascades.
                    cx.put(Tbl::Dashboards, uuid_key(dashboard.id.to_bytes()), None)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await
    }
}
