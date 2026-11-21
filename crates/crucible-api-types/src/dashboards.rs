//! Dashboards and their widgets: `projects/{project_id}/dashboards/...` (contract 12.6).
//!
//! A dashboard is a 12-column grid of at most 50 widgets. A widget holds a metrics query without a window (the
//! dashboard's time range supplies it), display options and a rectangle on the grid. The dashboard's version
//! covers its widgets: every change to a widget takes the dashboard's `ETag` in `If-Match` and moves it on.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::metrics::{FillMode, Granularity, MeasureRequest, MetricsFilter, MetricsQueryRequest};

/// What a widget shows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    /// The measures over time: up to four series.
    Timeseries,
    /// One number for the time range, with its trend.
    Stat,
    /// The measures of each dimension key for the time range.
    Table,
    /// One measure of each dimension key for the time range, as horizontal bars.
    Bar,
    /// Text in a small subset of Markdown: headings, lists, emphasis, code and links.
    Markdown,
}

/// How a time series draws its values.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ChartForm {
    /// Lines.
    Line,
    /// Bars per bucket.
    Bar,
    /// Lines with the area under each filled, the series laid over one another, not stacked.
    Area,
}

/// A widget's metrics query: `MetricsQuery` v1 without its window, which the dashboard's time range supplies. Its
/// members are those of the body of `POST metrics/query` other than `from` and `to`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WidgetQuery {
    /// The source, as the metrics schema lists it.
    pub source: String,
    /// One to four measures.
    pub measures: Vec<MeasureRequest>,
    /// Up to two dimension ids of the source, to split the series by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dimensions: Option<Vec<String>>,
    /// A filter over the source's filter fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub filter: Option<MetricsFilter>,
    /// The bucket width; `auto` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub granularity: Option<Granularity>,
    /// The time zone of the buckets. Only `UTC` is supported yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub timezone: Option<String>,
    /// With dimensions: how many keys to return, 1 to 20, default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub top_n: Option<u32>,
    /// What an empty bucket holds; `zero` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub fill: Option<FillMode>,
    /// Source `mcp_ops` only, and required there: the side to read, `client` or `server`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub side: Option<crate::mcp::McpSideParam>,
    /// Source `mcp_ops` only: which capture source each server is read from (contract 9.5); `auto` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub capture_source: Option<crate::mcp::SourceParam>,
    /// Source `mcp_ops` only: whether calls the caller caused count; false by default (contract 9.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub include_caller_faults: Option<bool>,
}

impl WidgetQuery {
    /// The body of `POST metrics/query` that reads this query over the window `[from, to)`.
    pub fn with_window(&self, from: String, to: String) -> MetricsQueryRequest {
        let Self {
            source,
            measures,
            dimensions,
            filter,
            granularity,
            timezone,
            top_n,
            fill,
            side,
            capture_source,
            include_caller_faults,
        } = self.clone();
        MetricsQueryRequest {
            source,
            measures,
            dimensions,
            filter,
            granularity,
            from,
            to,
            timezone,
            top_n,
            fill,
            side,
            capture_source,
            include_caller_faults,
        }
    }
}

/// How a widget is drawn.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WidgetDisplay {
    /// What the widget shows.
    pub kind: WidgetKind,
    /// The title, 1 to 120 characters.
    pub title: String,
    /// `timeseries` only: lines (the default), bars per bucket, or areas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub chart: Option<ChartForm>,
    /// `markdown` only: the text, 1 to 10,000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub text: Option<String>,
}

/// A widget's rectangle on the dashboard's grid of 12 columns, in cells. Rows are as tall as columns are wide.
/// Widgets of one dashboard never overlap.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WidgetLayout {
    /// The first column, from 0.
    pub x: u32,
    /// The first row, from 0.
    pub y: u32,
    /// The width in columns, at most 12.
    pub w: u32,
    /// The height in rows, at most 12.
    pub h: u32,
}

/// A widget of a dashboard.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Widget {
    /// The id: a UUID, or a name within a built-in dashboard.
    pub id: String,
    /// The query; absent for `markdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub query: Option<WidgetQuery>,
    /// How it is drawn.
    pub display: WidgetDisplay,
    /// Where it sits.
    pub layout: WidgetLayout,
}

/// A widget to add with `POST .../widgets`, or one of the widgets of a dashboard's `widgets`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WidgetInput {
    /// In a dashboard's `widgets`: the id of one of its widgets, which is rewritten; absent for a new widget.
    /// `POST .../widgets` does not take it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub id: Option<String>,
    /// The query; required unless the widget is `markdown`, which takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub query: Option<WidgetQuery>,
    /// How it is drawn.
    pub display: WidgetDisplay,
    /// Where it sits.
    pub layout: WidgetLayout,
}

/// The body of `PATCH .../widgets/{widget_id}`: the members to change. A `markdown` display drops the query.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct WidgetPatch {
    /// The query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub query: Option<WidgetQuery>,
    /// How it is drawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub display: Option<WidgetDisplay>,
    /// Where it sits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub layout: Option<WidgetLayout>,
}

/// A dashboard with its widgets.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Dashboard {
    /// The id: a UUID, or the `builtin_key` of a built-in dashboard.
    pub id: String,
    /// The key of a built-in dashboard, which is read-only: duplicate it to change it. Absent on the project's
    /// own dashboards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub builtin_key: Option<String>,
    /// The name.
    pub name: String,
    /// What the dashboard is for; may be empty.
    pub description: String,
    /// The widgets, in reading order: by row, then by column.
    pub widgets: Vec<Widget>,
    /// The version, also sent as `ETag`; every change to the dashboard or a widget takes it in `If-Match`. A
    /// built-in dashboard has the version of its definition.
    pub version: u32,
    /// When the dashboard was created; null for a built-in one.
    #[schema(required = true)]
    pub created_at: Option<String>,
    /// When the dashboard or a widget last changed; null for a built-in one.
    #[schema(required = true)]
    pub updated_at: Option<String>,
}

/// A dashboard as the list shows it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct DashboardSummary {
    /// The id: a UUID, or the `builtin_key` of a built-in dashboard.
    pub id: String,
    /// The key of a built-in dashboard; absent on the project's own dashboards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub builtin_key: Option<String>,
    /// The name.
    pub name: String,
    /// What the dashboard is for; may be empty.
    pub description: String,
    /// How many widgets it has.
    pub widget_count: u32,
    /// The version.
    pub version: u32,
    /// When the dashboard or a widget last changed; null for a built-in one.
    #[schema(required = true)]
    pub updated_at: Option<String>,
}

/// The answer of `GET projects/{project_id}/dashboards`: the project's dashboards, oldest first, then the
/// built-in ones.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct DashboardList {
    /// The dashboards.
    pub data: Vec<DashboardSummary>,
}

/// The body of `POST projects/{project_id}/dashboards`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DashboardCreate {
    /// The name, 1 to 120 characters.
    pub name: String,
    /// What the dashboard is for, at most 1,000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    /// The widgets, without ids; at most 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub widgets: Option<Vec<WidgetInput>>,
}

/// The body of `PATCH projects/{project_id}/dashboards/{dashboard_id}`: the members to change.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DashboardPatch {
    /// The name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
    /// What the dashboard is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    /// Every widget of the dashboard after the change: a member with the `id` of one of its widgets rewrites
    /// that widget, one without an `id` adds a widget, and a widget the list leaves out is deleted. One change,
    /// so a layout that moves several widgets is checked and stored whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub widgets: Option<Vec<WidgetInput>>,
}

/// The body of `POST projects/{project_id}/dashboards/{dashboard_id}/duplicate`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct DashboardDuplicate {
    /// The copy's name; the original's name with " (copy)" by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use utoipa::PartialSchema as _;
    use utoipa::openapi::{RefOr, Schema};

    use super::*;

    fn properties(schema: RefOr<Schema>) -> BTreeSet<String> {
        match schema {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => panic!("not an object schema"),
        }
    }

    // A widget query is a metrics query without its window, so a member added to one must be added to the
    // other, or `with_window` would drop it.
    #[test]
    fn a_widget_query_has_every_member_of_a_metrics_query_but_the_window() {
        let mut query = properties(MetricsQueryRequest::schema());
        assert!(query.remove("from") && query.remove("to"));
        assert_eq!(properties(WidgetQuery::schema()), query);
    }

    #[test]
    fn with_window_keeps_every_member() {
        let widget = WidgetQuery {
            source: "spans".to_owned(),
            measures: vec![MeasureRequest {
                measure: "duration".to_owned(),
                aggregation: "p95".to_owned(),
            }],
            dimensions: Some(vec!["model".to_owned()]),
            filter: Some(MetricsFilter::In {
                field: "level".to_owned(),
                values: vec!["error".to_owned()],
            }),
            granularity: Some(Granularity::Hour),
            timezone: Some("UTC".to_owned()),
            top_n: Some(3),
            fill: Some(FillMode::None),
            side: Some(crate::mcp::McpSideParam::Server),
            capture_source: Some(crate::mcp::SourceParam::Proxy),
            include_caller_faults: Some(true),
        };
        let request = widget.with_window("a".to_owned(), "b".to_owned());
        let mut json = serde_json::to_value(&request).expect("serializes");
        let object = json.as_object_mut().expect("an object");
        assert_eq!(object.remove("from"), Some("a".into()));
        assert_eq!(object.remove("to"), Some("b".into()));
        assert_eq!(json, serde_json::to_value(&widget).expect("serializes"));
    }
}
