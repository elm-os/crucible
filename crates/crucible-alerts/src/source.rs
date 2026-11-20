//! Where an evaluation reads its series from: the metrics registry, through a typed plan of class `alert`
//! over the minute rollup of its source: `span_metrics`, or `mcp_tool_rollup` under the rule's side and capture
//! source, without the calls the caller caused (contract 9.2).

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use crucible_control::Project;
use crucible_plan::{Caller, Source};
use crucible_query::{
    McpMetricsRequest, MetricValue, QueryService, ReadContext, RetentionSettings,
};
use crucible_records::TimeWindow;

use crate::spec::{MAX_SERIES, RuleSpec};

/// What an evaluation saw.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Observed {
    /// Each series that had rows: its key and its value in the measure's stored unit. A series whose
    /// percentile has no observation is absent, like a series without rows.
    pub series: Vec<(String, i64)>,
    /// Whether the rule has more series than [`MAX_SERIES`]; the ones nearest to a breach were kept.
    pub truncated: bool,
}

/// Why an evaluation could not read.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("{0}")]
pub struct ReadFailure(pub String);

/// The read of an evaluation.
#[async_trait]
pub trait SeriesSource: fmt::Debug + Send + Sync {
    /// The value of the rule's measure for each series, over `window`.
    async fn read(
        &self,
        project: &Project,
        spec: &RuleSpec,
        window: TimeWindow,
    ) -> Result<Observed, ReadFailure>;
}

/// The key of a series: the JSON array of its dimension values, `[]` for a rule without dimensions.
pub fn series_key(values: &[Arc<str>]) -> String {
    let values: Vec<&str> = values.iter().map(|value| &**value).collect();
    serde_json::to_string(&values).unwrap_or_else(|_| "[]".to_owned())
}

/// The dimension values a series key holds.
pub fn series_values(key: &str) -> Vec<String> {
    serde_json::from_str(key).unwrap_or_default()
}

/// The production source: the query service.
#[derive(Clone, Debug)]
pub struct QuerySource {
    service: QueryService,
}

impl QuerySource {
    /// A source that reads through `service`.
    pub const fn new(service: QueryService) -> Self {
        Self { service }
    }
}

#[async_trait]
impl SeriesSource for QuerySource {
    async fn read(
        &self,
        project: &Project,
        spec: &RuleSpec,
        window: TimeWindow,
    ) -> Result<Observed, ReadFailure> {
        if project.deleted_at.is_some() {
            return Err(ReadFailure("the project is deleted".to_owned()));
        }
        let scope = crucible_auth::stored_project_scope(project)
            .map_err(|_| ReadFailure("the project has no id".to_owned()))?;
        let retention = RetentionSettings {
            retention_days: project.retention_days,
            payload_retention_days: project.payload_retention_days,
            mcp_content_retention_days: project.mcp_content_retention_days,
        };
        let ctx = ReadContext::new(scope, retention, Caller::Session);
        let upward = spec.comparison.upward();
        let totals = match (spec.source, spec.mcp) {
            (Source::McpOps, Some(mcp)) => {
                let request = McpMetricsRequest {
                    window,
                    query: spec.query(),
                    side: mcp.side,
                    source: mcp.source,
                    include_caller_faults: false,
                };
                self.service
                    .mcp_metrics_totals(&ctx, request, upward, MAX_SERIES)
                    .await
            }
            (Source::McpOps, None) => {
                return Err(ReadFailure("the MCP rule names no side".to_owned()));
            }
            _ => {
                self.service
                    .metrics_totals(&ctx, window, &spec.query(), upward, MAX_SERIES)
                    .await
            }
        }
        .map_err(|error| ReadFailure(error.to_string()))?;
        Ok(Observed {
            series: totals
                .series
                .into_iter()
                .filter_map(|series| {
                    let value = match series.value? {
                        MetricValue::UInt(value) => i64::try_from(value).unwrap_or(i64::MAX),
                        MetricValue::Int(value) => value,
                    };
                    Some((series_key(&series.key), value))
                })
                .collect(),
            truncated: totals.truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_key_holds_its_values_whatever_they_contain() {
        assert_eq!(series_key(&[]), "[]");
        let values: Vec<Arc<str>> = vec![Arc::from("prod"), Arc::from("a \"quoted\", value")];
        let key = series_key(&values);
        assert_eq!(series_values(&key), ["prod", "a \"quoted\", value"]);
        assert!(series_values("not json").is_empty());
    }
}
