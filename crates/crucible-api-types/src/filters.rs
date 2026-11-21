//! The list filter: the conditions that `GET traces` and `GET spans` take as query parameters, the MCP server's
//! `list_traces` and `list_spans` take as their `filter`, and `POST search` takes as its `filters` (contracts 8.8
//! and 12.5).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::ToSchema;

use crate::enums::{Level, ObservationType};

/// Conditions on the rows of a list or of a search. Each member is one condition and all of them must hold; a
/// member with several values holds when any of them matches, and takes at most 100 values. Each condition tests
/// the span's own value, except on the trace list, whose parameters say what they test.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ListFilter {
    /// Environments, such as `production`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub environment: Option<Vec<String>>,
    /// Releases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub release: Option<Vec<String>>,
    /// Versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub version: Option<Vec<String>>,
    /// Levels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub level: Option<Vec<Level>>,
    /// Observation types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub observation_type: Option<Vec<ObservationType>>,
    /// User ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub user_id: Option<Vec<String>>,
    /// Session ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub session_id: Option<Vec<String>>,
    /// Tags; a row with any of them matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tags: Option<Vec<String>>,
    /// Metadata entries as `key=value`, split at the first `=`; each entry must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub metadata: Option<Vec<String>>,
    /// Text the name contains, without regard to ASCII case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use utoipa::openapi::path::Parameter;
    use utoipa::openapi::{RefOr, Schema};
    use utoipa::{IntoParams, PartialSchema as _};

    use super::*;
    use crate::{SpanListParams, TraceListParams};

    fn members() -> BTreeSet<String> {
        match ListFilter::schema() {
            RefOr::T(Schema::Object(object)) => object.properties.keys().cloned().collect(),
            _ => panic!("not an object schema"),
        }
    }

    fn names(params: Vec<Parameter>) -> BTreeSet<String> {
        params.into_iter().map(|p| p.name).collect()
    }

    // The lists take the filter as query parameters, so a member added to one must be added to the others.
    #[test]
    fn the_lists_take_every_member_of_the_filter_as_a_parameter() {
        assert_eq!(members().len(), 10);
        let paging = ["from", "to", "limit", "cursor"].map(str::to_owned);
        let mut spans = names(SpanListParams::into_params(|| None));
        for name in &paging {
            assert!(spans.remove(name), "{name}");
        }
        assert_eq!(spans, members());
        let mut traces = names(TraceListParams::into_params(|| None));
        for name in paging.iter().chain(&["since".to_owned()]) {
            assert!(traces.remove(name), "{name}");
        }
        assert_eq!(traces, members());
    }
}
