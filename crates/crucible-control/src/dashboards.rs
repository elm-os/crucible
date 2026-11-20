//! Dashboards and their widgets (contract 11.1), and the repository over them. A widget's query, display options
//! and rectangle are JSON documents that `crucible-api` owns. The dashboard's `version` covers its widgets: a
//! change to any widget is a change to the dashboard, so a layout that moves several widgets is one change, and
//! the widget cap and the free-cell rule are checked against one version. Built-in dashboards are code, not rows.

use async_trait::async_trait;
use crucible_records::{DashboardId, ProjectId, Timestamp, WidgetId};

use crate::error::TxError;
use crate::types::JsonDoc;

/// A dashboard.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dashboard {
    /// The id.
    pub id: DashboardId,
    /// The project.
    pub project_id: ProjectId,
    /// The display name.
    pub name: String,
    /// What the dashboard is for; may be empty.
    pub description: String,
    /// The optimistic-concurrency version of the dashboard and its widgets; starts at 1.
    pub version: u64,
    /// When the dashboard was created.
    pub created_at: Timestamp,
    /// When the dashboard or one of its widgets last changed.
    pub updated_at: Timestamp,
}

/// One widget of a dashboard. Its row also carries the dashboard and the project, which the store fills in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Widget {
    /// The id, unique within its dashboard.
    pub id: WidgetId,
    /// The metrics query without a window; `None` for a widget that reads no data.
    pub query: Option<JsonDoc>,
    /// The kind, the title and the display options.
    pub display: JsonDoc,
    /// The rectangle on the grid.
    pub layout: JsonDoc,
}

/// A dashboard with its widgets, ordered by id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DashboardRows {
    /// The dashboard.
    pub dashboard: Dashboard,
    /// Its widgets.
    pub widgets: Vec<Widget>,
}

/// A dashboard as a list shows it: without its widgets, with how many it has.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DashboardEntry {
    /// The dashboard.
    pub dashboard: Dashboard,
    /// How many widgets it has.
    pub widgets: u32,
}

/// Dashboards and their widgets. Every method is one statement.
#[async_trait]
pub trait DashboardRepo: Send + Sync {
    /// The dashboard of the project with this id, with its widgets.
    async fn get(
        &self,
        project_id: ProjectId,
        id: DashboardId,
    ) -> Result<Option<DashboardRows>, TxError>;
    /// The dashboards of a project, ordered by id.
    async fn list(&self, project_id: ProjectId) -> Result<Vec<DashboardEntry>, TxError>;
    /// Creates a dashboard with its widgets, whose ids must be distinct. A taken id is
    /// `UniqueViolation("dashboards_pkey")`; an unknown project is
    /// `ForeignKeyViolation("dashboards_project_id_fkey")`.
    async fn insert(&self, dashboard: &Dashboard, widgets: &[Widget]) -> Result<(), TxError>;
    /// If the stored version is `expected`: writes the dashboard's name, description and `updated_at` with
    /// version `expected + 1`, and makes `widgets` (whose ids must be distinct) its widgets. A stored widget whose
    /// id is among them is rewritten, the others are deleted. `false` when the version differs or there is no
    /// such dashboard in the project.
    async fn update_if_version(
        &self,
        dashboard: &Dashboard,
        widgets: &[Widget],
        expected: u64,
    ) -> Result<bool, TxError>;
    /// Deletes the dashboard with its widgets if its stored version is `expected`. `false` when it is not or
    /// there is no such dashboard in the project.
    async fn delete_if_version(
        &self,
        project_id: ProjectId,
        id: DashboardId,
        expected: u64,
    ) -> Result<bool, TxError>;
}
