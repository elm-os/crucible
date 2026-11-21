//! The list filter (contract 8.8): the conditions of the trace list, the span list, the search and the MCP
//! server's list tools, as one DTO, `crucible_api_types::ListFilter`, turned into the filter AST of
//! `crucible-plan` by one function, [`plan_filter`]. Each member is one condition and all of them must hold; a
//! member with several values holds when any value matches. Every problem names its member (contract 12.3),
//! under the pointer its caller gives the filter: a query parameter of a list, `/filters/...` in a search body,
//! `filter/...` in an MCP tool's arguments.

use crucible_api_types::{self as wire, ListFilter};
use crucible_plan::{Filter, TextField};
use crucible_records::{Level, ObservationType};

use crate::params::Params;

/// The most values one filter member takes.
pub const MAX_FILTER_VALUES: usize = 100;

/// A refused member of a list filter.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilterProblem {
    /// The member, such as `level`.
    pub member: &'static str,
    /// The problem code: `too_many_values` or `invalid_value`.
    pub code: &'static str,
    /// What to change.
    pub message: String,
}

fn names<T>(all: &[T], name: impl Fn(&T) -> &'static str) -> String {
    all.iter().map(name).collect::<Vec<_>>().join(", ")
}

fn invalid(member: &'static str, message: String) -> FilterProblem {
    FilterProblem {
        member,
        code: "invalid_value",
        message,
    }
}

/// The levels written as `values`, for a filter given as text (query parameters, an MCP tool's arguments).
pub fn parse_levels(values: &[String]) -> Result<Vec<wire::Level>, FilterProblem> {
    values
        .iter()
        .map(|text| Level::parse(text).map(wire::Level::from))
        .collect::<Option<_>>()
        .ok_or_else(|| {
            let all = names(Level::ALL, Level::as_str);
            invalid("level", format!("expected one of {all}"))
        })
}

/// The observation types written as `values`, for a filter given as text.
pub fn parse_observation_types(
    values: &[String],
) -> Result<Vec<wire::ObservationType>, FilterProblem> {
    values
        .iter()
        .map(|text| ObservationType::parse(text).map(wire::ObservationType::from))
        .collect::<Option<_>>()
        .ok_or_else(|| {
            let all = names(ObservationType::ALL, ObservationType::as_str);
            invalid("observation_type", format!("expected one of {all}"))
        })
}

/// The filter AST of a list filter: one condition per member that holds values, in a fixed order. A member with
/// more than [`MAX_FILTER_VALUES`] values, or a metadata entry without `key=value`, is a problem; every problem
/// is reported at once.
pub fn plan_filter(filter: &ListFilter) -> Result<Filter, Vec<FilterProblem>> {
    let mut problems = Vec::new();
    let mut conditions = Vec::new();
    // The values of a member that holds some and not too many.
    let mut held = |member: &'static str, len: usize| {
        if len > MAX_FILTER_VALUES {
            problems.push(FilterProblem {
                member,
                code: "too_many_values",
                message: format!("at most {MAX_FILTER_VALUES} values are accepted"),
            });
        }
        (1..=MAX_FILTER_VALUES).contains(&len)
    };
    let texts = [
        ("environment", TextField::Environment, &filter.environment),
        ("release", TextField::Release, &filter.release),
        ("version", TextField::Version, &filter.version),
        ("user_id", TextField::UserId, &filter.user_id),
        ("session_id", TextField::SessionId, &filter.session_id),
    ];
    for (member, field, values) in texts {
        if let Some(values) = values.as_ref().filter(|v| held(member, v.len())) {
            conditions.push(Filter::Text(field, values.clone()));
        }
    }
    if let Some(levels) = filter.level.as_ref().filter(|v| held("level", v.len())) {
        conditions.push(Filter::Level(levels.iter().map(|l| (*l).into()).collect()));
    }
    let types = filter.observation_type.as_ref();
    if let Some(types) = types.filter(|v| held("observation_type", v.len())) {
        let types = types.iter().map(|t| (*t).into()).collect();
        conditions.push(Filter::ObservationType(types));
    }
    if let Some(tags) = filter.tags.as_ref().filter(|v| held("tags", v.len())) {
        conditions.push(Filter::Tags(tags.clone()));
    }
    let metadata = filter
        .metadata
        .as_ref()
        .filter(|v| held("metadata", v.len()));
    for entry in metadata.into_iter().flatten() {
        match entry.split_once('=') {
            Some((key, value)) if !key.is_empty() => conditions.push(Filter::Metadata {
                key: key.to_owned(),
                value: value.to_owned(),
            }),
            _ => problems.push(invalid(
                "metadata",
                "expected `key=value` with a non-empty key".to_owned(),
            )),
        }
    }
    if let Some(text) = &filter.name {
        conditions.push(Filter::Name(text.clone()));
    }
    if problems.is_empty() {
        Ok(Filter::And(conditions))
    } else {
        Err(problems)
    }
}

/// Takes the filter parameters of a list request: the members of [`ListFilter`] as query parameters, a list
/// member repeated once per value. Problems are recorded on `params`, which refuses the request when it
/// finishes.
pub(crate) fn list_filter(params: &mut Params) -> Filter {
    let mut text = |name: &str| Some(params.all(name)).filter(|values| !values.is_empty());
    let (environment, release, version) = (text("environment"), text("release"), text("version"));
    let (user_id, session_id) = (text("user_id"), text("session_id"));
    let (tags, metadata) = (text("tags"), text("metadata"));
    let (level, types) = (params.all("level"), params.all("observation_type"));
    let name = params.optional("name");
    let mut problems = Vec::new();
    let level = parse_levels(&level).map_err(|problem| problems.push(problem));
    let types = parse_observation_types(&types).map_err(|problem| problems.push(problem));
    let filter = ListFilter {
        environment,
        release,
        version,
        level: level.ok().filter(|values| !values.is_empty()),
        observation_type: types.ok().filter(|values| !values.is_empty()),
        user_id,
        session_id,
        tags,
        metadata,
        name,
    };
    let planned = plan_filter(&filter).unwrap_or_else(|more| {
        problems.extend(more);
        Filter::all()
    });
    for problem in problems {
        params.error(problem.member, problem.code, &problem.message);
    }
    planned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(n: usize) -> Option<Vec<String>> {
        Some((0..n).map(|i| format!("v{i}")).collect())
    }

    #[test]
    fn each_member_is_one_condition_and_an_empty_filter_keeps_every_row() {
        assert!(plan_filter(&ListFilter::default()).unwrap().is_all());
        let filter = ListFilter {
            environment: Some(vec!["prod".to_owned()]),
            level: Some(vec![wire::Level::Error, wire::Level::Warning]),
            observation_type: Some(vec![wire::ObservationType::Tool]),
            tags: Some(vec!["vip".to_owned()]),
            metadata: Some(vec!["team=a=b".to_owned()]),
            name: Some("Checkout".to_owned()),
            ..ListFilter::default()
        };
        assert_eq!(
            plan_filter(&filter).unwrap(),
            Filter::And(vec![
                Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
                Filter::Level(vec![Level::Error, Level::Warning]),
                Filter::ObservationType(vec![ObservationType::Tool]),
                Filter::Tags(vec!["vip".to_owned()]),
                Filter::Metadata {
                    key: "team".to_owned(),
                    value: "a=b".to_owned(),
                },
                Filter::Name("Checkout".to_owned()),
            ])
        );
    }

    #[test]
    fn every_problem_names_its_member() {
        let filter = ListFilter {
            release: values(101),
            tags: values(101),
            metadata: Some(vec!["=x".to_owned(), "plain".to_owned()]),
            level: Some(vec![wire::Level::Debug; 101]),
            ..ListFilter::default()
        };
        let problems: Vec<(&str, &str)> = plan_filter(&filter)
            .unwrap_err()
            .iter()
            .map(|p| (p.member, p.code))
            .collect();
        assert_eq!(
            problems,
            [
                ("release", "too_many_values"),
                ("level", "too_many_values"),
                ("tags", "too_many_values"),
                ("metadata", "invalid_value"),
                ("metadata", "invalid_value"),
            ]
        );
        assert_eq!(
            parse_levels(&["loud".to_owned()]).unwrap_err().message,
            "expected one of debug, default, warning, error"
        );
        assert_eq!(
            parse_observation_types(&["tool".to_owned(), "span".to_owned()]),
            Ok(vec![
                wire::ObservationType::Tool,
                wire::ObservationType::Span
            ])
        );
    }
}
