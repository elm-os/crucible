//! The list filter of a request (contract 8.8): the filter query parameters of the trace list and the span
//! list, parsed into the filter AST of `crucible-plan`. Each parameter is one condition and all of them must
//! hold; a parameter repeated with several values holds when any value matches. Every problem names its
//! parameter (contract 12.3).

use crucible_plan::{Filter, TextField};
use crucible_records::{Level, ObservationType};

use crate::params::Params;

/// The most values one filter parameter takes.
pub(crate) const MAX_FILTER_VALUES: usize = 100;

/// The text fields, by parameter name.
const TEXT_FIELDS: [(&str, TextField); 5] = [
    ("environment", TextField::Environment),
    ("release", TextField::Release),
    ("version", TextField::Version),
    ("user_id", TextField::UserId),
    ("session_id", TextField::SessionId),
];

fn names<T>(all: &[T], name: impl Fn(&T) -> &'static str) -> String {
    all.iter().map(name).collect::<Vec<_>>().join(", ")
}

/// Takes the filter parameters of a list request. Problems are recorded on `params`, which refuses the request
/// when it finishes; the filter returned then holds only the conditions that parsed.
pub(crate) fn list_filter(params: &mut Params) -> Filter {
    let mut conditions = Vec::new();
    let values = |params: &mut Params, name: &str| {
        let values = params.all(name);
        if values.len() > MAX_FILTER_VALUES {
            let message = format!("at most {MAX_FILTER_VALUES} values are accepted");
            params.error(name, "too_many_values", &message);
            return Vec::new();
        }
        values
    };
    for (name, field) in TEXT_FIELDS {
        let given = values(params, name);
        if !given.is_empty() {
            conditions.push(Filter::Text(field, given));
        }
    }
    let levels = values(params, "level");
    let parsed: Option<Vec<Level>> = levels.iter().map(|text| Level::parse(text)).collect();
    match parsed {
        Some(levels) if !levels.is_empty() => conditions.push(Filter::Level(levels)),
        Some(_) => {}
        None => {
            let message = format!("expected one of {}", names(&Level::ALL, Level::as_str));
            params.error("level", "invalid_value", &message);
        }
    }
    let types = values(params, "observation_type");
    let parsed: Option<Vec<ObservationType>> = types
        .iter()
        .map(|text| ObservationType::parse(text))
        .collect();
    match parsed {
        Some(types) if !types.is_empty() => conditions.push(Filter::ObservationType(types)),
        Some(_) => {}
        None => {
            let all = names(&ObservationType::ALL, ObservationType::as_str);
            params.error(
                "observation_type",
                "invalid_value",
                &format!("expected one of {all}"),
            );
        }
    }
    let tags = values(params, "tags");
    if !tags.is_empty() {
        conditions.push(Filter::Tags(tags));
    }
    for entry in values(params, "metadata") {
        match entry.split_once('=') {
            Some((key, value)) if !key.is_empty() => conditions.push(Filter::Metadata {
                key: key.to_owned(),
                value: value.to_owned(),
            }),
            _ => params.error(
                "metadata",
                "invalid_value",
                "expected `key=value` with a non-empty key",
            ),
        }
    }
    if let Some(text) = params.optional("name") {
        conditions.push(Filter::Name(text));
    }
    Filter::And(conditions)
}
