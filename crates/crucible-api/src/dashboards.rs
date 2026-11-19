//! Dashboards (contracts 11.1 and 12.6): the rules every dashboard keeps, checked whole on each change, the
//! conversions between the stored rows and the DTOs, and the built-in starter dashboards.
//!
//! - A dashboard has a name of 1 to 120 characters, a description of at most 1,000, and at most 50 widgets.
//! - A widget sits on a grid of 12 columns and 600 rows, at least its kind's smallest size and at most 12 by 12
//!   cells, and overlaps no other widget.
//! - A widget's kind decides the shape of its query: a `stat` is one measure without dimensions, a `bar` is one
//!   measure split by one or two dimensions, a `timeseries` draws at most four series of one unit (one axis),
//!   and a `table` takes any query. A `markdown` widget has text and no query. Every query passes the checks of `metrics/query`, for a
//!   window that every granularity fits, so the dashboard's time range is the only thing that can still make a
//!   widget too broad.

mod starters;

use std::cmp::Ordering;

use crucible_api_types::{
    Dashboard as WireDashboard, DashboardSummary, FieldError, Widget as WireWidget, WidgetDisplay,
    WidgetKind, WidgetLayout, WidgetQuery,
};
use crucible_control as control;
use crucible_plan::{METRICS_MAX_SERIES, MetricsQuery};
use crucible_records::{RequestId, WidgetId};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::control::version;
use crate::problem::{ApiError, field_error};
use crate::routes::metrics::parse_request;

pub(crate) use starters::{starter, starters};

/// Columns of the grid.
pub(crate) const GRID_COLUMNS: u32 = 12;
/// Rows of the grid: room for the most widgets at their tallest, one under another.
pub(crate) const GRID_ROWS: u32 = 600;
/// The widest and the tallest a widget gets, in cells.
pub(crate) const MAX_SIZE: u32 = 12;
/// The most widgets of one dashboard (contract 12.6).
pub(crate) const MAX_WIDGETS: usize = 50;
/// The most series a chart draws: there are four series styles, each a dash and a marker.
pub(crate) const MAX_CHART_SERIES: u32 = 4;
const MAX_NAME: usize = 120;
const MAX_TITLE: usize = 120;
const MAX_DESCRIPTION: usize = 1_000;
const MAX_TEXT: usize = 10_000;
/// The window a widget's query is checked over: one hour, which every granularity fits.
const CHECK_WINDOW: (&str, &str) = ("2026-01-01T00:00:00.000000Z", "2026-01-01T01:00:00.000000Z");

/// The smallest width and height of a widget of `kind`, in cells: a chart needs room for its axes.
pub(crate) const fn min_size(kind: WidgetKind) -> (u32, u32) {
    match kind {
        WidgetKind::Timeseries | WidgetKind::Table | WidgetKind::Bar => (3, 2),
        WidgetKind::Stat | WidgetKind::Markdown => (2, 1),
    }
}

const fn kind_name(kind: WidgetKind) -> &'static str {
    match kind {
        WidgetKind::Timeseries => "timeseries",
        WidgetKind::Stat => "stat",
        WidgetKind::Table => "table",
        WidgetKind::Bar => "bar",
        WidgetKind::Markdown => "markdown",
    }
}

/// One widget of a dashboard as the rules see it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Draft<'a> {
    /// Where the widget is in the request, like `widgets/3/` or `` for a body that is the widget; `None` for a
    /// stored widget the request leaves as it is, which is not checked again.
    pub(crate) at: Option<&'a str>,
    /// The query.
    pub(crate) query: Option<&'a WidgetQuery>,
    /// How it is drawn.
    pub(crate) display: &'a WidgetDisplay,
    /// Where it sits.
    pub(crate) layout: WidgetLayout,
}

fn error(at: &str, member: &str, code: &str, message: &str) -> FieldError {
    field_error(&format!("{at}{member}"), code, message)
}

/// The error of a dashboard name, if it has one.
pub(crate) fn check_name(name: &str) -> Option<FieldError> {
    (name.trim().is_empty() || name.chars().count() > MAX_NAME)
        .then(|| field_error("name", "invalid", "a name of 1 to 120 characters"))
}

/// The error of a dashboard description, if it has one.
pub(crate) fn check_description(description: &str) -> Option<FieldError> {
    (description.chars().count() > MAX_DESCRIPTION)
        .then(|| field_error("description", "invalid", "at most 1,000 characters"))
}

/// Checks the widgets of a dashboard as they will be stored: each widget the request writes, that no two
/// overlap, and how many there are. `list_at` is the member that holds the widgets, for the count's error.
/// `Err` is a problem other than a validation failure.
pub(crate) fn check_widgets(
    rid: RequestId,
    drafts: &[Draft<'_>],
    list_at: &str,
) -> Result<Vec<FieldError>, ApiError> {
    let mut errors = Vec::new();
    if drafts.len() > MAX_WIDGETS {
        let message = "a dashboard has at most 50 widgets; remove some first";
        errors.push(field_error(list_at, "too_many", message));
    }
    for draft in drafts {
        if let Some(at) = draft.at {
            errors.extend(check_widget(rid, at, draft)?);
        }
    }
    for (j, later) in drafts.iter().enumerate() {
        let Some(earlier) = drafts[..j]
            .iter()
            .find(|earlier| overlaps(earlier.layout, later.layout))
        else {
            continue;
        };
        // The error goes to a widget of the request; stored widgets never overlap each other.
        let (at, other) = match (later.at, earlier.at) {
            (Some(at), _) => (at, earlier),
            (None, Some(at)) => (at, later),
            (None, None) => continue,
        };
        let message = format!(
            "the widget overlaps \"{}\"; move or resize one of them",
            other.display.title
        );
        errors.push(error(at, "layout", "overlaps", &message));
    }
    Ok(errors)
}

/// Whether two rectangles share a cell. An empty rectangle shares none.
pub(crate) fn overlaps(a: WidgetLayout, b: WidgetLayout) -> bool {
    let meet = |a: u32, a_len: u32, b: u32, b_len: u32| {
        let (a, b) = (u64::from(a), u64::from(b));
        a.max(b) < (a + u64::from(a_len)).min(b + u64::from(b_len))
    };
    meet(a.x, a.w, b.x, b.w) && meet(a.y, a.h, b.y, b.h)
}

fn check_widget(rid: RequestId, at: &str, draft: &Draft<'_>) -> Result<Vec<FieldError>, ApiError> {
    let display = draft.display;
    let kind = kind_name(display.kind);
    let mut errors = Vec::new();
    if display.title.trim().is_empty() || display.title.chars().count() > MAX_TITLE {
        errors.push(error(
            at,
            "display/title",
            "invalid",
            "a title of 1 to 120 characters",
        ));
    }
    if display.chart.is_some() && display.kind != WidgetKind::Timeseries {
        let message = "only a timeseries widget has a chart form; remove `chart`";
        errors.push(error(at, "display/chart", "invalid", message));
    }
    let markdown = display.kind == WidgetKind::Markdown;
    match (&display.text, markdown) {
        (None, true) => {
            let message = "a markdown widget needs its text";
            errors.push(error(at, "display/text", "required", message));
        }
        (Some(text), true) if text.trim().is_empty() || text.chars().count() > MAX_TEXT => {
            let message = "text of 1 to 10,000 characters";
            errors.push(error(at, "display/text", "invalid", message));
        }
        (Some(_), false) => {
            let message = "only a markdown widget has text; remove `text`";
            errors.push(error(at, "display/text", "invalid", message));
        }
        _ => {}
    }
    match (draft.query, markdown) {
        (Some(_), true) => {
            let message = "a markdown widget reads no data; remove `query`";
            errors.push(error(at, "query", "invalid", message));
        }
        (None, false) => {
            let message =
                format!("a {kind} widget needs a query; GET metrics/schema lists what it can ask");
            errors.push(error(at, "query", "required", &message));
        }
        (Some(query), false) => errors.extend(check_query(rid, at, display.kind, query)?),
        (None, true) => {}
    }
    let layout = draft.layout;
    let (min_w, min_h) = min_size(display.kind);
    let fits = (min_w..=MAX_SIZE).contains(&layout.w)
        && (min_h..=MAX_SIZE).contains(&layout.h)
        && layout
            .x
            .checked_add(layout.w)
            .is_some_and(|end| end <= GRID_COLUMNS)
        && layout
            .y
            .checked_add(layout.h)
            .is_some_and(|end| end <= GRID_ROWS);
    if !fits {
        let message = format!(
            "a {kind} widget is {min_w} to 12 columns wide and {min_h} to 12 rows tall, inside the grid's 12 \
             columns and 600 rows"
        );
        errors.push(error(at, "layout", "invalid", &message));
    }
    Ok(errors)
}

/// The series a chart of `query` draws: one per measure, or per measure and dimension key, plus the `other`
/// series of each additive measure.
pub(crate) fn chart_series(query: &MetricsQuery) -> u32 {
    if query.dimensions.is_empty() {
        return u32::try_from(query.measures.len()).unwrap_or(u32::MAX);
    }
    let keys = query.top_n.unwrap_or(METRICS_MAX_SERIES);
    query
        .measures
        .iter()
        .map(|(measure, _)| keys + u32::from(measure.additive()))
        .sum()
}

fn check_query(
    rid: RequestId,
    at: &str,
    kind: WidgetKind,
    query: &WidgetQuery,
) -> Result<Vec<FieldError>, ApiError> {
    let (from, to) = CHECK_WINDOW;
    let request = query.with_window(from.to_owned(), to.to_owned());
    let parsed = match parse_request(rid, 1, &request) {
        Ok((_, parsed)) => parsed,
        Err(problem) => return problem.field_errors_under(&format!("{at}query")),
    };
    let at = format!("{at}query/");
    let (measures, dimensions) = (parsed.measures.len(), parsed.dimensions.len());
    let mut errors = Vec::new();
    match kind {
        WidgetKind::Stat => {
            if measures != 1 {
                let message = "a stat widget shows one measure";
                errors.push(error(&at, "measures", "invalid", message));
            }
            if dimensions != 0 {
                let message = "a stat widget shows one number; remove the dimensions";
                errors.push(error(&at, "dimensions", "invalid", message));
            }
        }
        WidgetKind::Bar => {
            if measures != 1 {
                let message = "a bar widget shows one measure";
                errors.push(error(&at, "measures", "invalid", message));
            }
            if dimensions == 0 {
                let message = "a bar widget splits its measure by one or two dimensions";
                errors.push(error(&at, "dimensions", "required", message));
            }
        }
        WidgetKind::Timeseries => {
            // One axis: two measures of different units belong in two charts.
            let first = parsed.measures.first().map(|(m, _)| m.unit());
            if parsed.measures.iter().any(|(m, _)| Some(m.unit()) != first) {
                let message =
                    "a chart has one axis: choose measures of one unit, or one chart per unit";
                errors.push(error(&at, "measures", "invalid", message));
            }
            let series = chart_series(&parsed);
            if series > MAX_CHART_SERIES {
                let (member, fix) = if dimensions == 0 {
                    ("measures", "use at most four measures")
                } else {
                    (
                        "top_n",
                        "use one measure and lower `top_n` until the keys and `other` fit",
                    )
                };
                let message =
                    format!("a chart draws at most 4 series and this query has {series}; {fix}");
                errors.push(error(&at, member, "invalid", &message));
            }
        }
        WidgetKind::Table | WidgetKind::Markdown => {}
    }
    Ok(errors)
}

/// A JSON document of a widget part. Our own types always serialize.
pub(crate) fn doc(value: &impl Serialize) -> control::JsonDoc {
    serde_json::to_string(value)
        .ok()
        .and_then(|text| control::JsonDoc::parse(&text).ok())
        .unwrap_or_else(control::JsonDoc::empty)
}

fn part<T: DeserializeOwned>(doc: &control::JsonDoc, what: &str) -> Result<T, String> {
    serde_json::from_str(&doc.to_text()).map_err(|e| format!("a stored widget's {what}: {e}"))
}

/// A stored widget as the API serves it. `Err` names a part the store holds in a shape the API cannot read.
pub(crate) fn wire_widget(widget: &control::Widget) -> Result<WireWidget, String> {
    Ok(WireWidget {
        id: widget.id.to_string(),
        query: widget
            .query
            .as_ref()
            .map(|query| part(query, "query"))
            .transpose()?,
        display: part(&widget.display, "display")?,
        layout: part(&widget.layout, "layout")?,
    })
}

/// A widget as the store keeps it.
pub(crate) fn stored_widget(
    id: WidgetId,
    query: Option<&WidgetQuery>,
    display: &WidgetDisplay,
    layout: WidgetLayout,
) -> control::Widget {
    control::Widget {
        id,
        query: query.map(doc),
        display: doc(display),
        layout: doc(&layout),
    }
}

/// Reading order: by row, then by column.
pub(crate) fn reading_order(a: &WireWidget, b: &WireWidget) -> Ordering {
    (a.layout.y, a.layout.x, &a.id).cmp(&(b.layout.y, b.layout.x, &b.id))
}

/// A stored dashboard as the API serves it, its widgets in reading order.
pub(crate) fn wire_dashboard(rows: &control::DashboardRows) -> Result<WireDashboard, String> {
    let dashboard = &rows.dashboard;
    let mut widgets = rows
        .widgets
        .iter()
        .map(wire_widget)
        .collect::<Result<Vec<_>, _>>()?;
    widgets.sort_by(reading_order);
    Ok(WireDashboard {
        id: dashboard.id.to_string(),
        builtin_key: None,
        name: dashboard.name.clone(),
        description: dashboard.description.clone(),
        widgets,
        version: version(dashboard.version),
        created_at: Some(dashboard.created_at.to_string()),
        updated_at: Some(dashboard.updated_at.to_string()),
    })
}

/// A stored dashboard as the list shows it.
pub(crate) fn summary(entry: &control::DashboardEntry) -> DashboardSummary {
    let dashboard = &entry.dashboard;
    DashboardSummary {
        id: dashboard.id.to_string(),
        builtin_key: None,
        name: dashboard.name.clone(),
        description: dashboard.description.clone(),
        widget_count: entry.widgets,
        version: version(dashboard.version),
        updated_at: Some(dashboard.updated_at.to_string()),
    }
}

/// A built-in dashboard as the list shows it.
pub(crate) fn builtin_summary(dashboard: &WireDashboard) -> DashboardSummary {
    DashboardSummary {
        id: dashboard.id.clone(),
        builtin_key: dashboard.builtin_key.clone(),
        name: dashboard.name.clone(),
        description: dashboard.description.clone(),
        widget_count: u32::try_from(dashboard.widgets.len()).unwrap_or(u32::MAX),
        version: dashboard.version,
        updated_at: None,
    }
}

#[cfg(test)]
mod tests;
