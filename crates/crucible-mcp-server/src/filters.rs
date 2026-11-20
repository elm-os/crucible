//! The list filter of `list_traces` and `list_spans` (contract 8.8 and 12.5): the filter parameters of the REST
//! trace and span lists as one `filter` argument, parsed into the filter AST of `crucible-plan`. Each member is
//! one condition and all of them must hold; a member with several values holds when any value matches.

use crucible_plan::{Filter, TextField};
use crucible_records::{Level, ObservationType};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::Check;

/// The most values one filter member takes, as on the REST lists.
const MAX_VALUES: usize = 100;

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

fn names<T>(all: &[T], name: impl Fn(&T) -> &'static str) -> String {
    all.iter().map(name).collect::<Vec<_>>().join(", ")
}

impl ListFilter {
    /// The filter, or [`Filter::all`] without one. Problems are recorded on `check` with the member's pointer.
    pub(crate) fn parse(filter: Option<Self>, check: &mut Check) -> Filter {
        let Some(filter) = filter else {
            return Filter::all();
        };
        let mut conditions = Vec::new();
        let take = |name: &str, values: Vec<String>, check: &mut Check| {
            if values.len() > MAX_VALUES {
                let message = format!("at most {MAX_VALUES} values are accepted");
                check.error(&format!("filter/{name}"), "too_many_values", &message);
                return Vec::new();
            }
            values
        };
        let texts = [
            ("environment", TextField::Environment, filter.environment),
            ("release", TextField::Release, filter.release),
            ("version", TextField::Version, filter.version),
            ("user_id", TextField::UserId, filter.user_id),
            ("session_id", TextField::SessionId, filter.session_id),
        ];
        for (name, field, values) in texts {
            let values = take(name, values, check);
            if !values.is_empty() {
                conditions.push(Filter::Text(field, values));
            }
        }
        let levels = take("level", filter.level, check);
        match levels.iter().map(|text| Level::parse(text)).collect() {
            Some(levels) => {
                let levels: Vec<Level> = levels;
                if !levels.is_empty() {
                    conditions.push(Filter::Level(levels));
                }
            }
            None => check.error(
                "filter/level",
                "invalid_value",
                &format!("expected one of {}", names(Level::ALL, Level::as_str)),
            ),
        }
        let types = take("observation_type", filter.observation_type, check);
        match types
            .iter()
            .map(|text| ObservationType::parse(text))
            .collect()
        {
            Some(types) => {
                let types: Vec<ObservationType> = types;
                if !types.is_empty() {
                    conditions.push(Filter::ObservationType(types));
                }
            }
            None => check.error(
                "filter/observation_type",
                "invalid_value",
                &format!(
                    "expected one of {}",
                    names(ObservationType::ALL, ObservationType::as_str)
                ),
            ),
        }
        let tags = take("tags", filter.tags, check);
        if !tags.is_empty() {
            conditions.push(Filter::Tags(tags));
        }
        for entry in take("metadata", filter.metadata, check) {
            match entry.split_once('=') {
                Some((key, value)) if !key.is_empty() => conditions.push(Filter::Metadata {
                    key: key.to_owned(),
                    value: value.to_owned(),
                }),
                _ => check.error(
                    "filter/metadata",
                    "invalid_value",
                    "expected `key=value` with a non-empty key",
                ),
            }
        }
        if let Some(text) = filter.name {
            conditions.push(Filter::Name(text));
        }
        Filter::And(conditions)
    }
}
