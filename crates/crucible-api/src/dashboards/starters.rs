//! The built-in starter dashboards (contract 12.6): definitions in code, served read-only under their
//! `builtin_key` and copied into rows by Duplicate, so an improved definition reaches every project without a
//! migration. A starter is listed when every source it reads has metrics in the registry: the MCP starter
//! appears once the `mcp_ops` source does.

use crucible_api_types::{
    ChartForm, Dashboard, MeasureRequest, Widget, WidgetDisplay, WidgetKind, WidgetLayout,
    WidgetQuery,
};
use crucible_plan::{Source, SourceSchema};

/// The version of the definitions. It goes up when a starter changes.
const VERSION: u32 = 1;

fn query(
    source: &str,
    measures: &[(&str, &str)],
    dimensions: &[&str],
    top_n: Option<u32>,
) -> WidgetQuery {
    WidgetQuery {
        source: source.to_owned(),
        measures: measures
            .iter()
            .map(|(measure, aggregation)| MeasureRequest {
                measure: (*measure).to_owned(),
                aggregation: (*aggregation).to_owned(),
            })
            .collect(),
        dimensions: (!dimensions.is_empty())
            .then(|| dimensions.iter().map(|d| (*d).to_owned()).collect()),
        filter: None,
        granularity: None,
        timezone: None,
        top_n,
        fill: None,
    }
}

fn spans(measures: &[(&str, &str)]) -> WidgetQuery {
    query("spans", measures, &[], None)
}

fn by(source: &str, measures: &[(&str, &str)], dimension: &str, top_n: u32) -> WidgetQuery {
    query(source, measures, &[dimension], Some(top_n))
}

fn widget(
    id: &str,
    kind: WidgetKind,
    title: &str,
    query: WidgetQuery,
    chart: Option<ChartForm>,
    [x, y, w, h]: [u32; 4],
) -> Widget {
    Widget {
        id: id.to_owned(),
        query: Some(query),
        display: WidgetDisplay {
            kind,
            title: title.to_owned(),
            chart,
            text: None,
        },
        layout: WidgetLayout { x, y, w, h },
    }
}

fn stat(id: &str, title: &str, query: WidgetQuery, at: [u32; 4]) -> Widget {
    widget(id, WidgetKind::Stat, title, query, None, at)
}

fn lines(id: &str, title: &str, query: WidgetQuery, at: [u32; 4]) -> Widget {
    widget(
        id,
        WidgetKind::Timeseries,
        title,
        query,
        Some(ChartForm::Line),
        at,
    )
}

fn bars(id: &str, title: &str, query: WidgetQuery, at: [u32; 4]) -> Widget {
    widget(
        id,
        WidgetKind::Timeseries,
        title,
        query,
        Some(ChartForm::Bar),
        at,
    )
}

fn breakdown(id: &str, title: &str, query: WidgetQuery, at: [u32; 4]) -> Widget {
    widget(id, WidgetKind::Bar, title, query, None, at)
}

fn table(id: &str, title: &str, query: WidgetQuery, at: [u32; 4]) -> Widget {
    widget(id, WidgetKind::Table, title, query, None, at)
}

fn dashboard(key: &str, name: &str, description: &str, widgets: Vec<Widget>) -> Dashboard {
    Dashboard {
        id: key.to_owned(),
        builtin_key: Some(key.to_owned()),
        name: name.to_owned(),
        description: description.to_owned(),
        widgets,
        version: VERSION,
        created_at: None,
        updated_at: None,
    }
}

const PERCENTILES: [(&str, &str); 3] = [
    ("duration", "p50"),
    ("duration", "p95"),
    ("duration", "p99"),
];

fn overview() -> Dashboard {
    dashboard(
        "overview",
        "Agent overview",
        "Traces, spans, errors and cost at a glance.",
        vec![
            stat(
                "traces",
                "Traces",
                query("traces", &[("traces", "sum")], &[], None),
                [0, 0, 3, 2],
            ),
            stat("spans", "Spans", spans(&[("count", "sum")]), [3, 0, 3, 2]),
            stat(
                "errors",
                "Errors",
                spans(&[("error_count", "sum")]),
                [6, 0, 3, 2],
            ),
            stat(
                "cost",
                "Cost",
                spans(&[("total_cost", "sum")]),
                [9, 0, 3, 2],
            ),
            bars(
                "traces-over-time",
                "Traces over time",
                query("traces", &[("traces", "sum")], &[], None),
                [0, 2, 6, 3],
            ),
            lines(
                "spans-and-errors",
                "Spans and errors",
                spans(&[("count", "sum"), ("error_count", "sum")]),
                [6, 2, 6, 3],
            ),
            lines(
                "duration",
                "Span duration",
                spans(&PERCENTILES),
                [0, 5, 6, 3],
            ),
            breakdown(
                "cost-by-model",
                "Cost by model",
                by("spans", &[("total_cost", "sum")], "model", 10),
                [6, 5, 6, 3],
            ),
        ],
    )
}

fn cost_and_tokens() -> Dashboard {
    dashboard(
        "cost-and-tokens",
        "Cost and tokens",
        "What the models cost, and the tokens behind it.",
        vec![
            stat(
                "cost",
                "Cost",
                spans(&[("total_cost", "sum")]),
                [0, 0, 4, 2],
            ),
            stat(
                "input",
                "Input tokens",
                spans(&[("input_tokens", "sum")]),
                [4, 0, 4, 2],
            ),
            stat(
                "output",
                "Output tokens",
                spans(&[("output_tokens", "sum")]),
                [8, 0, 4, 2],
            ),
            bars(
                "cost-over-time",
                "Cost over time",
                spans(&[("total_cost", "sum")]),
                [0, 2, 6, 3],
            ),
            lines(
                "tokens-over-time",
                "Tokens over time",
                spans(&[("input_tokens", "sum"), ("output_tokens", "sum")]),
                [6, 2, 6, 3],
            ),
            table(
                "by-model",
                "Cost and tokens by model",
                by(
                    "spans",
                    &[
                        ("total_cost", "sum"),
                        ("input_tokens", "sum"),
                        ("output_tokens", "sum"),
                    ],
                    "model",
                    10,
                ),
                [0, 5, 12, 4],
            ),
        ],
    )
}

fn latency() -> Dashboard {
    dashboard(
        "latency",
        "Latency",
        "How long spans take, and how long models take to answer.",
        vec![
            stat(
                "p50",
                "Duration p50",
                spans(&[("duration", "p50")]),
                [0, 0, 3, 2],
            ),
            stat(
                "p95",
                "Duration p95",
                spans(&[("duration", "p95")]),
                [3, 0, 3, 2],
            ),
            stat(
                "p99",
                "Duration p99",
                spans(&[("duration", "p99")]),
                [6, 0, 3, 2],
            ),
            stat(
                "ttft-p95",
                "Time to first token p95",
                spans(&[("ttft", "p95")]),
                [9, 0, 3, 2],
            ),
            lines(
                "duration",
                "Span duration",
                spans(&PERCENTILES),
                [0, 2, 6, 3],
            ),
            lines(
                "ttft",
                "Time to first token",
                spans(&[("ttft", "p50"), ("ttft", "p95"), ("ttft", "p99")]),
                [6, 2, 6, 3],
            ),
            lines(
                "p95-by-model",
                "Duration p95 by model",
                by("spans", &[("duration", "p95")], "model", 4),
                [0, 5, 12, 3],
            ),
        ],
    )
}

fn errors() -> Dashboard {
    dashboard(
        "errors",
        "Errors",
        "Where spans fail: by type, model and environment.",
        vec![
            stat(
                "errors",
                "Errors",
                spans(&[("error_count", "sum")]),
                [0, 0, 6, 2],
            ),
            stat("spans", "Spans", spans(&[("count", "sum")]), [6, 0, 6, 2]),
            bars(
                "over-time",
                "Errors over time",
                spans(&[("error_count", "sum")]),
                [0, 2, 6, 3],
            ),
            lines(
                "by-type",
                "Errors by type",
                by("spans", &[("error_count", "sum")], "observation_type", 3),
                [6, 2, 6, 3],
            ),
            breakdown(
                "by-model",
                "Errors by model",
                by("spans", &[("error_count", "sum")], "model", 10),
                [0, 5, 6, 3],
            ),
            table(
                "by-environment",
                "Errors and spans by environment",
                by(
                    "spans",
                    &[("error_count", "sum"), ("count", "sum")],
                    "environment",
                    10,
                ),
                [6, 5, 6, 3],
            ),
        ],
    )
}

fn mcp() -> Dashboard {
    let calls = [("mcp.calls", "sum")];
    let errors = [("mcp.error_count", "sum")];
    dashboard(
        "mcp",
        "MCP servers",
        "Calls, errors and latency of the MCP servers.",
        vec![
            stat(
                "calls",
                "Calls",
                query("mcp_ops", &calls, &[], None),
                [0, 0, 4, 2],
            ),
            stat(
                "errors",
                "Errors",
                query("mcp_ops", &errors, &[], None),
                [4, 0, 4, 2],
            ),
            stat(
                "error-rate",
                "Error rate",
                query("mcp_ops", &[("mcp.error_rate", "rate")], &[], None),
                [8, 0, 4, 2],
            ),
            lines(
                "calls-by-server",
                "Calls by server",
                by("mcp_ops", &calls, "server_name", 3),
                [0, 2, 6, 3],
            ),
            lines(
                "duration-p90",
                "Call duration p90",
                query("mcp_ops", &[("mcp.duration_p90", "p90")], &[], None),
                [6, 2, 6, 3],
            ),
            breakdown(
                "calls-by-tool",
                "Calls by tool",
                by("mcp_ops", &calls, "target_name", 10),
                [0, 5, 6, 3],
            ),
            breakdown(
                "errors-by-tool",
                "Errors by tool",
                by("mcp_ops", &errors, "target_name", 10),
                [6, 5, 6, 3],
            ),
        ],
    )
}

/// Every starter, listed or not, in the order the list shows them.
pub(super) fn all() -> Vec<Dashboard> {
    vec![overview(), cost_and_tokens(), latency(), errors(), mcp()]
}

/// Whether every source the dashboard reads has metrics in the registry.
pub(super) fn listed(dashboard: &Dashboard) -> bool {
    dashboard.widgets.iter().all(|widget| {
        widget.query.as_ref().is_none_or(|query| {
            Source::parse(&query.source).is_some_and(|source| SourceSchema::of(source).is_some())
        })
    })
}

/// The starters this server lists.
pub(crate) fn starters() -> Vec<Dashboard> {
    all().into_iter().filter(listed).collect()
}

/// The listed starter with this key.
pub(crate) fn starter(key: &str) -> Option<Dashboard> {
    starters()
        .into_iter()
        .find(|d| d.builtin_key.as_deref() == Some(key))
}
