//! Parameter binding: the window parameters `{from}` and `{to}`, the values of an ad hoc run and the declared
//! parameters of a saved query. A value is only ever sent as a `param_*` value, which the server parses by the
//! type the text declares; nothing here builds SQL.

use std::collections::BTreeMap;

use crucible_control::{SavedParam, SqlParamType};
use crucible_env::SqlParam;
use crucible_records::{Timestamp, Uuid};

use crate::lexer::{Checked, ParamUse};

/// The names of the window parameters.
pub const WINDOW_PARAMS: [&str; 2] = ["from", "to"];

/// Why parameters could not be bound. Each says how to fix it.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum BindError {
    /// The text uses a parameter that has no value.
    #[error("The query uses {{{0}}} and no value was given for it. Add a value for \"{0}\".")]
    Missing(String),
    /// A value names a parameter the text does not use.
    #[error(
        "A value was given for \"{0}\", which the query does not use. Remove it, or use {{{0}:Type}} in the query."
    )]
    Unused(String),
    /// A value was given for a window parameter.
    #[error("\"{0}\" is set by the time window. Remove its value and change the window instead.")]
    Reserved(String),
    /// A saved query's text uses a parameter it does not declare.
    #[error(
        "The query uses {{{0}}}, which the saved query does not declare. Declare \"{0}\" as a parameter."
    )]
    Undeclared(String),
    /// A saved query declares a parameter its text does not use.
    #[error(
        "The saved query declares \"{0}\" and its text does not use it. Remove the declaration, or use {{{0}:Type}} in the query."
    )]
    DeclaredUnused(String),
    /// A value does not fit its declared type.
    #[error("The value of \"{name}\" is not {expected}. {fix}")]
    BadValue {
        /// The parameter.
        name: String,
        /// What the type is, in words.
        expected: &'static str,
        /// What to write instead.
        fix: &'static str,
    },
    /// The window is empty or reversed.
    #[error("The time window ends before it starts. Choose a window whose end is after its start.")]
    EmptyWindow,
}

/// A timestamp in the text form ClickHouse parses for the declared type: with microseconds for `DateTime64`,
/// whole seconds for `DateTime`.
fn time_text(at: Timestamp, ty: &str) -> String {
    let text = at.to_string().replace('T', " ");
    let text = text.trim_end_matches('Z');
    if ty.trim_start().starts_with("DateTime64") {
        text.to_owned()
    } else {
        text.split('.').next().unwrap_or(text).to_owned()
    }
}

fn window_value(name: &str, ty: &str, from: Timestamp, to: Timestamp) -> Option<String> {
    match name {
        "from" => Some(time_text(from, ty)),
        "to" => Some(time_text(to, ty)),
        _ => None,
    }
}

/// The parameters of an ad hoc run: the window, and `values` for every other parameter the text uses.
pub fn bind(
    checked: &Checked,
    from: Timestamp,
    to: Timestamp,
    values: &BTreeMap<String, String>,
) -> Result<Vec<SqlParam>, BindError> {
    if from >= to {
        return Err(BindError::EmptyWindow);
    }
    for name in values.keys() {
        if WINDOW_PARAMS.contains(&name.as_str()) {
            return Err(BindError::Reserved(name.clone()));
        }
        if !checked.params.iter().any(|p| p.name == *name) {
            return Err(BindError::Unused(name.clone()));
        }
    }
    checked
        .params
        .iter()
        .map(|ParamUse { name, ty }| {
            let value = match window_value(name, ty, from, to) {
                Some(value) => value,
                None => values
                    .get(name)
                    .cloned()
                    .ok_or_else(|| BindError::Missing(name.clone()))?,
            };
            Ok(SqlParam {
                name: name.clone(),
                value,
            })
        })
        .collect()
}

/// Checks a value against a declared type and returns it in ClickHouse's text form.
fn typed(param: &SavedParam, value: &str) -> Result<String, BindError> {
    let bad = |expected: &'static str, fix: &'static str| BindError::BadValue {
        name: param.name.clone(),
        expected,
        fix,
    };
    let value = value.trim();
    match param.ty {
        SqlParamType::String => Ok(value.to_owned()),
        SqlParamType::Int => value
            .parse::<i64>()
            .map(|n| n.to_string())
            .map_err(|_| bad("a whole number", "Write digits only, for example 42.")),
        SqlParamType::Float => value
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(|n| n.to_string())
            .ok_or_else(|| bad("a number", "Write a number, for example 0.25.")),
        SqlParamType::Bool => match value {
            "true" | "false" => Ok(value.to_owned()),
            _ => Err(bad("true or false", "Write true or false.")),
        },
        SqlParamType::Timestamp => Timestamp::parse_rfc3339(value)
            .map(|at| time_text(at, "DateTime64(6)"))
            .map_err(|_| {
                bad(
                    "a timestamp",
                    "Write an RFC 3339 time, for example 2026-01-31T12:00:00Z.",
                )
            }),
        SqlParamType::Uuid => Uuid::parse(value).map(|id| id.to_string()).map_err(|_| {
            bad(
                "a UUID",
                "Write a UUID, for example 0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b.",
            )
        }),
    }
}

/// Checks that a saved query's declarations and its text agree: every parameter of the text other than the
/// window is declared once, every declaration is used, and every default fits its type.
pub fn check_declarations(checked: &Checked, declared: &[SavedParam]) -> Result<(), BindError> {
    for param in &checked.params {
        let known = WINDOW_PARAMS.contains(&param.name.as_str())
            || declared.iter().any(|d| d.name == param.name);
        if !known {
            return Err(BindError::Undeclared(param.name.clone()));
        }
    }
    for (n, param) in declared.iter().enumerate() {
        if WINDOW_PARAMS.contains(&param.name.as_str()) {
            return Err(BindError::Reserved(param.name.clone()));
        }
        let used = checked.params.iter().any(|p| p.name == param.name);
        let repeated = declared[..n].iter().any(|d| d.name == param.name);
        if !used || repeated {
            return Err(BindError::DeclaredUnused(param.name.clone()));
        }
        if let Some(default) = &param.default {
            typed(param, default)?;
        }
    }
    Ok(())
}

/// The parameters of a saved-query run: the window, and for each declared parameter the given value or its
/// default, checked against its type. Only declared parameters are accepted.
pub fn bind_saved(
    checked: &Checked,
    declared: &[SavedParam],
    from: Timestamp,
    to: Timestamp,
    values: &BTreeMap<String, String>,
) -> Result<Vec<SqlParam>, BindError> {
    check_declarations(checked, declared)?;
    let mut typed_values = BTreeMap::new();
    for name in values.keys() {
        if WINDOW_PARAMS.contains(&name.as_str()) {
            return Err(BindError::Reserved(name.clone()));
        }
        if !declared.iter().any(|d| d.name == *name) {
            return Err(BindError::Unused(name.clone()));
        }
    }
    for param in declared {
        let value = values
            .get(&param.name)
            .or(param.default.as_ref())
            .ok_or_else(|| BindError::Missing(param.name.clone()))?;
        typed_values.insert(param.name.clone(), typed(param, value)?);
    }
    bind(checked, from, to, &typed_values)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::lexer::check;

    const TEXT: &str = "SELECT 1 FROM spans WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime} AND \
                        user_id = {user:String} LIMIT {n:Int64}";

    fn window() -> (Timestamp, Timestamp) {
        (
            Timestamp::parse_rfc3339("2026-11-17T09:30:00.25Z").unwrap(),
            Timestamp::parse_rfc3339("2026-11-18T09:30:00Z").unwrap(),
        )
    }

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn declared() -> Vec<SavedParam> {
        vec![
            SavedParam {
                name: "user".to_owned(),
                ty: SqlParamType::String,
                default: None,
            },
            SavedParam {
                name: "n".to_owned(),
                ty: SqlParamType::Int,
                default: Some("10".to_owned()),
            },
        ]
    }

    #[test]
    fn the_window_is_written_for_the_declared_type() {
        let checked = check(TEXT).unwrap();
        let (from, to) = window();
        let bound = bind(&checked, from, to, &values(&[("user", "u1"), ("n", "5")])).unwrap();
        let pairs: Vec<(&str, &str)> = bound
            .iter()
            .map(|p| (p.name.as_str(), p.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("from", "2026-11-17 09:30:00.250000"),
                ("to", "2026-11-18 09:30:00"),
                ("user", "u1"),
                ("n", "5")
            ]
        );
    }

    #[test]
    fn ad_hoc_values_must_match_the_text() {
        let checked = check(TEXT).unwrap();
        let (from, to) = window();
        assert_eq!(
            bind(&checked, from, to, &values(&[("user", "u1")])),
            Err(BindError::Missing("n".to_owned()))
        );
        assert_eq!(
            bind(
                &checked,
                from,
                to,
                &values(&[("user", "u"), ("n", "1"), ("x", "1")])
            ),
            Err(BindError::Unused("x".to_owned()))
        );
        assert_eq!(
            bind(
                &checked,
                from,
                to,
                &values(&[("user", "u"), ("n", "1"), ("from", "x")])
            ),
            Err(BindError::Reserved("from".to_owned()))
        );
        assert_eq!(
            bind(&checked, to, from, &values(&[("user", "u"), ("n", "1")])),
            Err(BindError::EmptyWindow)
        );
        assert!(
            BindError::Missing("n".to_owned())
                .to_string()
                .contains("{n}")
        );
    }

    #[test]
    fn a_saved_query_takes_declared_parameters_only() {
        let checked = check(TEXT).unwrap();
        let (from, to) = window();
        let bound =
            bind_saved(&checked, &declared(), from, to, &values(&[("user", "u1")])).unwrap();
        assert_eq!(bound[3].value, "10", "the default");
        assert_eq!(
            bind_saved(&checked, &declared(), from, to, &values(&[])),
            Err(BindError::Missing("user".to_owned()))
        );
        assert_eq!(
            bind_saved(
                &checked,
                &declared(),
                from,
                to,
                &values(&[("user", "u"), ("extra", "1")])
            ),
            Err(BindError::Unused("extra".to_owned()))
        );
        assert!(matches!(
            bind_saved(
                &checked,
                &declared(),
                from,
                to,
                &values(&[("user", "u"), ("n", "1; DROP")])
            ),
            Err(BindError::BadValue { .. })
        ));
        // Declarations and text must agree.
        assert_eq!(
            check_declarations(&checked, &declared()[..1]),
            Err(BindError::Undeclared("n".to_owned()))
        );
        let mut extra = declared();
        extra.push(SavedParam {
            name: "ghost".to_owned(),
            ty: SqlParamType::Bool,
            default: None,
        });
        assert_eq!(
            check_declarations(&checked, &extra),
            Err(BindError::DeclaredUnused("ghost".to_owned()))
        );
    }

    #[test]
    fn each_type_checks_its_values() {
        let param = |ty| SavedParam {
            name: "p".to_owned(),
            ty,
            default: None,
        };
        assert_eq!(typed(&param(SqlParamType::Int), " -42 ").unwrap(), "-42");
        assert!(typed(&param(SqlParamType::Int), "4.2").is_err());
        assert_eq!(typed(&param(SqlParamType::Float), "0.25").unwrap(), "0.25");
        assert!(typed(&param(SqlParamType::Float), "inf").is_err());
        assert_eq!(typed(&param(SqlParamType::Bool), "true").unwrap(), "true");
        assert!(typed(&param(SqlParamType::Bool), "1").is_err());
        assert_eq!(
            typed(&param(SqlParamType::Timestamp), "2026-01-31T12:00:00Z").unwrap(),
            "2026-01-31 12:00:00.000000"
        );
        assert!(typed(&param(SqlParamType::Timestamp), "yesterday").is_err());
        assert!(
            typed(
                &param(SqlParamType::Uuid),
                "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
            )
            .is_ok()
        );
        assert!(typed(&param(SqlParamType::Uuid), "x").is_err());
        assert_eq!(typed(&param(SqlParamType::String), "a'b").unwrap(), "a'b");
    }

    proptest! {
        /// Whatever a caller sends, a bound saved run carries exactly the text's parameters, and non-string
        /// values are in the canonical form of their type.
        #[test]
        fn bound_values_are_canonical(user in "\\PC{0,20}", n in "\\PC{0,8}") {
            let checked = check(TEXT).unwrap();
            let (from, to) = window();
            if let Ok(bound) = bind_saved(&checked, &declared(), from, to, &values(&[("user", &user), ("n", &n)])) {
                prop_assert_eq!(bound.len(), 4);
                prop_assert!(bound[3].value.parse::<i64>().is_ok());
                prop_assert_eq!(&bound[2].value, user.trim());
            }
        }
    }
}
