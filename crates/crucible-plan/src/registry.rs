//! The metrics registry (contract section 8.8). `crucible-plan` owns it: the sources, the rollup that carries
//! each source's data bounds, and for the sources the metrics compiler covers their measures, dimensions, filter
//! fields, granularities and limits. The metrics schema endpoint and the compiler (`endpoints::metrics`) read
//! this one registry, so the schema cannot describe a query the compiler refuses.
//!
//! The `spans` source is compiled to `span_metrics` (tier `rollup`). The `traces` source counts traces, the
//! root heads of `trace_heads` (tier `core`), without dimensions or filter fields yet. The `mcp_ops` source is
//! compiled to `mcp_tool_rollup` (tier `rollup`, `endpoints::mcp_metrics`) with the alert measures of contract
//! 9.9, one measure per query, split by server or target; a query of it names its side and capture source.

use crucible_records::{ErrorClass, TableId};
use crucible_schema::{Column, ColumnRef, McpToolRollupCol, SpanMetricsCol};

use crate::filter::{FilterField, FilterTarget};
use crate::literal::Literal;
use crate::node::{Measure, Step};
use crate::pred::{CmpOp, Pred};

/// A metrics source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Source {
    /// Spans.
    Spans,
    /// MCP operations.
    McpOps,
    /// Traces.
    Traces,
}

/// The minute rollup of a source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Rollup {
    /// The rollup table.
    pub table: TableId,
    /// Its minute column.
    pub minute: ColumnRef,
}

impl Source {
    /// Every source.
    pub const ALL: [Self; 3] = [Self::Spans, Self::McpOps, Self::Traces];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spans => "spans",
            Self::McpOps => "mcp_ops",
            Self::Traces => "traces",
        }
    }

    /// The source with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }

    /// The minute rollup that covers the source, if one exists. Traces have none.
    pub fn rollup(self) -> Option<Rollup> {
        match self {
            Self::Spans => Some(Rollup {
                table: SpanMetricsCol::TABLE,
                minute: SpanMetricsCol::Minute.column_ref(),
            }),
            Self::McpOps => Some(Rollup {
                table: McpToolRollupCol::TABLE,
                minute: McpToolRollupCol::Minute.column_ref(),
            }),
            Self::Traces => None,
        }
    }
}

/// The most buckets one series of a metrics query holds.
pub const METRICS_MAX_BUCKETS: u64 = 500;
/// The most dimension keys a metrics query returns; the remainder becomes the `other` series for additive
/// measures.
pub const METRICS_MAX_SERIES: u32 = 20;
/// The most points a metrics query returns in total.
pub const METRICS_MAX_POINTS: u64 = 10_000;
/// The most measures of one metrics query.
pub const METRICS_MAX_MEASURES: usize = 4;
/// The most dimensions of one metrics query.
pub const METRICS_MAX_DIMENSIONS: usize = 2;

/// How a measure is aggregated.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Aggregation {
    /// The sum.
    Sum,
    /// The median.
    P50,
    /// The 90th percentile.
    P90,
    /// The 95th percentile.
    P95,
    /// The 99th percentile.
    P99,
    /// The share of the calls that failed, in parts per million: errors over calls.
    Rate,
}

impl Aggregation {
    /// Every aggregation.
    pub const ALL: [Self; 6] = [
        Self::Sum,
        Self::P50,
        Self::P90,
        Self::P95,
        Self::P99,
        Self::Rate,
    ];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::P50 => "p50",
            Self::P90 => "p90",
            Self::P95 => "p95",
            Self::P99 => "p99",
            Self::Rate => "rate",
        }
    }

    /// The aggregation with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == name)
    }

    /// The quantile of a percentile in parts per million, as the histogram kernel of contract 6.5 takes it;
    /// `None` for the sum.
    pub const fn quantile_ppm(self) -> Option<u32> {
        match self {
            Self::Sum | Self::Rate => None,
            Self::P50 => Some(500_000),
            Self::P90 => Some(900_000),
            Self::P95 => Some(950_000),
            Self::P99 => Some(990_000),
        }
    }
}

/// The tier a measure is read from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tier {
    /// A minute rollup.
    Rollup,
    /// The core tier.
    Core,
}

impl Tier {
    /// The wire name (`source_tier`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rollup => "rollup",
            Self::Core => "core",
        }
    }
}

const SUM: &[Aggregation] = &[Aggregation::Sum];
const PERCENTILES: &[Aggregation] = &[
    Aggregation::P50,
    Aggregation::P90,
    Aggregation::P95,
    Aggregation::P99,
];

/// A measure of a source: the `spans` measures, then the `traces` one, then the `mcp_ops` ones (contract 9.9),
/// which read `mcp_tool_rollup` without the calls the caller caused (contract 9.2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum MeasureId {
    /// The number of spans.
    Count,
    /// The number of spans at level `error`.
    ErrorCount,
    /// Input tokens.
    InputTokens,
    /// Output tokens.
    OutputTokens,
    /// Total tokens.
    TotalTokens,
    /// Total cost.
    TotalCost,
    /// Span duration.
    Duration,
    /// Time to first token, of generations.
    Ttft,
    /// The number of traces: root spans, counted from their trace heads.
    Traces,
    /// MCP calls (`mcp.calls`).
    McpCalls,
    /// MCP calls with an error class other than `none` (`mcp.error_count`).
    McpErrorCount,
    /// Failed MCP calls over all MCP calls, in parts per million (`mcp.error_rate`).
    McpErrorRate,
    /// The median MCP call duration (`mcp.duration_p50`).
    McpDurationP50,
    /// The 90th percentile of the MCP call duration (`mcp.duration_p90`).
    McpDurationP90,
    /// The 99th percentile of the MCP call duration (`mcp.duration_p99`).
    McpDurationP99,
}

const RATE: &[Aggregation] = &[Aggregation::Rate];
const P50: &[Aggregation] = &[Aggregation::P50];
const P90: &[Aggregation] = &[Aggregation::P90];
const P99: &[Aggregation] = &[Aggregation::P99];

impl MeasureId {
    /// Every measure.
    pub const ALL: [Self; 15] = [
        Self::Count,
        Self::ErrorCount,
        Self::InputTokens,
        Self::OutputTokens,
        Self::TotalTokens,
        Self::TotalCost,
        Self::Duration,
        Self::Ttft,
        Self::Traces,
        Self::McpCalls,
        Self::McpErrorCount,
        Self::McpErrorRate,
        Self::McpDurationP50,
        Self::McpDurationP90,
        Self::McpDurationP99,
    ];

    /// The source the measure belongs to.
    pub const fn source(self) -> Source {
        match self {
            Self::Traces => Source::Traces,
            Self::McpCalls
            | Self::McpErrorCount
            | Self::McpErrorRate
            | Self::McpDurationP50
            | Self::McpDurationP90
            | Self::McpDurationP99 => Source::McpOps,
            _ => Source::Spans,
        }
    }

    /// The wire id.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::ErrorCount => "error_count",
            Self::InputTokens => "input_tokens",
            Self::OutputTokens => "output_tokens",
            Self::TotalTokens => "total_tokens",
            Self::TotalCost => "total_cost",
            Self::Duration => "duration",
            Self::Ttft => "ttft",
            Self::Traces => "traces",
            Self::McpCalls => "mcp.calls",
            Self::McpErrorCount => "mcp.error_count",
            Self::McpErrorRate => "mcp.error_rate",
            Self::McpDurationP50 => "mcp.duration_p50",
            Self::McpDurationP90 => "mcp.duration_p90",
            Self::McpDurationP99 => "mcp.duration_p99",
        }
    }

    /// The measure with this wire id.
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == id)
    }

    /// The label.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Count => "Spans",
            Self::ErrorCount => "Errors",
            Self::InputTokens => "Input tokens",
            Self::OutputTokens => "Output tokens",
            Self::TotalTokens => "Total tokens",
            Self::TotalCost => "Total cost",
            Self::Duration => "Duration",
            Self::Ttft => "Time to first token",
            Self::Traces => "Traces",
            Self::McpCalls => "MCP calls",
            Self::McpErrorCount => "MCP errors",
            Self::McpErrorRate => "MCP error rate",
            Self::McpDurationP50 => "MCP duration p50",
            Self::McpDurationP90 => "MCP duration p90",
            Self::McpDurationP99 => "MCP duration p99",
        }
    }

    /// The unit of the stored value: `count`, `tokens`, `nusd` (billionths of a US dollar), `us`
    /// (microseconds) or `ppm` (parts per million).
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Count | Self::ErrorCount | Self::Traces | Self::McpCalls | Self::McpErrorCount => {
                "count"
            }
            Self::InputTokens | Self::OutputTokens | Self::TotalTokens => "tokens",
            Self::TotalCost => "nusd",
            Self::Duration
            | Self::Ttft
            | Self::McpDurationP50
            | Self::McpDurationP90
            | Self::McpDurationP99 => "us",
            Self::McpErrorRate => "ppm",
        }
    }

    /// Whether the values of two series add up to the value of their union: true for the sums, false for the
    /// percentiles and the rate.
    pub const fn additive(self) -> bool {
        !matches!(
            self,
            Self::Duration
                | Self::Ttft
                | Self::McpErrorRate
                | Self::McpDurationP50
                | Self::McpDurationP90
                | Self::McpDurationP99
        )
    }

    /// The aggregations the measure allows. An MCP percentile measure is one percentile, as the alert
    /// measures of contract 9.9 name them.
    pub const fn aggregations(self) -> &'static [Aggregation] {
        match self {
            Self::McpErrorRate => RATE,
            Self::McpDurationP50 => P50,
            Self::McpDurationP90 => P90,
            Self::McpDurationP99 => P99,
            _ if self.additive() => SUM,
            _ => PERCENTILES,
        }
    }

    /// The tier the measure is read from.
    pub const fn tier(self) -> Tier {
        match self {
            Self::Traces => Tier::Core,
            _ => Tier::Rollup,
        }
    }

    /// The plan measure that computes it: the sum of a rollup partial, the merge of a rollup histogram, or the
    /// count of the root heads. An MCP measure also has its row condition ([`MeasureId::condition`]), and the
    /// rate's sum is split by error class ([`MeasureId::by_error_class`]).
    pub fn measure(self) -> Measure {
        let column = |c: SpanMetricsCol| c.column_ref();
        match self {
            Self::Count => Measure::Sum(column(SpanMetricsCol::Count)),
            Self::ErrorCount => Measure::Sum(column(SpanMetricsCol::ErrorCount)),
            Self::InputTokens => Measure::Sum(column(SpanMetricsCol::InputTokens)),
            Self::OutputTokens => Measure::Sum(column(SpanMetricsCol::OutputTokens)),
            Self::TotalTokens => Measure::Sum(column(SpanMetricsCol::TotalTokens)),
            Self::TotalCost => Measure::Sum(column(SpanMetricsCol::TotalCostNusd)),
            Self::Duration => Measure::HistogramMerge(column(SpanMetricsCol::DurHist)),
            Self::Ttft => Measure::HistogramMerge(column(SpanMetricsCol::TtftHist)),
            Self::Traces => Measure::Count,
            Self::McpCalls | Self::McpErrorCount | Self::McpErrorRate => {
                Measure::Sum(McpToolRollupCol::Count.column_ref())
            }
            Self::McpDurationP50 | Self::McpDurationP90 | Self::McpDurationP99 => {
                Measure::HistogramMerge(McpToolRollupCol::DurHist.column_ref())
            }
        }
    }

    /// The rows the measure counts beyond those of its query: the classes other than `none` for
    /// `mcp.error_count`.
    pub fn condition(self) -> Option<Pred> {
        match self {
            Self::McpErrorCount => Some(Pred::cmp(
                McpToolRollupCol::ErrorClass,
                CmpOp::Ne,
                Literal::Enum(ErrorClass::None.value()),
            )),
            _ => None,
        }
    }

    /// Whether the measure is read grouped by `error_class` as well and folded: the rate, whose numerator is
    /// the calls of the classes other than `none` and whose denominator is every call.
    pub const fn by_error_class(self) -> bool {
        matches!(self, Self::McpErrorRate)
    }
}

/// The type of a dimension's values.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DimensionType {
    /// A string.
    String,
    /// One of a closed set of names.
    Enum,
}

impl DimensionType {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Enum => "enum",
        }
    }
}

/// A dimension of a source: the `spans` dimensions, then the `mcp_ops` ones.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum DimensionId {
    /// The environment.
    Environment,
    /// The observation type.
    ObservationType,
    /// The response model.
    Model,
    /// The level.
    Level,
    /// The release.
    Release,
    /// The MCP server.
    McpServer,
    /// The MCP tool or prompt name, or resource template.
    McpTarget,
}

impl DimensionId {
    /// Every dimension.
    pub const ALL: [Self; 7] = [
        Self::Environment,
        Self::ObservationType,
        Self::Model,
        Self::Level,
        Self::Release,
        Self::McpServer,
        Self::McpTarget,
    ];

    /// The source the dimension belongs to.
    pub const fn source(self) -> Source {
        match self {
            Self::McpServer | Self::McpTarget => Source::McpOps,
            _ => Source::Spans,
        }
    }

    /// The wire id.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::ObservationType => "observation_type",
            Self::Model => "model",
            Self::Level => "level",
            Self::Release => "release",
            Self::McpServer => "server_name",
            Self::McpTarget => "target_name",
        }
    }

    /// The dimension with this wire id.
    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.as_str() == id)
    }

    /// The type of the dimension's values.
    pub const fn ty(self) -> DimensionType {
        match self {
            Self::ObservationType | Self::Level => DimensionType::Enum,
            Self::Environment | Self::Model | Self::Release | Self::McpServer | Self::McpTarget => {
                DimensionType::String
            }
        }
    }

    /// Whether the dimension can take many values: the user-controlled strings.
    pub const fn high_cardinality(self) -> bool {
        matches!(self, Self::Release)
    }

    /// The rollup key column.
    pub fn column(self) -> ColumnRef {
        match self {
            Self::Environment => SpanMetricsCol::Environment.column_ref(),
            Self::ObservationType => SpanMetricsCol::ObservationType.column_ref(),
            Self::Model => SpanMetricsCol::ModelResponse.column_ref(),
            Self::Level => SpanMetricsCol::Level.column_ref(),
            Self::Release => SpanMetricsCol::Release.column_ref(),
            Self::McpServer => McpToolRollupCol::ServerName.column_ref(),
            Self::McpTarget => McpToolRollupCol::TargetName.column_ref(),
        }
    }
}

/// The most measures of one query of the `mcp_ops` source: each MCP measure counts its own rows.
pub const MCP_METRICS_MAX_MEASURES: usize = 1;

/// The limits of a metrics query.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MetricsLimits {
    /// The most buckets per series.
    pub max_buckets: u64,
    /// The most dimension keys.
    pub max_series: u32,
    /// The most points in total.
    pub max_points: u64,
    /// The most measures.
    pub max_measures: usize,
    /// The most dimensions.
    pub max_dimensions: usize,
}

/// What the metrics schema endpoint returns for one source.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceSchema {
    /// The source.
    pub source: Source,
    /// Its measures.
    pub measures: Vec<MeasureId>,
    /// Its dimensions.
    pub dimensions: Vec<DimensionId>,
    /// The filter fields its queries evaluate; each has the operator [`FilterField::operator`].
    pub filter_fields: Vec<FilterField>,
    /// The explicit granularities, finest first; `auto` picks among them.
    pub granularities: Vec<Step>,
    /// The limits.
    pub limits: MetricsLimits,
}

impl SourceSchema {
    /// The schema of a source the compiler covers, or `None`. A query of `mcp_ops` also names its side and
    /// capture source, outside the filter (contract 9.5).
    pub fn of(source: Source) -> Option<Self> {
        let filter_fields = match source {
            Source::Spans => FilterTarget::SpanMetrics.fields(),
            Source::Traces | Source::McpOps => Vec::new(),
        };
        let dimensions: Vec<DimensionId> = DimensionId::ALL
            .into_iter()
            .filter(|d| d.source() == source)
            .collect();
        let max_measures = match source {
            Source::McpOps => MCP_METRICS_MAX_MEASURES,
            Source::Spans | Source::Traces => METRICS_MAX_MEASURES,
        };
        Some(Self {
            source,
            measures: MeasureId::ALL
                .into_iter()
                .filter(|m| m.source() == source)
                .collect(),
            limits: MetricsLimits {
                max_buckets: METRICS_MAX_BUCKETS,
                max_series: METRICS_MAX_SERIES,
                max_points: METRICS_MAX_POINTS,
                max_measures,
                max_dimensions: dimensions.len().min(METRICS_MAX_DIMENSIONS),
            },
            dimensions,
            filter_fields,
            granularities: Step::ALL.to_vec(),
        })
    }
}

/// The metrics schema: one entry per source the compiler covers, in the order of [`Source::ALL`].
pub fn metrics_schema() -> Vec<SourceSchema> {
    Source::ALL
        .into_iter()
        .filter_map(SourceSchema::of)
        .collect()
}
