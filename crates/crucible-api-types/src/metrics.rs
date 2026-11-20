//! Metrics: `POST projects/{project_id}/metrics/query`, `GET .../metrics/schema` and `GET .../filter-values`
//! (contract 8.8 and 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::meta::Meta;

/// The bucket width of a metrics query.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum Granularity {
    /// The finest width that keeps the answer inside the caps.
    Auto,
    /// One minute.
    Minute,
    /// Five minutes.
    FiveMinutes,
    /// Fifteen minutes.
    FifteenMinutes,
    /// One hour.
    Hour,
    /// One day.
    Day,
    /// One week, from Monday.
    Week,
}

/// What an empty bucket holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum FillMode {
    /// Every bucket of the window is returned: 0 for a sum, null for a percentile.
    Zero,
    /// Empty buckets are left out.
    None,
}

/// A filter over a source's filter fields: conditions combined with `and`, `or` and `not`. The fields and
/// the operator each takes are in the metrics schema.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum MetricsFilter {
    /// Every operand holds.
    #[schema(no_recursion)]
    And(Vec<MetricsFilter>),
    /// Any operand holds.
    #[schema(no_recursion)]
    Or(Vec<MetricsFilter>),
    /// The operand does not hold.
    #[schema(no_recursion)]
    Not(Box<MetricsFilter>),
    /// The field is one of the values (operator `in`).
    In {
        /// The field.
        field: String,
        /// The values.
        values: Vec<String>,
    },
}

/// One measure of a query and how it is aggregated.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MeasureRequest {
    /// A measure id of the source in the metrics schema.
    pub measure: String,
    /// An aggregation the measure allows: `sum`, or a percentile `p50`, `p90`, `p95`, `p99`.
    pub aggregation: String,
}

/// The body of `POST projects/{project_id}/metrics/query` (`MetricsQuery` v1).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct MetricsQueryRequest {
    /// The source: `spans` or `traces`.
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
    /// Start of the window, inclusive (RFC 3339).
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the window, exclusive (RFC 3339). At most the project's maximum window.
    #[schema(format = DateTime)]
    pub to: String,
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

/// One dimension value of a series key.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SeriesKey {
    /// The dimension id.
    pub dimension: String,
    /// The value.
    pub value: String,
}

/// One bucket of a series.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MetricsPoint {
    /// The first instant of the bucket.
    #[schema(format = DateTime)]
    pub bucket: String,
    /// The value in the measure's unit; null for a percentile of a bucket without spans.
    #[schema(required = true)]
    pub value: Option<f64>,
}

/// One series: one measure of one dimension key, or of the `other` key.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MetricsSeries {
    /// The measure id.
    pub measure: String,
    /// The aggregation.
    pub aggregation: String,
    /// The unit of the values: `count`, `tokens`, `usd`, `us` or `ppm` (parts per million, a rate).
    pub unit: String,
    /// The dimension values of the key; empty without dimensions and for the `other` series.
    pub key: Vec<SeriesKey>,
    /// Whether this series sums every key beyond `top_n` (additive measures only).
    pub other: bool,
    /// The buckets in time order.
    pub points: Vec<MetricsPoint>,
}

/// The answer of `POST projects/{project_id}/metrics/query`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MetricsResponse {
    /// The series.
    pub series: Vec<MetricsSeries>,
    /// The granularity used.
    pub granularity: Granularity,
    /// Whether keys beyond `top_n` were left out of a percentile measure, which has no `other` series.
    pub truncated: bool,
    /// Where the measures were read: `rollup` or `core`.
    pub source_tier: String,
    /// The instant the answer describes.
    #[schema(format = DateTime)]
    pub as_of: String,
    /// Whether the last bucket ends after `as_of` and may still grow.
    pub partial_last_bucket: bool,
    /// Whether a value above saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}

/// A measure of the metrics schema.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SchemaMeasure {
    /// The measure id.
    pub id: String,
    /// The label.
    pub label: String,
    /// The unit of its values: `count`, `tokens`, `usd`, `us` or `ppm` (parts per million, a rate).
    pub unit: String,
    /// The aggregations it allows.
    pub aggregations: Vec<String>,
    /// Whether the values of two keys add up to the value of both: true for sums.
    pub additive: bool,
    /// Where it is read: `rollup` or `core`.
    pub tier: String,
}

/// A dimension of the metrics schema.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SchemaDimension {
    /// The dimension id.
    pub id: String,
    /// `string`, or `enum` for a closed set of names.
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub dimension_type: String,
    /// Whether the dimension can take many values.
    pub high_cardinality: bool,
}

/// A filter field of the metrics schema.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SchemaFilterField {
    /// The field.
    pub field: String,
    /// The operator its condition takes.
    pub operator: String,
}

/// The caps of a source's queries.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SchemaLimits {
    /// The most buckets per series.
    pub max_buckets: u32,
    /// The most dimension keys; the rest form the `other` series of additive measures.
    pub max_series: u32,
    /// The most points in one answer.
    pub max_points: u32,
    /// The most measures of one query.
    pub max_measures: u32,
    /// The most dimensions of one query.
    pub max_dimensions: u32,
}

/// What can be queried of one source.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct SourceSchema {
    /// The source.
    pub source: String,
    /// Its measures.
    pub measures: Vec<SchemaMeasure>,
    /// Its dimensions.
    pub dimensions: Vec<SchemaDimension>,
    /// Its filter fields.
    pub filter_fields: Vec<SchemaFilterField>,
    /// The explicit granularities, finest first; `auto` picks among them.
    pub granularities: Vec<Granularity>,
    /// The caps.
    pub limits: SchemaLimits,
}

/// The answer of `GET projects/{project_id}/metrics/schema`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct MetricsSchema {
    /// One entry per source with metrics.
    pub sources: Vec<SourceSchema>,
}

/// The query of `GET projects/{project_id}/filter-values`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct FilterValuesParams {
    /// The source whose field is looked up.
    pub source: String,
    /// A filter field of the source.
    pub field: String,
    /// Only values that start with this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub prefix: Option<String>,
    /// Start of the window, inclusive (RFC 3339).
    #[param(format = DateTime)]
    pub from: String,
    /// End of the window, exclusive (RFC 3339).
    #[param(format = DateTime)]
    pub to: String,
    /// How many values: 1 to 100, default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(minimum = 1, maximum = 100)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

/// One value of a filter field.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct FilterValue {
    /// The value.
    pub value: String,
    /// How many spans carry it in the window.
    pub count: crate::wire::SafeU64,
}

/// The answer of `GET projects/{project_id}/filter-values`: the most frequent values first.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct FilterValues {
    /// The values.
    pub data: Vec<FilterValue>,
    /// Whether more values exist than `data` holds.
    pub truncated: bool,
    /// Whether a count saturated at 2^53 - 1.
    pub saturated: bool,
    /// How complete and how fresh the answer is.
    pub meta: Meta,
}
