use std::collections::BTreeSet;

use crucible_api_types::{ChartForm, MeasureRequest, WidgetInput};
use crucible_plan::{MeasureId, Source, SourceSchema};
use crucible_records::DashboardId;
use proptest::prelude::*;

use super::starters::{all, listed};
use super::*;

fn rid() -> RequestId {
    RequestId::from_bytes([1; 16])
}

fn spans_query(measures: &[(&str, &str)], dimensions: &[&str], top_n: Option<u32>) -> WidgetQuery {
    WidgetQuery {
        source: "spans".to_owned(),
        measures: measures
            .iter()
            .map(|(m, a)| MeasureRequest {
                measure: (*m).to_owned(),
                aggregation: (*a).to_owned(),
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

/// A widget of `kind` that is valid wherever its rectangle is.
fn valid(kind: WidgetKind, layout: WidgetLayout) -> WidgetInput {
    let query = match kind {
        WidgetKind::Markdown => None,
        WidgetKind::Bar => Some(spans_query(&[("count", "sum")], &["model"], Some(5))),
        _ => Some(spans_query(&[("count", "sum")], &[], None)),
    };
    WidgetInput {
        id: None,
        query,
        display: WidgetDisplay {
            kind,
            title: "A widget".to_owned(),
            chart: None,
            text: (kind == WidgetKind::Markdown).then(|| "Some *text*".to_owned()),
        },
        layout,
    }
}

const fn at(x: u32, y: u32, w: u32, h: u32) -> WidgetLayout {
    WidgetLayout { x, y, w, h }
}

fn check(inputs: &[WidgetInput]) -> Vec<(String, String)> {
    let places: Vec<String> = (0..inputs.len()).map(|i| format!("widgets/{i}/")).collect();
    let drafts: Vec<Draft<'_>> = inputs
        .iter()
        .zip(&places)
        .map(|(input, place)| Draft {
            at: Some(place),
            query: input.query.as_ref(),
            display: &input.display,
            layout: input.layout,
        })
        .collect();
    let mut found: Vec<(String, String)> = check_widgets(rid(), &drafts, "widgets")
        .expect("no problem other than validation")
        .into_iter()
        .map(|e| (e.pointer, e.code))
        .collect();
    found.sort();
    found
}

fn pair(pointer: &str, code: &str) -> (String, String) {
    (pointer.to_owned(), code.to_owned())
}

#[test]
fn every_listed_starter_keeps_the_rules() {
    let starters = starters();
    assert!(
        starters.len() >= 4,
        "the four span starters are always listed"
    );
    let mut keys = BTreeSet::new();
    for dashboard in &starters {
        let key = dashboard.builtin_key.clone().expect("a starter has a key");
        assert_eq!(dashboard.id, key);
        assert!(keys.insert(key.clone()), "{key} is listed twice");
        assert!(
            DashboardId::parse(&key).is_err(),
            "{key} reads as a dashboard id"
        );
        assert!(
            check_name(&dashboard.name).is_none()
                && check_description(&dashboard.description).is_none()
        );
        let inputs: Vec<WidgetInput> = dashboard
            .widgets
            .iter()
            .map(|w| WidgetInput {
                id: None,
                query: w.query.clone(),
                display: w.display.clone(),
                layout: w.layout,
            })
            .collect();
        assert_eq!(check(&inputs), Vec::new(), "{key} breaks a rule");
        let ids: BTreeSet<&str> = dashboard.widgets.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids.len(),
            dashboard.widgets.len(),
            "{key} repeats a widget id"
        );
        let mut sorted = dashboard.widgets.clone();
        sorted.sort_by(reading_order);
        assert_eq!(sorted, dashboard.widgets, "{key} is not in reading order");
    }
}

// The MCP starter reads `mcp_ops`, which is listed only once the registry compiles that source.
#[test]
fn a_starter_is_listed_when_its_sources_have_metrics() {
    let mcp = all()
        .into_iter()
        .find(|d| d.builtin_key.as_deref() == Some("mcp"))
        .expect("the MCP starter");
    assert_eq!(listed(&mcp), SourceSchema::of(Source::McpOps).is_some());
    assert_eq!(starter("mcp").is_some(), listed(&mcp));
    assert!(starter("overview").is_some());
    assert!(starter("nope").is_none());
}

#[test]
fn display_and_query_follow_the_kind() {
    let mut markdown_with_query = valid(WidgetKind::Markdown, at(0, 0, 4, 2));
    markdown_with_query.query = Some(spans_query(&[("count", "sum")], &[], None));
    let mut stat_without_query = valid(WidgetKind::Stat, at(4, 0, 4, 2));
    stat_without_query.query = None;
    let mut bar_with_chart = valid(WidgetKind::Bar, at(8, 0, 4, 2));
    bar_with_chart.display.chart = Some(ChartForm::Line);
    let mut untitled = valid(WidgetKind::Table, at(0, 2, 6, 3));
    untitled.display.title = "  ".to_owned();
    let mut table_with_text = valid(WidgetKind::Table, at(6, 2, 6, 3));
    table_with_text.display.text = Some("x".to_owned());
    let mut markdown_without_text = valid(WidgetKind::Markdown, at(0, 5, 4, 2));
    markdown_without_text.display.text = None;
    assert_eq!(
        check(&[
            markdown_with_query,
            stat_without_query,
            bar_with_chart,
            untitled,
            table_with_text,
            markdown_without_text,
        ]),
        vec![
            pair("/widgets/0/query", "invalid"),
            pair("/widgets/1/query", "required"),
            pair("/widgets/2/display/chart", "invalid"),
            pair("/widgets/3/display/title", "invalid"),
            pair("/widgets/4/display/text", "invalid"),
            pair("/widgets/5/display/text", "required"),
        ]
    );
}

#[test]
fn a_query_the_registry_refuses_points_into_the_widget() {
    let mut unknown = valid(WidgetKind::Timeseries, at(0, 0, 6, 3));
    unknown.query = Some(spans_query(&[("nope", "sum")], &[], None));
    let mut percentile_sum = valid(WidgetKind::Timeseries, at(6, 0, 6, 3));
    percentile_sum.query = Some(spans_query(&[("duration", "sum")], &[], None));
    assert_eq!(
        check(&[unknown, percentile_sum]),
        vec![
            pair("/widgets/0/query/measures/0/measure", "invalid"),
            pair("/widgets/1/query/measures", "invalid"),
        ]
    );
}

#[test]
fn a_chart_draws_at_most_four_series() {
    let chart = |query: WidgetQuery| {
        let mut widget = valid(WidgetKind::Timeseries, at(0, 0, 6, 3));
        widget.query = Some(query);
        check(&[widget])
    };
    // Three keys and `other` of an additive measure; four keys of a percentile, which has no `other`.
    assert!(chart(spans_query(&[("count", "sum")], &["model"], Some(3))).is_empty());
    assert!(chart(spans_query(&[("duration", "p95")], &["model"], Some(4))).is_empty());
    assert_eq!(
        chart(spans_query(&[("count", "sum")], &["model"], Some(4))),
        vec![pair("/widgets/0/query/top_n", "invalid")]
    );
    // Without `top_n`, a split chart asks for 20 keys.
    assert_eq!(
        chart(spans_query(&[("count", "sum")], &["model"], None)),
        vec![pair("/widgets/0/query/top_n", "invalid")]
    );
}

#[test]
fn a_dashboard_holds_at_most_fifty_widgets() {
    let widgets: Vec<WidgetInput> = (0..51)
        .map(|i| valid(WidgetKind::Stat, at(0, i * 2, 2, 2)))
        .collect();
    assert_eq!(check(&widgets), vec![pair("/widgets", "too_many")]);
    assert!(check(&widgets[..50]).is_empty());
}

#[test]
fn a_stored_widget_is_not_checked_again_and_takes_no_blame() {
    let stored = valid(WidgetKind::Stat, at(0, 0, 4, 2));
    let mut broken = stored.clone();
    broken.display.title = String::new();
    let new = valid(WidgetKind::Stat, at(2, 1, 4, 2));
    let drafts = [
        Draft {
            at: None,
            query: broken.query.as_ref(),
            display: &broken.display,
            layout: broken.layout,
        },
        Draft {
            at: Some(""),
            query: new.query.as_ref(),
            display: &new.display,
            layout: new.layout,
        },
    ];
    let errors = check_widgets(rid(), &drafts, "").expect("validation only");
    let found: Vec<_> = errors.into_iter().map(|e| (e.pointer, e.code)).collect();
    assert_eq!(found, vec![pair("/layout", "overlaps")]);
}

fn kind() -> impl Strategy<Value = WidgetKind> {
    prop_oneof![
        Just(WidgetKind::Timeseries),
        Just(WidgetKind::Stat),
        Just(WidgetKind::Table),
        Just(WidgetKind::Bar),
        Just(WidgetKind::Markdown),
    ]
}

/// Rectangles around the grid's top-left corner, some outside it.
fn rect() -> impl Strategy<Value = WidgetLayout> {
    (0u32..14, 0u32..20, 0u32..15, 0u32..15).prop_map(|(x, y, w, h)| at(x, y, w, h))
}

/// The spans source's measures, each with an aggregation it allows.
fn measure() -> impl Strategy<Value = (MeasureId, &'static str)> {
    let measures: Vec<MeasureId> = MeasureId::ALL
        .into_iter()
        .filter(|m| m.source() == Source::Spans)
        .collect();
    proptest::sample::select(measures).prop_flat_map(|m| {
        let names: Vec<&'static str> = m.aggregations().iter().map(|a| a.as_str()).collect();
        proptest::sample::select(names).prop_map(move |a| (m, a))
    })
}

proptest! {
    // The rules on rectangles, against an independent model: the cells each widget covers on a grid larger
    // than every rectangle generated. A widget fits when its size suits its kind and it lies inside the 12
    // columns; it overlaps when a cell it covers is covered by an earlier widget.
    #[test]
    fn layouts_are_refused_exactly_when_they_leave_the_grid_or_overlap(
        widgets in proptest::collection::vec((kind(), rect()), 0..8),
    ) {
        let inputs: Vec<WidgetInput> = widgets.iter().map(|(k, r)| valid(*k, *r)).collect();
        let mut owner = vec![vec![None::<usize>; 40]; 40];
        let mut expected = Vec::new();
        for (i, (kind, r)) in widgets.iter().enumerate() {
            let (min_w, min_h) = min_size(*kind);
            let fits = (min_w..=12).contains(&r.w) && (min_h..=12).contains(&r.h) && r.x + r.w <= 12;
            if !fits {
                expected.push(pair(&format!("/widgets/{i}/layout"), "invalid"));
            }
            let mut overlapping = false;
            for x in r.x..r.x + r.w {
                for y in r.y..r.y + r.h {
                    let cell = &mut owner[x as usize][y as usize];
                    overlapping |= cell.is_some();
                    cell.get_or_insert(i);
                }
            }
            if overlapping {
                expected.push(pair(&format!("/widgets/{i}/layout"), "overlaps"));
            }
        }
        expected.sort();
        prop_assert_eq!(check(&inputs), expected);
    }

    // The shape a kind takes, for every query the registry accepts: a stat is one measure without
    // dimensions, a bar one measure with dimensions, a chart at most four series (each measure with each key,
    // and `other` for an additive measure), and a table anything.
    #[test]
    fn a_kind_takes_exactly_its_shapes(
        kind in prop_oneof![
            Just(WidgetKind::Timeseries),
            Just(WidgetKind::Stat),
            Just(WidgetKind::Table),
            Just(WidgetKind::Bar),
        ],
        measures in proptest::collection::vec(measure(), 1..=4),
        dimensions in proptest::sample::subsequence(vec!["environment", "model", "level"], 0..=2),
        top_n in proptest::option::of(1u32..=20),
    ) {
        let names: Vec<(&str, &str)> = measures.iter().map(|(m, a)| (m.as_str(), *a)).collect();
        let mut widget = valid(kind, at(0, 0, 6, 3));
        widget.query = Some(spans_query(&names, &dimensions, top_n));
        let (m, d) = (measures.len(), dimensions.len());
        let series: u32 = if d == 0 {
            m as u32
        } else {
            measures.iter().map(|(measure, _)| top_n.unwrap_or(20) + u32::from(measure.additive())).sum()
        };
        let mut expected = Vec::new();
        match kind {
            WidgetKind::Stat => {
                if m != 1 { expected.push(pair("/widgets/0/query/measures", "invalid")); }
                if d != 0 { expected.push(pair("/widgets/0/query/dimensions", "invalid")); }
            }
            WidgetKind::Bar => {
                if m != 1 { expected.push(pair("/widgets/0/query/measures", "invalid")); }
                if d == 0 { expected.push(pair("/widgets/0/query/dimensions", "required")); }
            }
            WidgetKind::Timeseries if series > 4 => {
                let member = if d == 0 { "measures" } else { "top_n" };
                expected.push(pair(&format!("/widgets/0/query/{member}"), "invalid"));
            }
            _ => {}
        }
        expected.sort();
        prop_assert_eq!(check(&[widget]), expected);
    }

    // What the store keeps reads back as what was written.
    #[test]
    fn a_widget_reads_back_from_the_store(
        kind in kind(),
        r in rect(),
        title in "[a-zA-Z ]{1,20}",
        chart in proptest::option::of(prop_oneof![Just(ChartForm::Line), Just(ChartForm::Bar)]),
        id in any::<[u8; 16]>(),
    ) {
        let mut widget = valid(kind, r);
        widget.display.title = title;
        widget.display.chart = chart;
        let id = WidgetId::from_bytes(id);
        let stored = stored_widget(id, widget.query.as_ref(), &widget.display, widget.layout);
        let read = wire_widget(&stored).expect("reads back");
        prop_assert_eq!(read.id, id.to_string());
        prop_assert_eq!(read.query, widget.query);
        prop_assert_eq!(read.display, widget.display);
        prop_assert_eq!(read.layout, widget.layout);
    }
}
