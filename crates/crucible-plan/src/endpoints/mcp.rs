//! What the MCP analytics builders share (contract 9.9): the side, the capture-source selection of contract 9.5
//! and the caller-fault rule of contract 9.2. The builders are in `mcp_rollup` (the aggregating tables, which the
//! 300 ms dashboard target binds) and `mcp_ops` (the raw rows).
//!
//! - **Side.** Every builder reads one side, client or server, as a predicate on the table's first key column
//!   after `project_id`; the two sides are never unioned or summed (contract 3.5).
//! - **Capture sources.** A read first takes, from `mcp_tool_rollup`, which capture sources each server has rows
//!   of in the window ([`mcp_presence`]). [`McpSelection::select`] picks one class per server: the one the
//!   request names, or under `auto` the best-ranked class present (SDK, then proxy, then OTLP). Every data plan
//!   carries the choice as a predicate, so rows of two classes are never summed. The selection always sees the
//!   server's whole side in the window, never rows already narrowed to a tool, a client or a deploy.
//! - **Proxy.** The class `proxy` holds both proxy capture sources, `proxy_stdio` and `proxy_http`. A call
//!   travels over one transport, so the two never observe the same call and their rows add up.
//! - **Caller faults.** Rollup reads group by `caller_fault` and the service keeps those calls apart; raw reads
//!   leave them out unless the request includes them.
//! - **Hourly tier.** A rollup read over a window of at least [`MCP_HOURLY_MIN_WINDOW_HOURS`] hours takes the
//!   window's whole hours from `mcp_tool_rollup_hourly` and only the minutes at its edges from `mcp_tool_rollup`
//!   ([`crate::HourSplit`]), with the same answer: each minute is read from one of the two, and the hourly rows
//!   of an hour merge to what its minute rows merge to.

use crucible_records::{CaptureSource, McpSide, ProjectScope, TimeWindow};
use crucible_schema::{Column, ColumnRef, McpToolRollupCol};

use crate::build::RetentionClamp;
use crate::class::QueryClass;
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{AggOutput, Aggregate, Direction, Measure, Node, Step};
use crate::plan::QueryPlan;
use crate::pred::Pred;
use crate::registry::METRICS_MAX_BUCKETS;

/// The method of a tool call: the rows the tool endpoints read.
pub const TOOLS_CALL: &str = "tools/call";

/// The shortest window, in hours, whose rollup reads take their whole hours from `mcp_tool_rollup_hourly`.
///
/// A window of `w` hours reads at most two partial hours of minute rows at its edges and its whole hours from
/// the tier, which holds one row per key and hour where the minute rollup holds up to sixty, so the tier never
/// reads more rows; what it adds is a second table reference in the statement. Measured on the dense dataset of
/// the MCP dashboard (375 rollup rows a minute on the server side, ClickHouse with two cores; measured on a
/// laptop, not reference hardware), the two reads cost the same at 6 hours, the tiered one is slower below
/// (3 hours: 36 ms against 25 ms for `tools`) and faster above (12 hours: 48 ms against 84 ms; 24 hours: 80 ms
/// against 157 ms). 12 hours keeps a factor of two above that crossover, which grows on sparser data, where a
/// minute read is cheaper, and puts the UI's ranges of 24 hours, 7 days and 30 days on the tier and its one
/// hour on minute rows.
pub const MCP_HOURLY_MIN_WINDOW_HOURS: i64 = 12;

/// The plan of `root`, a read of `mcp_tool_rollup`, over `window` clamped to retention: over the hourly tier
/// for a clamped window of at least [`MCP_HOURLY_MIN_WINDOW_HOURS`] hours, when the tier can serve the read
/// (`Node::hourly`), else over minute rows alone.
pub(crate) fn rollup_plan(
    clamp: &RetentionClamp,
    scope: ProjectScope,
    window: TimeWindow,
    class: QueryClass,
    root: Node,
) -> Result<Option<QueryPlan>, PlanError> {
    let long = clamp
        .window(McpToolRollupCol::TABLE, window)?
        .is_some_and(|clamped| clamped.len_micros() >= MCP_HOURLY_MIN_WINDOW_HOURS * 3_600_000_000);
    let root = if long {
        root.clone().hourly().unwrap_or(root)
    } else {
        root
    };
    clamp.plan(scope, window, class, root)
}

/// A class of capture sources, as the `source` parameter names it (contract 9.5), in order of preference.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CaptureClass {
    /// An SDK wrapper in the server or client.
    Sdk,
    /// The proxy, over stdio or HTTP.
    Proxy,
    /// Plain semantic-convention spans from a server with neither.
    Otlp,
}

impl CaptureClass {
    /// Every class, best first.
    pub const ALL: [Self; 3] = [Self::Sdk, Self::Proxy, Self::Otlp];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sdk => "sdk",
            Self::Proxy => "proxy",
            Self::Otlp => "otlp",
        }
    }

    /// The class with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// The stored capture sources of the class.
    pub const fn sources(self) -> &'static [CaptureSource] {
        match self {
            Self::Sdk => &[CaptureSource::Sdk],
            Self::Proxy => &[CaptureSource::ProxyStdio, CaptureSource::ProxyHttp],
            Self::Otlp => &[CaptureSource::Otlp],
        }
    }

    /// The class of a stored capture source.
    pub const fn of(source: CaptureSource) -> Self {
        match source {
            CaptureSource::Sdk => Self::Sdk,
            CaptureSource::ProxyStdio | CaptureSource::ProxyHttp => Self::Proxy,
            CaptureSource::Otlp => Self::Otlp,
        }
    }

    fn literals(self) -> Vec<Literal> {
        self.sources()
            .iter()
            .map(|s| Literal::Enum(s.value()))
            .collect()
    }
}

/// The `source` parameter of every MCP read: `auto` (the default) or one class.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SourceChoice {
    /// Per server, the best-ranked class that has rows in the window.
    Auto,
    /// This class, for every server.
    Class(CaptureClass),
}

impl SourceChoice {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Class(class) => class.as_str(),
        }
    }

    /// The choice with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            other => CaptureClass::parse(other).map(Self::Class),
        }
    }
}

/// One server of a selection: the classes it has rows of on the side in the window, and the class its rows are
/// read from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServerSources {
    /// The server.
    pub server_name: String,
    /// The classes with rows, best first.
    pub available: Vec<CaptureClass>,
    /// The class read: the request's, or under `auto` the best available. `None` when the request names a class
    /// the server has no rows of.
    pub chosen: Option<CaptureClass>,
}

/// The capture-source class each server is read from, for one side and window (contract 9.5).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct McpSelection {
    choice: SourceChoice,
    servers: Vec<ServerSources>,
}

impl McpSelection {
    /// The selection for the presence rows `(server_name, capture_source)` of one side and window, which
    /// [`mcp_presence`] returns. Rows may repeat and come in any order; servers come out in name order.
    pub fn select(
        choice: SourceChoice,
        presence: impl IntoIterator<Item = (String, CaptureSource)>,
    ) -> Self {
        let mut found: std::collections::BTreeMap<String, Vec<CaptureClass>> =
            std::collections::BTreeMap::new();
        for (server, source) in presence {
            let classes = found.entry(server).or_default();
            let class = CaptureClass::of(source);
            if !classes.contains(&class) {
                classes.push(class);
            }
        }
        let servers = found
            .into_iter()
            .map(|(server_name, mut available)| {
                available.sort_unstable();
                let chosen = match choice {
                    SourceChoice::Auto => available.first().copied(),
                    SourceChoice::Class(class) => available.contains(&class).then_some(class),
                };
                ServerSources {
                    server_name,
                    available,
                    chosen,
                }
            })
            .collect();
        Self { choice, servers }
    }

    /// The choice the selection was made under.
    pub const fn choice(&self) -> SourceChoice {
        self.choice
    }

    /// Every server with rows on the side in the window, in name order.
    pub fn servers(&self) -> &[ServerSources] {
        &self.servers
    }

    /// The class a server is read from, if any.
    pub fn chosen(&self, server_name: &str) -> Option<CaptureClass> {
        self.servers
            .iter()
            .find(|s| s.server_name == server_name)
            .and_then(|s| s.chosen)
    }

    /// Whether no server is read from: the reads that follow have no rows.
    pub fn is_empty(&self) -> bool {
        self.servers.iter().all(|s| s.chosen.is_none())
    }

    /// The rows of the chosen classes, over a table's `server_name` and `capture_source` columns. A named class
    /// is one condition for every server; under `auto` each class keeps the servers that chose it. `None` when
    /// no server is read from.
    fn pred(&self, server: ColumnRef, source: ColumnRef) -> Option<Pred> {
        if self.is_empty() {
            return None;
        }
        if let SourceChoice::Class(class) = self.choice {
            return Some(Pred::In(source, class.literals()));
        }
        let mut parts: Vec<Pred> = CaptureClass::ALL
            .into_iter()
            .filter_map(|class| {
                let names: Vec<Literal> = self
                    .servers
                    .iter()
                    .filter(|s| s.chosen == Some(class))
                    .map(|s| Literal::String(s.server_name.clone()))
                    .collect();
                (!names.is_empty()).then(|| {
                    Pred::And(vec![
                        Pred::In(server, names),
                        Pred::In(source, class.literals()),
                    ])
                })
            })
            .collect();
        Some(match parts.len() {
            1 => parts.swap_remove(0),
            _ => Pred::Or(parts),
        })
    }
}

/// What every MCP read shares: the side, the capture sources, whether caller-fault calls count, and an
/// optional server.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct McpRead<'a> {
    /// The side read: client or server.
    pub side: McpSide,
    /// The capture source of each server.
    pub selection: &'a McpSelection,
    /// Whether caller-fault calls count in the read's measures (contract 9.2; default false).
    pub include_caller_faults: bool,
    /// Only this server.
    pub server: Option<&'a str>,
}

/// `side = <side>` for a side that is client or server; the API never asks for `none`.
pub(crate) fn side_pred(column: impl Into<ColumnRef>, side: McpSide) -> Result<Pred, PlanError> {
    match side {
        McpSide::Client | McpSide::Server => Ok(Pred::eq(column, Literal::Enum(side.value()))),
        McpSide::None => Err(PlanError::UnsupportedFilter {
            field: "side",
            target: "MCP analytics, which read the client or the server side",
        }),
    }
}

impl McpRead<'_> {
    /// The conditions on a table with `side`, `server_name` and `capture_source` columns: the side, the chosen
    /// capture sources and the server. `None` when no server is read from.
    pub(crate) fn conditions(
        &self,
        side: ColumnRef,
        server: ColumnRef,
        source: ColumnRef,
    ) -> Result<Option<Vec<Pred>>, PlanError> {
        let Some(sources) = self.selection.pred(server, source) else {
            return Ok(None);
        };
        let mut parts = vec![side_pred(side, self.side)?, sources];
        if let Some(name) = self.server {
            parts.push(Pred::eq(server, name));
        }
        Ok(Some(parts))
    }
}

/// The step of an MCP time series over `window`: the finest that gives at most [`METRICS_MAX_BUCKETS`]
/// buckets, else a week.
pub fn mcp_series_step(window: TimeWindow) -> Step {
    Step::ALL
        .into_iter()
        .find(|step| step.buckets(window).1 <= METRICS_MAX_BUCKETS)
        .unwrap_or(Step::Week)
}

/// The capture sources of each server on one side in the window (contract 9.5), the input of
/// [`McpSelection::select`]: `server_name`, `capture_source`, `sum(count)` and `max(last_seen)` per pair, in
/// that order, from `mcp_tool_rollup`. With `server`, that server only. It runs in `class`: the class of the
/// read it selects for, `analytics` for an endpoint and `alert` for an alert's evaluation.
pub fn mcp_presence(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    class: QueryClass,
    side: McpSide,
    server: Option<&str>,
) -> Result<Option<QueryPlan>, PlanError> {
    let mut parts = vec![side_pred(McpToolRollupCol::Side, side)?];
    if let Some(name) = server {
        parts.push(Pred::eq(McpToolRollupCol::ServerName, name));
    }
    let aggregate = Aggregate::new(
        McpToolRollupCol::TABLE,
        and(parts),
        [
            McpToolRollupCol::ServerName,
            McpToolRollupCol::CaptureSource,
        ],
        vec![
            Measure::Sum(McpToolRollupCol::Count.column_ref()),
            Measure::Max(McpToolRollupCol::LastSeen.column_ref()),
        ],
    )?
    .ordered(vec![
        (AggOutput::Group(0), Direction::Asc),
        (AggOutput::Group(1), Direction::Asc),
    ])?;
    rollup_plan(clamp, scope, window, class, Node::Aggregate(aggregate))
}

/// The conjunction of `parts`, without the wrapper for one.
pub(crate) fn and(mut parts: Vec<Pred>) -> Pred {
    match parts.len() {
        1 => parts.swap_remove(0),
        _ => Pred::And(parts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(rows: &[(&str, CaptureSource)]) -> Vec<(String, CaptureSource)> {
        rows.iter().map(|(s, c)| ((*s).to_owned(), *c)).collect()
    }

    #[test]
    fn auto_picks_the_best_present_class_per_server() {
        let rows = presence(&[
            ("b", CaptureSource::Otlp),
            ("a", CaptureSource::ProxyHttp),
            ("a", CaptureSource::Sdk),
            ("b", CaptureSource::ProxyStdio),
            ("c", CaptureSource::Otlp),
            ("a", CaptureSource::ProxyStdio),
        ]);
        let selection = McpSelection::select(SourceChoice::Auto, rows.clone());
        let chosen: Vec<(&str, Option<CaptureClass>)> = selection
            .servers()
            .iter()
            .map(|s| (s.server_name.as_str(), s.chosen))
            .collect();
        assert_eq!(
            chosen,
            [
                ("a", Some(CaptureClass::Sdk)),
                ("b", Some(CaptureClass::Proxy)),
                ("c", Some(CaptureClass::Otlp)),
            ]
        );
        assert_eq!(
            selection.servers()[0].available,
            [CaptureClass::Sdk, CaptureClass::Proxy]
        );
        let named = McpSelection::select(SourceChoice::Class(CaptureClass::Otlp), rows);
        assert_eq!(named.chosen("a"), None);
        assert_eq!(named.chosen("b"), Some(CaptureClass::Otlp));
        assert!(!named.is_empty());
        assert!(McpSelection::select(SourceChoice::Auto, Vec::new()).is_empty());
    }

    #[test]
    fn the_predicate_keeps_each_class_to_the_servers_that_chose_it() {
        let server = McpToolRollupCol::ServerName.column_ref();
        let source = McpToolRollupCol::CaptureSource.column_ref();
        let rows = presence(&[("a", CaptureSource::Sdk), ("b", CaptureSource::Otlp)]);
        let auto = McpSelection::select(SourceChoice::Auto, rows.clone());
        let enums = |c: CaptureClass| c.literals();
        assert_eq!(
            auto.pred(server, source),
            Some(Pred::Or(vec![
                Pred::And(vec![
                    Pred::In(server, vec![Literal::String("a".to_owned())]),
                    Pred::In(source, enums(CaptureClass::Sdk)),
                ]),
                Pred::And(vec![
                    Pred::In(server, vec![Literal::String("b".to_owned())]),
                    Pred::In(source, enums(CaptureClass::Otlp)),
                ]),
            ]))
        );
        let named = McpSelection::select(SourceChoice::Class(CaptureClass::Proxy), rows);
        assert_eq!(named.pred(server, source), None);
        let proxied = McpSelection::select(
            SourceChoice::Class(CaptureClass::Proxy),
            presence(&[("a", CaptureSource::ProxyHttp)]),
        );
        assert_eq!(
            proxied.pred(server, source),
            Some(Pred::In(source, enums(CaptureClass::Proxy)))
        );
    }

    #[test]
    fn wire_names_round_trip() {
        for choice in [
            SourceChoice::Auto,
            SourceChoice::Class(CaptureClass::Sdk),
            SourceChoice::Class(CaptureClass::Proxy),
            SourceChoice::Class(CaptureClass::Otlp),
        ] {
            assert_eq!(SourceChoice::parse(choice.as_str()), Some(choice));
        }
        assert_eq!(SourceChoice::parse("proxy_stdio"), None);
        for source in CaptureSource::ALL {
            assert!(CaptureClass::of(*source).sources().contains(source));
        }
    }

    #[test]
    fn the_series_step_fits_the_bucket_cap() {
        let day = 86_400_000_000;
        let window = |len: i64| {
            TimeWindow::new(
                crucible_records::Timestamp::from_micros(0),
                crucible_records::Timestamp::from_micros(len),
            )
            .unwrap()
        };
        assert_eq!(mcp_series_step(window(3_600_000_000)), Step::Minute);
        assert_eq!(mcp_series_step(window(day)), Step::FiveMinutes);
        assert_eq!(mcp_series_step(window(7 * day)), Step::Hour);
        assert_eq!(mcp_series_step(window(30 * day)), Step::Day);
    }
}
