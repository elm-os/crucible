//! The list filter of `list_traces` and `list_spans` (contract 8.8 and 12.5): the filter parameters of the REST
//! trace and span lists as one `filter` argument, turned into the REST lists' filter DTO and parsed into the
//! filter AST of `crucible-plan` by the REST API's one conversion. Each member is one condition and all of them
//! must hold; a member with several values holds when any value matches.

use crucible_api::{FilterProblem, parse_levels, parse_observation_types, plan_filter};
use crucible_api_types as wire;
use crucible_plan::{Filter, FilterField, TRACE_LIST_MAX_TAGS};
use crucible_query::QueryServiceError;
use crucible_records::RequestId;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::{ToolError, field_error};
use crate::tools::Check;

/// Conditions on the rows of a list. Each member is one condition and all of them must hold; a member with
/// several values holds when any of them matches.
#[derive(Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListFilter {
    /// Environments, such as `production`.
    #[serde(default)]
    environment: Vec<String>,
    /// Releases.
    #[serde(default)]
    release: Vec<String>,
    /// Versions.
    #[serde(default)]
    version: Vec<String>,
    /// User ids.
    #[serde(default)]
    user_id: Vec<String>,
    /// Session ids.
    #[serde(default)]
    session_id: Vec<String>,
    /// Levels: `debug`, `default`, `warning` or `error`.
    #[serde(default)]
    level: Vec<String>,
    /// Observation types: `span`, `generation`, `event`, `embedding`, `agent`, `tool`, `chain`, `retriever`,
    /// `evaluator` or `guardrail`. On `list_traces` it keeps the traces with a span of one of these types.
    #[serde(default)]
    observation_type: Vec<String>,
    /// Tags; a row with any of them matches.
    #[serde(default)]
    tags: Vec<String>,
    /// Metadata entries as `key=value`, split at the first `=`; each entry must match.
    #[serde(default)]
    metadata: Vec<String>,
    /// Text the name contains, without regard to ASCII case.
    #[serde(default)]
    name: Option<String>,
}

impl ListFilter {
    /// The filter of `list_traces`, which tests at most 20 tags (`TRACE_LIST_MAX_TAGS`).
    pub(crate) fn parse_for_traces(filter: Option<Self>, check: &mut Check) -> Filter {
        if filter
            .as_ref()
            .is_some_and(|filter| filter.tags.len() > TRACE_LIST_MAX_TAGS)
        {
            check.error(
                "filter/tags",
                "too_many_values",
                &format!("list_traces tests at most {TRACE_LIST_MAX_TAGS} tags"),
            );
            return Filter::all();
        }
        Self::parse(filter, check)
    }

    /// The filter, or [`Filter::all`] without one: the REST lists' filter DTO and its one conversion, with each
    /// problem recorded on `check` under the member's pointer.
    pub(crate) fn parse(filter: Option<Self>, check: &mut Check) -> Filter {
        let Some(filter) = filter else {
            return Filter::all();
        };
        let mut report = |problem: FilterProblem| {
            let pointer = format!("filter/{}", problem.member);
            check.error(&pointer, problem.code, &problem.message);
        };
        let level = parse_levels(&filter.level).map_err(&mut report).ok();
        let types = parse_observation_types(&filter.observation_type)
            .map_err(&mut report)
            .ok();
        let some = |values: Vec<String>| Some(values).filter(|values| !values.is_empty());
        let filter = wire::ListFilter {
            environment: some(filter.environment),
            release: some(filter.release),
            version: some(filter.version),
            level: level.filter(|values| !values.is_empty()),
            observation_type: types.filter(|values| !values.is_empty()),
            user_id: some(filter.user_id),
            session_id: some(filter.session_id),
            tags: some(filter.tags),
            metadata: some(filter.metadata),
            name: filter.name,
        };
        plan_filter(&filter).unwrap_or_else(|problems| {
            problems.into_iter().for_each(report);
            Filter::all()
        })
    }
}

/// A failed read of a filtered list. A refusal of one filter member points at that member and says what to
/// change; any other failure is [`ToolError::query`].
pub(crate) fn list_error(error: &QueryServiceError, request_id: RequestId) -> ToolError {
    match error {
        QueryServiceError::Validation(field)
            if FilterField::ALL
                .iter()
                .any(|known| known.as_str() == *field) =>
        {
            // The trace list refuses `metadata` with `observation_type` as the REST list does: code `conflict`
            // (contract 12.5).
            let (code, message) = match *field {
                "metadata" | "observation_type" => (
                    "conflict",
                    "list_traces cannot combine `metadata` with `observation_type`; drop one of them, or use \
                     list_spans, which tests both on each span",
                ),
                _ => ("unsupported", "this list cannot test this member; drop it"),
            };
            ToolError::validation(vec![field_error(&format!("filter/{field}"), code, message)])
        }
        _ => ToolError::query(error, request_id),
    }
}
