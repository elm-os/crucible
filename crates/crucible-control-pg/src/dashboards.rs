//! The dashboard repository over Postgres (migration V9): a dashboard and its widgets are read, written and
//! deleted in one statement each, with the widgets passed as one JSON array.

use async_trait::async_trait;
use crucible_control::{
    Dashboard, DashboardEntry, DashboardRepo, DashboardRows, JsonDoc, TxError, Widget,
};
use crucible_records::{DashboardId, ProjectId, WidgetId};

use crate::sql::{Cols, P, int};
use crate::store::{Handle, every};

const DASHBOARD: &str =
    "d.id, d.project_id, d.name, d.description, d.version, d.created_at, d.updated_at";

/// The columns of a stored widget, from `jsonb_to_recordset` over the array that [`widgets_json`] writes.
const RECORDSET: &str =
    "jsonb_to_recordset($WIDGETS::jsonb) AS w(id uuid, query jsonb, display jsonb, layout jsonb)";

fn dashboard(c: &mut Cols<'_>) -> Result<Dashboard, TxError> {
    Ok(Dashboard {
        id: DashboardId::from_uuid(c.uuid()?),
        project_id: ProjectId::from_uuid(c.uuid()?),
        name: c.text()?,
        description: c.text()?,
        version: c.u64()?,
        created_at: c.ts()?,
        updated_at: c.ts()?,
    })
}

/// The widgets as the JSON array the statements read with `jsonb_to_recordset`. The documents are JSON
/// already, so they are written as they are.
fn widgets_json(widgets: &[Widget]) -> String {
    let rows: Vec<String> = widgets
        .iter()
        .map(|w| {
            format!(
                r#"{{"id":"{}","query":{},"display":{},"layout":{}}}"#,
                w.id,
                w.query
                    .as_ref()
                    .map_or_else(|| "null".to_owned(), JsonDoc::to_text),
                w.display.to_text(),
                w.layout.to_text()
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

fn recordset(param: usize) -> String {
    RECORDSET.replace("$WIDGETS", &format!("${param}"))
}

#[async_trait]
impl DashboardRepo for Handle {
    async fn get(
        &self,
        project_id: ProjectId,
        id: DashboardId,
    ) -> Result<Option<DashboardRows>, TxError> {
        let sql = format!(
            "SELECT {DASHBOARD}, w.id, w.query::text, w.display::text, w.layout::text \
             FROM dashboards d LEFT JOIN widgets w ON w.dashboard_id = d.id \
             WHERE d.project_id = $1 AND d.id = $2 ORDER BY w.id"
        );
        let rows = self
            .read(&sql, &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid())])
            .await?;
        let mut found: Option<DashboardRows> = None;
        for row in &rows {
            let mut c = Cols::new(row);
            let dashboard = dashboard(&mut c)?;
            let widget = match c.opt_uuid()? {
                Some(widget_id) => Some(Widget {
                    id: WidgetId::from_uuid(widget_id),
                    query: c
                        .opt_text()?
                        .map(|text| JsonDoc::parse(&text))
                        .transpose()?,
                    display: JsonDoc::parse(&c.text()?)?,
                    layout: JsonDoc::parse(&c.text()?)?,
                }),
                None => None,
            };
            let rows = found.get_or_insert_with(|| DashboardRows {
                dashboard,
                widgets: Vec::new(),
            });
            rows.widgets.extend(widget);
        }
        Ok(found)
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<DashboardEntry>, TxError> {
        let sql = format!(
            "SELECT {DASHBOARD}, (SELECT count(*) FROM widgets w WHERE w.dashboard_id = d.id)::integer \
             FROM dashboards d WHERE d.project_id = $1 ORDER BY d.id"
        );
        every(
            &self.read(&sql, &[P::Uuid(project_id.uuid())]).await?,
            |c| {
                Ok(DashboardEntry {
                    dashboard: dashboard(c)?,
                    widgets: c.u32()?,
                })
            },
        )
    }

    async fn insert(&self, dashboard: &Dashboard, widgets: &[Widget]) -> Result<(), TxError> {
        let json = widgets_json(widgets);
        let sql = format!(
            "WITH d AS (INSERT INTO dashboards (id, project_id, name, description, version, created_at, \
             updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id), \
             put AS (INSERT INTO widgets (dashboard_id, id, project_id, query, display, layout) \
             SELECT d.id, w.id, $2, w.query, w.display, w.layout FROM d CROSS JOIN {}) \
             SELECT 1 FROM d",
            recordset(8)
        );
        self.write(
            &sql,
            &[
                P::Uuid(dashboard.id.uuid()),
                P::Uuid(dashboard.project_id.uuid()),
                P::Text(&dashboard.name),
                P::Text(&dashboard.description),
                int(dashboard.version)?,
                P::Ts(dashboard.created_at),
                P::Ts(dashboard.updated_at),
                P::Text(&json),
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if_version(
        &self,
        dashboard: &Dashboard,
        widgets: &[Widget],
        expected: u64,
    ) -> Result<bool, TxError> {
        let json = widgets_json(widgets);
        // The delete and the upsert touch disjoint rows: the ids that are not in the array, and those that are.
        let sql = format!(
            "WITH d AS (UPDATE dashboards SET name = $3, description = $4, updated_at = $5, \
             version = version + 1 WHERE project_id = $1 AND id = $2 AND version = $6 RETURNING id), \
             w AS (SELECT * FROM {}), \
             gone AS (DELETE FROM widgets WHERE dashboard_id IN (SELECT id FROM d) \
             AND id NOT IN (SELECT id FROM w)), \
             put AS (INSERT INTO widgets (dashboard_id, id, project_id, query, display, layout) \
             SELECT d.id, w.id, $1, w.query, w.display, w.layout FROM d CROSS JOIN w \
             ON CONFLICT (dashboard_id, id) DO UPDATE SET query = excluded.query, \
             display = excluded.display, layout = excluded.layout) \
             SELECT 1 FROM d",
            recordset(7)
        );
        let changed = self
            .write(
                &sql,
                &[
                    P::Uuid(dashboard.project_id.uuid()),
                    P::Uuid(dashboard.id.uuid()),
                    P::Text(&dashboard.name),
                    P::Text(&dashboard.description),
                    P::Ts(dashboard.updated_at),
                    int(expected)?,
                    P::Text(&json),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn delete_if_version(
        &self,
        project_id: ProjectId,
        id: DashboardId,
        expected: u64,
    ) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM dashboards WHERE project_id = $1 AND id = $2 AND version = $3 RETURNING 1",
                &[
                    P::Uuid(project_id.uuid()),
                    P::Uuid(id.uuid()),
                    int(expected)?,
                ],
            )
            .await?;
        Ok(deleted == 1)
    }
}
