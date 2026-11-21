//! Goldens and the evaluation of one case: what the driver checks, from the export request down to each
//! attribute (`docs/testing/sdk-conformance.md`, "Goldens").

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crucible_records::{
    CancelSource, Era as RecordEra, ErrorClass, ErrorSubclass, McpClassification,
    McpErrorAnnotations, McpErrorInput, McpSide, McpStatus, NanoUsd, SpanId, SpanKind, TraceId,
    Truncation, classify_mcp_error,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::collector::{API_PATH, Event, FeedbackPost, RecordedSpan};
use crate::hash::{mrtr_hash, session_hash};
use crate::records::Records;
use crate::registry::{Registry, Rule, Target};
use crate::spec::{Case, ResponseEvent};
use crate::suite::{Kind, Manifest, Scenario, Suite, SuiteError, check_schema, parse, schema};
use crate::value::{Value, get};

/// A value an assertion compares with: a literal, or a case variable (`"$era"`).
#[derive(Clone, Debug, PartialEq)]
pub enum Expected {
    /// A JSON literal.
    Literal(Json),
    /// A case variable.
    Var(String),
}

impl Expected {
    fn parse(json: &Json) -> Result<Self, String> {
        match json {
            Json::String(s) if s.starts_with('$') => Ok(Self::Var(s[1..].to_owned())),
            Json::String(_) | Json::Bool(_) | Json::Number(_) => Ok(Self::Literal(json.clone())),
            Json::Array(items) if items.iter().all(Json::is_string) => {
                Ok(Self::Literal(json.clone()))
            }
            other => Err(format!(
                "{other} is not a string, number, boolean or string array"
            )),
        }
    }
}

/// One key assertion.
#[derive(Clone, Debug)]
pub enum Assertion {
    /// Equal value and type.
    Equals(Expected),
    /// The key is absent.
    Absent,
    /// The key is present.
    Present,
    /// A string value matches.
    Matches(Regex),
    /// A string value contains every substring.
    Contains(Vec<String>),
    /// A string value parses to this JSON value.
    Json(Json),
    /// A USD amount equal in nano-USD (contract 3.4).
    Usd(NanoUsd),
    /// The contract 9.6 hash of a string the runner observed: `step` and a pointer into its response.
    MrtrHash(String, String),
    /// The contract 9.4 hash of the session id the runner observed on a step.
    SessionHash(String),
    /// Checked only when the key is present.
    Optional(Box<Assertion>),
}

impl Assertion {
    /// Parses an assertion from its JSON form.
    pub fn parse(json: &Json) -> Result<Self, String> {
        let Json::Object(object) = json else {
            return Expected::parse(json).map(Self::Equals);
        };
        let mut entries = object.iter();
        let (Some((key, value)), None) = (entries.next(), entries.next()) else {
            return Err(format!(
                "{json} is not an assertion: it has more or less than one key"
            ));
        };
        let text = || {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} takes a string"))
        };
        match key.as_str() {
            "absent" if *value == Json::Bool(true) => Ok(Self::Absent),
            "present" if *value == Json::Bool(true) => Ok(Self::Present),
            "matches" => Regex::new(&text()?)
                .map(Self::Matches)
                .map_err(|e| format!("matches: {e}")),
            "contains" => value
                .as_array()
                .filter(|items| !items.is_empty())
                .and_then(|items| {
                    items
                        .iter()
                        .map(|i| i.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                })
                .map(Self::Contains)
                .ok_or_else(|| "contains takes a non-empty list of strings".to_owned()),
            "json" => Ok(Self::Json(value.clone())),
            "usd" => NanoUsd::from_usd_decimal(&text()?)
                .map(Self::Usd)
                .ok_or_else(|| "usd takes a decimal string".to_owned()),
            "mrtr_hash" => {
                let reference = text()?;
                let slash = reference
                    .find('/')
                    .ok_or("mrtr_hash takes \"<step>/<json pointer>\"")?;
                Ok(Self::MrtrHash(
                    reference[..slash].to_owned(),
                    reference[slash..].to_owned(),
                ))
            }
            "session_hash" => text().map(Self::SessionHash),
            "optional" => match Self::parse(value)? {
                Self::Absent | Self::Optional(_) | Self::Present => {
                    Err("optional wraps a value assertion".to_owned())
                }
                inner => Ok(Self::Optional(Box::new(inner))),
            },
            other => Err(format!("{other:?} is not an assertion")),
        }
    }

    fn vars(&self, out: &mut Vec<String>) {
        match self {
            Self::Equals(Expected::Var(v)) => out.push(v.clone()),
            Self::Optional(inner) => inner.vars(out),
            _ => {}
        }
    }

    fn steps(&self, out: &mut Vec<String>) {
        match self {
            Self::MrtrHash(step, _) | Self::SessionHash(step) => out.push(step.clone()),
            Self::Optional(inner) => inner.steps(out),
            _ => {}
        }
    }
}

/// Case conditions: any of a list of all-of maps. Empty means always.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct When(Vec<BTreeMap<String, Vec<String>>>);

const MCP_CONDITIONS: [(&str, &[&str]); 2] = [
    ("era", &["legacy", "modern"]),
    ("transport", &["stdio", "streamable_http"]),
];
const MANIFEST_CONDITIONS: [(&str, &[&str]); 2] = [
    ("throw_outcome", &["tool_error", "internal"]),
    ("progress_count", &["observed", "unobserved"]),
];

impl When {
    /// Parses `when`: an object, or a list of objects any of which may hold.
    pub fn parse(json: Option<&Json>, mcp: bool) -> Result<Self, String> {
        let Some(json) = json else {
            return Ok(Self::default());
        };
        let maps: Vec<&serde_json::Map<String, Json>> = match json {
            Json::Object(o) => vec![o],
            Json::Array(items) if !items.is_empty() => items
                .iter()
                .map(Json::as_object)
                .collect::<Option<_>>()
                .ok_or("when lists objects")?,
            _ => return Err("when is an object or a non-empty list of objects".to_owned()),
        };
        let mut out = Vec::new();
        for map in maps {
            let mut conditions = BTreeMap::new();
            for (key, value) in map {
                let allowed = MCP_CONDITIONS
                    .iter()
                    .filter(|_| mcp)
                    .chain(MANIFEST_CONDITIONS.iter())
                    .find(|(name, _)| *name == key.as_str())
                    .map(|(_, values)| *values)
                    .ok_or_else(|| format!("when cannot test {key:?} here"))?;
                let values: Vec<String> = match value {
                    Json::String(s) => vec![s.clone()],
                    Json::Array(items) => items
                        .iter()
                        .map(|i| i.as_str().map(str::to_owned))
                        .collect::<Option<_>>()
                        .ok_or("when values are strings")?,
                    _ => return Err("when values are strings".to_owned()),
                };
                if let Some(bad) = values.iter().find(|v| !allowed.contains(&v.as_str())) {
                    return Err(format!("when: {key} cannot be {bad:?}"));
                }
                conditions.insert(key.clone(), values);
            }
            out.push(conditions);
        }
        Ok(Self(out))
    }

    fn holds(&self, facts: &BTreeMap<&'static str, String>) -> bool {
        self.0.is_empty()
            || self.0.iter().any(|all| {
                all.iter().all(|(key, values)| {
                    facts
                        .get(key.as_str())
                        .is_some_and(|fact| values.contains(fact))
                })
            })
    }
}

/// The expected MCP classification (contract 9.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// `status`.
    pub status: McpStatus,
    /// `error_class`, when checked.
    pub error_class: Option<ErrorClass>,
    /// `error_subclass`, when checked.
    pub error_subclass: Option<ErrorSubclass>,
    /// `caller_fault`, when checked.
    pub caller_fault: Option<bool>,
}

impl Outcome {
    const OK: Self = Self {
        status: McpStatus::Ok,
        error_class: None,
        error_subclass: None,
        caller_fault: None,
    };
}

/// One expected span.
#[derive(Clone, Debug)]
pub struct Expectation {
    /// Report name.
    pub id: String,
    /// Attribute values (or `name`) that pick the span.
    pub select: Vec<(String, Expected)>,
    /// When the expectation applies.
    pub when: When,
    /// Span kind.
    pub kind: Option<SpanKind>,
    /// `Some(None)`: a root; `Some(Some(id))`: the child of that expectation's span.
    pub parent: Option<Option<String>>,
    /// Literal trace id.
    pub trace_id: Option<TraceId>,
    /// Literal parent span id.
    pub parent_span_id: Option<SpanId>,
    /// Duration lower bound.
    pub min_duration_ms: Option<u64>,
    /// Key assertions.
    pub attributes: Vec<(String, Assertion)>,
    /// Derived-field values.
    pub fields: Vec<(String, Expected)>,
    /// MCP classification.
    pub outcome: Option<Outcome>,
}

/// Assertions for every span of a case.
#[derive(Clone, Debug, Default)]
pub struct Checks {
    /// Key assertions.
    pub attributes: Vec<(String, Assertion)>,
    /// Derived-field values.
    pub fields: Vec<(String, Expected)>,
}

/// A planted value that must not appear in any export.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Planted {
    /// Report name.
    pub label: String,
    /// The value.
    pub value: String,
}

/// The `Idempotency-Key` an expected feedback post carries.
#[derive(Clone, Debug)]
pub enum KeyExpectation {
    /// This key, which the caller passed.
    Literal(String),
    /// A key the SDK made: it matches, and differs from every other key the SDK made in the case.
    Matches(Regex),
}

/// One expected post to the feedback route (contract 12.6).
#[derive(Clone, Debug)]
pub struct FeedbackExpectation {
    /// Report name.
    pub id: String,
    /// The `Idempotency-Key` header.
    pub idempotency_key: KeyExpectation,
    /// The JSON body; numbers compare by value.
    pub body: Json,
    /// Expectation ids of spans whose export arrives before this post.
    pub after: Vec<String>,
}

/// A scenario's golden.
#[derive(Clone, Debug)]
pub struct Golden {
    /// Planted values.
    pub forbidden_strings: Vec<Planted>,
    /// Expected spans.
    pub spans: Vec<Expectation>,
    /// Assertions for every span.
    pub every_span: Checks,
    /// Selectors that must match no span.
    pub absent: Vec<Vec<(String, Expected)>>,
    /// Checkpoint name to expectation ids whose spans arrive before it.
    pub checkpoints: BTreeMap<String, Vec<String>>,
    /// The posts to the feedback route, in the order they must arrive.
    pub feedback: Vec<FeedbackExpectation>,
}

/// `goldens/common.json`.
#[derive(Clone, Debug)]
pub struct Common {
    /// Expectations the driver adds to every legacy case.
    pub legacy_handshake: Vec<Expectation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenFile {
    schema: String,
    scenario: String,
    #[serde(default)]
    forbidden_strings: Vec<Planted>,
    spans: Vec<ExpectationFile>,
    #[serde(default)]
    every_span: Option<ChecksFile>,
    #[serde(default)]
    absent: Vec<BTreeMap<String, Json>>,
    #[serde(default)]
    checkpoints: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    feedback: Vec<FeedbackFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedbackFile {
    id: String,
    idempotency_key: Json,
    body: Json,
    #[serde(default)]
    after: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommonFile {
    schema: String,
    legacy_handshake: Vec<ExpectationFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksFile {
    #[serde(default)]
    attributes: BTreeMap<String, Json>,
    #[serde(default)]
    fields: BTreeMap<String, Json>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectationFile {
    id: String,
    select: BTreeMap<String, Json>,
    #[serde(default)]
    when: Option<Json>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, deserialize_with = "present")]
    parent: Option<Option<String>>,
    #[serde(default)]
    trace_id: Option<String>,
    #[serde(default)]
    parent_span_id: Option<String>,
    #[serde(default)]
    min_duration_ms: Option<u64>,
    #[serde(default)]
    attributes: BTreeMap<String, Json>,
    #[serde(default)]
    fields: BTreeMap<String, Json>,
    #[serde(default)]
    outcome: Option<OutcomeFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeFile {
    status: String,
    #[serde(default)]
    error_class: Option<String>,
    #[serde(default)]
    error_subclass: Option<String>,
    #[serde(default)]
    caller_fault: Option<bool>,
}

/// Tells an explicit `null` (`Some(None)`) from an absent field (`None`).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// The case variables a kind of golden may name.
fn known_vars(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Mcp => &[
            "era",
            "transport",
            "capture_source",
            "protocol_version",
            "server_name",
            "server_version",
            "deploy_id",
            "client_name",
            "client_version",
            "service_name",
        ],
        Kind::Agent => &["service_name", "environment", "release"],
    }
}

fn assertions(map: BTreeMap<String, Json>) -> Result<Vec<(String, Assertion)>, String> {
    map.into_iter()
        .map(|(key, json)| {
            Assertion::parse(&json)
                .map(|a| (key.clone(), a))
                .map_err(|e| format!("{key}: {e}"))
        })
        .collect()
}

fn expecteds(map: BTreeMap<String, Json>) -> Result<Vec<(String, Expected)>, String> {
    map.into_iter()
        .map(|(key, json)| {
            Expected::parse(&json)
                .map(|e| (key.clone(), e))
                .map_err(|e| format!("{key}: {e}"))
        })
        .collect()
}

fn checks(raw: Option<ChecksFile>, registry: &Registry, kind: Kind) -> Result<Checks, String> {
    let Some(raw) = raw else {
        return Ok(Checks::default());
    };
    let checks = Checks {
        attributes: assertions(raw.attributes)?,
        fields: expecteds(raw.fields)?,
    };
    check_names(&checks.attributes, &checks.fields, registry, kind)?;
    Ok(checks)
}

fn check_names(
    attributes: &[(String, Assertion)],
    fields: &[(String, Expected)],
    registry: &Registry,
    kind: Kind,
) -> Result<(), String> {
    if let Some((field, _)) = fields.iter().find(|(f, _)| !registry.has_field(f)) {
        return Err(format!("unknown field {field:?}"));
    }
    let mut vars = Vec::new();
    for (_, a) in attributes {
        a.vars(&mut vars);
    }
    for (_, e) in fields {
        if let Expected::Var(v) = e {
            vars.push(v.clone());
        }
    }
    if let Some(bad) = vars
        .iter()
        .find(|v| !known_vars(kind).contains(&v.as_str()))
    {
        return Err(format!("unknown variable ${bad}"));
    }
    Ok(())
}

fn expectation(
    raw: ExpectationFile,
    registry: &Registry,
    kind: Kind,
) -> Result<Expectation, String> {
    let id = raw.id;
    let err = |e: String| format!("span {id:?}: {e}");
    if raw.select.is_empty() {
        return Err(err("select is empty".to_owned()));
    }
    let outcome = match raw.outcome {
        None => None,
        Some(_) if kind == Kind::Agent => {
            return Err(err("outcome applies to MCP spans".to_owned()));
        }
        Some(o) => Some(Outcome {
            status: McpStatus::parse(&o.status)
                .ok_or_else(|| err(format!("status {:?}", o.status)))?,
            error_class: o
                .error_class
                .map(|c| ErrorClass::parse(&c).ok_or_else(|| err(format!("error_class {c:?}"))))
                .transpose()?,
            error_subclass: o
                .error_subclass
                .map(|c| {
                    ErrorSubclass::parse(&c).ok_or_else(|| err(format!("error_subclass {c:?}")))
                })
                .transpose()?,
            caller_fault: o.caller_fault,
        }),
    };
    let expectation = Expectation {
        select: expecteds(raw.select).map_err(err)?,
        when: When::parse(raw.when.as_ref(), kind == Kind::Mcp).map_err(err)?,
        kind: raw
            .kind
            .map(|k| SpanKind::parse(&k).ok_or_else(|| err(format!("kind {k:?}"))))
            .transpose()?,
        parent: raw.parent,
        trace_id: raw
            .trace_id
            .map(|t| TraceId::parse(&t).map_err(|e| err(format!("trace_id: {e}"))))
            .transpose()?,
        parent_span_id: raw
            .parent_span_id
            .map(|s| SpanId::parse(&s).map_err(|e| err(format!("parent_span_id: {e}"))))
            .transpose()?,
        min_duration_ms: raw.min_duration_ms,
        attributes: assertions(raw.attributes).map_err(err)?,
        fields: expecteds(raw.fields).map_err(err)?,
        outcome,
        id,
    };
    check_names(&expectation.attributes, &expectation.fields, registry, kind)
        .map_err(|e| format!("span {:?}: {e}", expectation.id))?;
    Ok(expectation)
}

impl Common {
    /// Parses `goldens/common.json`.
    pub fn parse(file: &str, bytes: &[u8]) -> Result<Self, SuiteError> {
        let raw: CommonFile = parse(file, bytes)?;
        check_schema(file, &raw.schema, schema::COMMON)?;
        // Field names are checked against the registry when a golden that uses them is loaded.
        let mut legacy_handshake = Vec::new();
        for e in raw.legacy_handshake {
            if !e.fields.is_empty() || e.outcome.is_some() || e.when.is_some() {
                return Err(SuiteError::new(
                    file,
                    "handshake expectations hold select, kind, parent and attributes only",
                ));
            }
            let attributes = assertions(e.attributes).map_err(|m| SuiteError::new(file, m))?;
            legacy_handshake.push(Expectation {
                select: expecteds(e.select).map_err(|m| SuiteError::new(file, m))?,
                when: When::default(),
                kind: e
                    .kind
                    .map(|k| SpanKind::parse(&k).ok_or_else(|| SuiteError::new(file, "bad kind")))
                    .transpose()?,
                parent: e.parent,
                trace_id: None,
                parent_span_id: None,
                min_duration_ms: e.min_duration_ms,
                attributes,
                fields: Vec::new(),
                outcome: None,
                id: e.id,
            });
        }
        Ok(Self { legacy_handshake })
    }
}

impl Golden {
    /// Parses and checks a golden against its scenario and the registry.
    pub fn parse(
        file: &str,
        bytes: &[u8],
        scenario: &Scenario,
        registry: &Registry,
    ) -> Result<Self, SuiteError> {
        let raw: GoldenFile = parse(file, bytes)?;
        check_schema(file, &raw.schema, schema::GOLDEN)?;
        let err = |m: String| SuiteError::new(file, m);
        if raw.scenario != scenario.id {
            return Err(err(format!(
                "scenario is {:?}, not {:?}",
                raw.scenario, scenario.id
            )));
        }
        let kind = scenario.kind;
        let spans = raw
            .spans
            .into_iter()
            .map(|e| expectation(e, registry, kind))
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        if spans.is_empty() {
            return Err(err("a golden expects at least one span".to_owned()));
        }
        let ids: Vec<&str> = spans.iter().map(|e| e.id.as_str()).collect();
        for (i, id) in ids.iter().enumerate() {
            if ids[..i].contains(id) || ["initialize", "initialized"].contains(id) {
                return Err(err(format!("span id {id:?} is used twice or is reserved")));
            }
        }
        for e in &spans {
            if let Some(Some(parent)) = &e.parent
                && !ids.contains(&parent.as_str())
            {
                return Err(err(format!(
                    "span {:?}: parent {parent:?} is not a span id",
                    e.id
                )));
            }
            let mut steps = Vec::new();
            for (_, a) in &e.attributes {
                a.steps(&mut steps);
            }
            for step in steps {
                if !scenario.steps.iter().any(|s| s.step == step) && step != "initialize" {
                    return Err(err(format!(
                        "span {:?}: no step {step:?} in the scenario",
                        e.id
                    )));
                }
            }
        }
        let checkpoint_names: Vec<&str> = scenario
            .steps
            .iter()
            .filter_map(|s| s.checkpoint.as_deref())
            .collect();
        for (name, members) in &raw.checkpoints {
            if !checkpoint_names.contains(&name.as_str()) {
                return Err(err(format!(
                    "checkpoint {name:?} is not a step of the scenario"
                )));
            }
            if let Some(bad) = members.iter().find(|m| !ids.contains(&m.as_str())) {
                return Err(err(format!(
                    "checkpoint {name:?} names unknown span {bad:?}"
                )));
            }
        }
        let absent = raw
            .absent
            .into_iter()
            .map(expecteds)
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        if absent.iter().any(Vec::is_empty) {
            return Err(err("an absent selector is empty".to_owned()));
        }
        if raw.forbidden_strings.iter().any(|p| p.value.is_empty()) {
            return Err(err("a forbidden string is empty".to_owned()));
        }
        let feedback = feedback_expectations(raw.feedback, scenario, &ids).map_err(err)?;
        Ok(Self {
            forbidden_strings: raw.forbidden_strings,
            spans,
            every_span: checks(raw.every_span, registry, kind).map_err(err)?,
            absent,
            checkpoints: raw.checkpoints,
            feedback,
        })
    }
}

/// Parses the expected feedback posts: one per feedback step of the scenario, in step order.
fn feedback_expectations(
    raw: Vec<FeedbackFile>,
    scenario: &Scenario,
    span_ids: &[&str],
) -> Result<Vec<FeedbackExpectation>, String> {
    let steps = scenario
        .steps
        .iter()
        .filter(|s| s.feedback.is_some())
        .count();
    if raw.len() != steps {
        return Err(format!(
            "feedback lists {} posts; the scenario has {steps} feedback steps",
            raw.len()
        ));
    }
    raw.into_iter()
        .map(|f| {
            let id = f.id;
            let err = |e: &str| format!("feedback {id:?}: {e}");
            let idempotency_key = match &f.idempotency_key {
                Json::String(key) => KeyExpectation::Literal(key.clone()),
                Json::Object(o) if o.len() == 1 => match o.get("matches").and_then(Json::as_str) {
                    Some(pattern) => KeyExpectation::Matches(
                        Regex::new(pattern).map_err(|e| err(&format!("matches: {e}")))?,
                    ),
                    None => {
                        return Err(err("idempotency_key is a string or {\"matches\": <regex>}"));
                    }
                },
                _ => return Err(err("idempotency_key is a string or {\"matches\": <regex>}")),
            };
            if !f.body.is_object() {
                return Err(err("body is a JSON object"));
            }
            if let Some(bad) = f.after.iter().find(|a| !span_ids.contains(&a.as_str())) {
                return Err(err(&format!("after names unknown span {bad:?}")));
            }
            Ok(FeedbackExpectation {
                idempotency_key,
                body: f.body,
                after: f.after,
                id,
            })
        })
        .collect()
}

/// One failed check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// The expectation id, or the span name for checks on every span.
    pub span: Option<String>,
    /// What was checked.
    pub check: String,
    /// The expected value.
    pub expected: String,
    /// The value found.
    pub actual: String,
    /// The contract section.
    pub contract: Option<String>,
}

/// The result of evaluating one case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// Checks evaluated.
    pub checks: u32,
    /// Failed checks.
    pub failures: Vec<Finding>,
}

/// Everything one evaluation reads.
#[derive(Clone, Copy, Debug)]
pub struct CaseInput<'a> {
    /// The suite.
    pub suite: &'a Suite,
    /// The case.
    pub case: &'a Case<'a>,
    /// The SDK's manifest.
    pub manifest: &'a Manifest,
    /// The collector's log for the case.
    pub events: &'a [Event],
    /// The runner's response events.
    pub responses: &'a [ResponseEvent],
}

/// Evaluates one case.
pub fn evaluate(input: &CaseInput<'_>) -> Evaluation {
    let mut eval = Eval::new(input);
    eval.run();
    Evaluation {
        checks: eval.checks,
        failures: eval.failures,
    }
}

struct Eval<'a> {
    input: &'a CaseInput<'a>,
    registry: &'a Registry,
    golden: Option<&'a Golden>,
    vars: BTreeMap<&'static str, Json>,
    facts: BTreeMap<&'static str, String>,
    spans: Vec<(usize, &'a RecordedSpan)>,
    /// What the real normalizer makes of the case's exports.
    records: Records,
    checks: u32,
    failures: Vec<Finding>,
}

fn show(value: Option<&Value>) -> String {
    value.map_or_else(|| "absent".to_owned(), |v| v.to_json().to_string())
}

fn is_mcp(span: &RecordedSpan) -> bool {
    get(&span.attributes, "mcp.method.name").is_some()
}

impl<'a> Eval<'a> {
    fn new(input: &'a CaseInput<'a>) -> Self {
        let suite = input.suite;
        let constants = &suite.constants;
        let case = input.case;
        let kind = case.scenario.kind;
        let mut vars = BTreeMap::new();
        let mut facts = BTreeMap::new();
        facts.insert(
            "throw_outcome",
            input.manifest.throw_outcome.as_str().to_owned(),
        );
        facts.insert(
            "progress_count",
            input.manifest.progress_count.as_str().to_owned(),
        );
        match kind {
            Kind::Mcp => {
                if let Some(variant) = case.variant {
                    let era = variant.era();
                    vars.insert("era", Json::from(era.as_str()));
                    vars.insert("transport", Json::from(variant.transport().as_str()));
                    vars.insert(
                        "capture_source",
                        Json::from(input.manifest.source.capture_source(variant.transport())),
                    );
                    vars.insert("protocol_version", Json::from(constants.eras.version(era)));
                    facts.insert("era", era.as_str().to_owned());
                    facts.insert("transport", variant.transport().as_str().to_owned());
                }
                vars.insert(
                    "server_name",
                    Json::from(constants.mcp.server.name.as_str()),
                );
                vars.insert(
                    "server_version",
                    Json::from(constants.mcp.server.version.as_str()),
                );
                if let Some(deploy) = &constants.mcp.sdk.deploy_id {
                    vars.insert("deploy_id", Json::from(deploy.as_str()));
                }
                vars.insert("client_name", Json::from(constants.client.name.as_str()));
                vars.insert(
                    "client_version",
                    Json::from(constants.client.version.as_str()),
                );
                vars.insert(
                    "service_name",
                    Json::from(constants.mcp.sdk.service_name.as_str()),
                );
            }
            Kind::Agent => {
                let sdk = &constants.agent.sdk;
                vars.insert("service_name", Json::from(sdk.service_name.as_str()));
                if let Some(env) = &sdk.environment {
                    vars.insert("environment", Json::from(env.as_str()));
                }
                if let Some(release) = &sdk.release {
                    vars.insert("release", Json::from(release.as_str()));
                }
            }
        }
        let spans = input
            .events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                Event::Export(export) => Some((i, export)),
                Event::Checkpoint(_) | Event::Feedback(_) => None,
            })
            .flat_map(|(i, export)| export.spans.iter().map(move |s| (i, s)))
            .collect();
        Self {
            input,
            registry: &suite.registry,
            golden: suite.goldens.get(&case.scenario.id),
            vars,
            facts,
            spans,
            records: Records::of(input.events),
            checks: 0,
            failures: Vec::new(),
        }
    }

    fn check(
        &mut self,
        ok: bool,
        span: Option<&str>,
        check: impl Into<String>,
        expected: impl Into<String>,
        actual: impl FnOnce() -> String,
        contract: Option<&str>,
    ) {
        self.checks += 1;
        if !ok {
            self.failures.push(Finding {
                span: span.map(str::to_owned),
                check: check.into(),
                expected: expected.into(),
                actual: actual(),
                contract: contract.map(str::to_owned),
            });
        }
    }

    fn resolve(&self, expected: &Expected) -> Option<Json> {
        match expected {
            Expected::Literal(json) => Some(json.clone()),
            Expected::Var(name) => self.vars.get(name.as_str()).cloned(),
        }
    }

    fn run(&mut self) {
        self.exports();
        self.taint();
        self.every_span_contract();
        self.normalizer();
        let bound = self.bind();
        self.rules();
        self.golden_extras(&bound);
        self.feedback(&bound);
    }

    /// The posts to the feedback route: as many as the golden expects, in its order, each to the project's route
    /// with the secret key, a JSON body equal to the expected one and its `Idempotency-Key`.
    fn feedback(&mut self, bound: &BTreeMap<String, &'a RecordedSpan>) {
        let expected: &[FeedbackExpectation] = self.golden.map_or(&[], |g| g.feedback.as_slice());
        let posts: Vec<(usize, &'a FeedbackPost)> = self
            .input
            .events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match e {
                Event::Feedback(post) => Some((i, post)),
                _ => None,
            })
            .collect();
        self.check(
            posts.len() == expected.len(),
            None,
            "posts to the feedback route",
            expected.len().to_string(),
            || posts.len().to_string(),
            Some("12.6"),
        );
        let constants = &self.input.suite.constants;
        let route = format!("{API_PATH}/projects/{}/feedback", constants.project_id);
        let bearer = format!("Bearer {}", constants.keys.secret_key);
        let basic = format!(
            "Basic {}",
            STANDARD.encode(format!(
                "{}:{}",
                constants.keys.public_key, constants.keys.secret_key
            ))
        );
        let mut made: Vec<&str> = Vec::new();
        for (e, (at, post)) in expected.iter().zip(posts.iter().copied()) {
            let id = Some(e.id.as_str());
            self.check(
                post.path == route,
                id,
                "the project's feedback route",
                format!("POST {route}"),
                || format!("POST {}", post.path),
                Some("12.6"),
            );
            let media = post
                .content_type
                .as_deref()
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            self.check(
                media == "application/json",
                id,
                "feedback Content-Type",
                "application/json",
                || format!("{:?}", post.content_type),
                Some("12.6"),
            );
            let auth = post.authorization.as_deref();
            self.check(
                auth == Some(bearer.as_str()) || auth == Some(basic.as_str()),
                id,
                "feedback Authorization",
                "Bearer <secret key> or Basic base64(<public key>:<secret key>) with the run spec's keys",
                || match auth {
                    None => "absent".to_owned(),
                    Some(value) => format!(
                        "a {} credential that is not the run's key",
                        value.split(' ').next().unwrap_or("malformed")
                    ),
                },
                Some("11.3"),
            );
            let key = post.idempotency_key.as_deref();
            let (ok, want) = match &e.idempotency_key {
                KeyExpectation::Literal(literal) => {
                    (key == Some(literal.as_str()), literal.clone())
                }
                KeyExpectation::Matches(regex) => {
                    let fresh = key.is_some_and(|k| regex.is_match(k) && !made.contains(&k));
                    if let Some(k) = key {
                        made.push(k);
                    }
                    (
                        fresh,
                        format!("a key the SDK made, matching {}, used once", regex.as_str()),
                    )
                }
            };
            self.check(
                ok,
                id,
                "Idempotency-Key",
                want,
                || key.map_or_else(|| "absent".to_owned(), str::to_owned),
                Some("12.5"),
            );
            let body: Option<Json> = serde_json::from_slice(&post.body).ok();
            self.check(
                body.as_ref().is_some_and(|b| json_equal(b, &e.body)),
                id,
                "feedback body",
                e.body.to_string(),
                || String::from_utf8_lossy(&post.body).into_owned(),
                Some("12.6"),
            );
            for span in &e.after {
                let exported = bound.get(span).and_then(|bound_span| {
                    self.spans
                        .iter()
                        .find(|(_, s)| std::ptr::eq(*s, *bound_span))
                        .map(|(i, _)| *i)
                });
                self.check(
                    exported.is_some_and(|i| i < at),
                    id,
                    format!("posted after the span {span:?} was exported"),
                    "the span's export first",
                    || match exported {
                        None => "the span never arrived".to_owned(),
                        Some(_) => "the post came first".to_owned(),
                    },
                    Some("12.6"),
                );
            }
        }
    }

    fn exports(&mut self) {
        let registry = self.registry;
        let export_spec = &registry.export;
        let keys = &self.input.suite.constants.keys;
        let bearer = format!("Bearer {}", keys.secret_key);
        let basic = format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", keys.public_key, keys.secret_key))
        );
        let contract = export_spec.contract.clone();
        let exports: Vec<_> = self
            .input
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Export(x) => Some(x),
                Event::Checkpoint(_) | Event::Feedback(_) => None,
            })
            .collect();
        self.check(
            !exports.is_empty(),
            None,
            "at least one export arrived",
            "one or more requests",
            || "none".to_owned(),
            Some(contract.as_str()),
        );
        for (n, export) in exports.iter().enumerate() {
            let label = format!("export {}", n + 1);
            self.check(
                export.method == "POST" && export.path == export_spec.path,
                Some(label.as_str()),
                "method and path",
                format!("POST {}", export_spec.path),
                || format!("{} {}", export.method, export.path),
                Some(contract.as_str()),
            );
            let media = export
                .content_type
                .as_deref()
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            self.check(
                media == export_spec.media_type,
                Some(label.as_str()),
                "Content-Type",
                export_spec.media_type.clone(),
                || format!("{:?}", export.content_type),
                Some(contract.as_str()),
            );
            let encoding = export
                .content_encoding
                .as_deref()
                .unwrap_or("identity")
                .trim()
                .to_ascii_lowercase();
            self.check(
                export_spec.encodings.contains(&encoding),
                Some(label.as_str()),
                "Content-Encoding",
                export_spec.encodings.join(" or "),
                || encoding.clone(),
                Some(contract.as_str()),
            );
            let auth = export.authorization.as_deref();
            self.check(
                auth == Some(bearer.as_str()) || auth == Some(basic.as_str()),
                Some(label.as_str()),
                "Authorization",
                "Bearer <secret key> or Basic base64(<public key>:<secret key>) with the run spec's keys",
                || match auth {
                    None => "absent".to_owned(),
                    Some(value) => format!(
                        "a {} credential that is not the run's key",
                        value.split(' ').next().unwrap_or("malformed")
                    ),
                },
                Some("12.2"),
            );
            self.check(
                export.refusal.is_none(),
                Some(label.as_str()),
                "the collector accepted the request",
                "200",
                || {
                    format!(
                        "{}: {}",
                        export.status,
                        export.refusal.clone().unwrap_or_default()
                    )
                },
                Some(contract.as_str()),
            );
        }
        let spans = self.spans.clone();
        for (_, span) in spans {
            self.check(
                span.rejection.is_none(),
                Some(span.name.as_str()),
                "trace and span ids are not all zero",
                "non-zero ids",
                || format!("{:?}", span.rejection),
                Some("1.1"),
            );
        }
    }

    fn taint(&mut self) {
        let keys = &self.input.suite.constants.keys;
        let mut forbidden: Vec<(String, String)> = vec![
            ("the project secret key".to_owned(), keys.secret_key.clone()),
            (
                "the Basic credential".to_owned(),
                STANDARD.encode(format!("{}:{}", keys.public_key, keys.secret_key)),
            ),
        ];
        if let Some(golden) = self.golden {
            forbidden.extend(
                golden
                    .forbidden_strings
                    .iter()
                    .map(|p| (format!("planted {}", p.label), p.value.clone())),
            );
        }
        for response in self.input.responses {
            if let Some(session) = response.session_id.as_deref().filter(|s| !s.is_empty()) {
                forbidden.push(("a raw MCP session id".to_owned(), session.to_owned()));
            }
            if let Some(state) = response
                .message
                .as_ref()
                .and_then(|m| m.pointer("/result/requestState"))
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
            {
                forbidden.push(("a raw requestState".to_owned(), state.to_owned()));
            }
        }
        forbidden.sort();
        forbidden.dedup();
        for (label, value) in forbidden {
            let found = self
                .input
                .events
                .iter()
                .enumerate()
                .find_map(|(i, e)| match e {
                    Event::Export(x) => {
                        memchr::memmem::find(&x.body, value.as_bytes()).map(|_| (i, x))
                    }
                    Event::Checkpoint(_) | Event::Feedback(_) => None,
                });
            let place = found.map(|(_, export)| {
                export
                    .spans
                    .iter()
                    .find_map(|s| locate(s, &value))
                    .unwrap_or_else(|| "the request body".to_owned())
            });
            self.check(
                place.is_none(),
                None,
                format!("{label} never leaves the sender"),
                "absent from every export",
                || place.clone().unwrap_or_default(),
                Some("9.1"),
            );
        }
    }

    fn every_span_contract(&mut self) {
        let spans = self.spans.clone();
        for (_, span) in spans {
            let mut forbidden = Vec::new();
            let mut wrong = Vec::new();
            let every = span
                .attributes
                .iter()
                .chain(span.resource.iter())
                .chain(span.events.iter().flat_map(|e| e.attributes.iter()));
            for (key, value) in every {
                if self.registry.is_forbidden(key) {
                    forbidden.push(key.clone());
                }
                if let Some(e) = self.registry.type_error(key, value) {
                    wrong.push(e);
                }
            }
            let contract = self.registry.forbidden.contract.clone();
            self.check(
                forbidden.is_empty(),
                Some(span.name.as_str()),
                "no forbidden keys",
                "none",
                || forbidden.join(", "),
                Some(contract.as_str()),
            );
            self.check(
                wrong.is_empty(),
                Some(span.name.as_str()),
                "registry types",
                "every known key has its registry type",
                || wrong.join("; "),
                None,
            );
        }
    }

    /// The real normalizer takes every accepted export, and makes an MCP operation of exactly the spans that
    /// carry `mcp.method.name` (contract 9.1).
    fn normalizer(&mut self) {
        let errors = self.records.errors.join("; ");
        self.check(
            errors.is_empty(),
            None,
            "the normalizer takes every export",
            "no error",
            || errors.clone(),
            Some("3.2"),
        );
        if !errors.is_empty() {
            return;
        }
        let spans = self.spans.clone();
        for (_, span) in spans.iter().filter(|(_, s)| s.rejection.is_none()) {
            let (carries, made) = (is_mcp(span), self.records.op(span).is_some());
            self.check(
                carries == made,
                Some(span.name.as_str()),
                "an MCP operation record exactly for a span with mcp.method.name",
                if carries { "a record" } else { "no record" },
                || if made { "a record" } else { "no record" }.to_owned(),
                Some("9.1"),
            );
            self.content(span);
        }
    }

    /// The real normalizer keeps a span's captured content as it was sent, whatever the capture source
    /// (contract 9.10): the arguments and the result byte for byte, the captured flag, the sample rate and the
    /// truncation, read from the span's keys as the contract reads them.
    fn content(&mut self, span: &RecordedSpan) {
        let Some(op) = self.records.op(span) else {
            return;
        };
        let attrs = &span.attributes;
        let text = |key: &str| get(attrs, key).and_then(Value::as_str);
        let (args, result) = (
            text("gen_ai.tool.call.arguments"),
            text("gen_ai.tool.call.result"),
        );
        let captured = match get(attrs, "crucible.mcp.content.captured") {
            Some(Value::Bool(flag)) => *flag,
            Some(Value::String(flag)) if flag == "true" || flag == "false" => flag == "true",
            _ => args.is_some() || result.is_some(),
        };
        let rate = match get(attrs, "crucible.mcp.content.sample_rate") {
            Some(Value::Double(rate)) if rate.is_finite() => (*rate as f32).clamp(0.0, 1.0),
            Some(Value::Int(rate)) => (*rate as f32).clamp(0.0, 1.0),
            Some(Value::Double(_)) => 0.0,
            _ => f32::from(u8::from(captured)),
        };
        let truncation = text("crucible.mcp.content.truncation")
            .and_then(Truncation::parse)
            .unwrap_or(Truncation::None);
        let (args, result) = if captured {
            (args.unwrap_or(""), result.unwrap_or(""))
        } else {
            ("", "")
        };
        let want = (
            captured,
            rate,
            truncation,
            args.as_bytes(),
            result.as_bytes(),
        );
        let found = (
            op.content_captured,
            op.content_sample_rate,
            op.content_truncation,
            &op.args[..],
            &op.result[..],
        );
        let ok = want == found;
        let (want, found) = (describe_content(want), describe_content(found));
        self.check(
            ok,
            Some(span.name.as_str()),
            "the normalizer's record keeps the captured content as sent",
            want,
            || found,
            Some("9.10"),
        );
    }

    fn selects(&self, span: &RecordedSpan, select: &[(String, Expected)]) -> bool {
        select.iter().all(|(key, expected)| {
            let Some(want) = self.resolve(expected) else {
                return false;
            };
            if key == "name" {
                return want.as_str() == Some(span.name.as_str());
            }
            equals(get(&span.attributes, key), &want)
        })
    }

    fn describe(&self, select: &[(String, Expected)]) -> String {
        select
            .iter()
            .map(|(k, e)| format!("{k} = {}", self.resolve(e).unwrap_or(Json::Null)))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Matches every applicable expectation to exactly one span; returns the bindings.
    fn bind(&mut self) -> BTreeMap<String, &'a RecordedSpan> {
        let mut expectations: Vec<&'a Expectation> = Vec::new();
        let case = self.input.case;
        if case.era() == Some(crate::suite::Era::Legacy) {
            expectations.extend(self.input.suite.common.legacy_handshake.iter());
        }
        if let Some(golden) = self.golden {
            expectations.extend(golden.spans.iter().filter(|e| e.when.holds(&self.facts)));
        }
        let mut bound: BTreeMap<String, &'a RecordedSpan> = BTreeMap::new();
        let mut used: Vec<usize> = Vec::new();
        for e in &expectations {
            let matches: Vec<usize> = (0..self.spans.len())
                .filter(|i| self.selects(self.spans[*i].1, &e.select))
                .collect();
            let described = self.describe(&e.select);
            self.check(
                matches.len() == 1,
                Some(e.id.as_str()),
                format!("exactly one span with {described}"),
                "1",
                || matches.len().to_string(),
                None,
            );
            if let [only] = matches.as_slice() {
                let twice = used.contains(only);
                self.check(
                    !twice,
                    Some(e.id.as_str()),
                    "the span matches no other expectation",
                    "one expectation",
                    || "two or more".to_owned(),
                    None,
                );
                used.push(*only);
                bound.insert(e.id.clone(), self.spans[*only].1);
            }
        }
        let mcp = case.scenario.kind == Kind::Mcp;
        let unexpected: Vec<String> = self
            .spans
            .iter()
            .enumerate()
            .filter(|(i, (_, s))| !used.contains(i) && (!mcp || is_mcp(s)))
            .map(|(_, (_, s))| {
                if mcp {
                    format!(
                        "{} (jsonrpc.request.id {})",
                        show(get(&s.attributes, "mcp.method.name")),
                        show(get(&s.attributes, "jsonrpc.request.id"))
                    )
                } else {
                    format!("{:?}", s.name)
                }
            })
            .collect();
        self.check(
            unexpected.is_empty(),
            None,
            if mcp {
                "every MCP operation is expected exactly once"
            } else {
                "every span is expected exactly once"
            },
            "no other spans",
            || unexpected.join(", "),
            None,
        );
        for e in &expectations {
            if let Some(span) = bound.get(&e.id).copied() {
                self.expectation(e, span, &bound);
            }
        }
        bound
    }

    fn expectation(
        &mut self,
        e: &Expectation,
        span: &RecordedSpan,
        bound: &BTreeMap<String, &'a RecordedSpan>,
    ) {
        let id = Some(e.id.as_str());
        if let Some(kind) = e.kind {
            self.check(
                span.kind == kind,
                id,
                "span kind",
                kind.as_str(),
                || span.kind.as_str().to_owned(),
                Some("9.1"),
            );
        }
        match &e.parent {
            None => {}
            Some(None) => self.check(
                span.parent_span_id.is_none(),
                id,
                "a root span (no trace context was propagated)",
                "no parent",
                || format!("parent {:?}", span.parent_span_id.map(|p| p.to_string())),
                Some("9.4"),
            ),
            Some(Some(parent)) => {
                let parent_span = bound.get(parent).copied();
                self.check(
                    parent_span.is_some_and(|p| {
                        p.trace_id == span.trace_id && span.parent_span_id == Some(p.span_id)
                    }),
                    id,
                    format!("child of {parent:?}"),
                    parent_span.map_or_else(
                        || format!("the span of {parent:?}, which matched nothing"),
                        |p| format!("trace {} parent {}", p.trace_id, p.span_id),
                    ),
                    || {
                        format!(
                            "trace {} parent {:?}",
                            span.trace_id,
                            span.parent_span_id.map(|p| p.to_string())
                        )
                    },
                    None,
                );
            }
        }
        if let Some(trace) = e.trace_id {
            self.check(
                span.trace_id == trace,
                id,
                "trace id from the propagated traceparent",
                trace.to_string(),
                || span.trace_id.to_string(),
                Some("9.1"),
            );
        }
        if let Some(parent) = e.parent_span_id {
            self.check(
                span.parent_span_id == Some(parent),
                id,
                "parent span id from the propagated traceparent",
                parent.to_string(),
                || format!("{:?}", span.parent_span_id.map(|p| p.to_string())),
                Some("9.1"),
            );
        }
        if let Some(ms) = e.min_duration_ms {
            let micros = span.end.micros().saturating_sub(span.start.micros());
            self.check(
                micros >= i64::try_from(ms.saturating_mul(1000)).unwrap_or(i64::MAX),
                id,
                "duration",
                format!("at least {ms} ms"),
                || format!("{} ms", micros / 1000),
                None,
            );
        }
        for (key, assertion) in &e.attributes {
            self.assert(id, span, key, assertion, None);
        }
        for (field, expected) in &e.fields {
            self.field(id, span, field, expected, None);
        }
        if self.input.case.scenario.kind == Kind::Mcp {
            self.outcome(id, span, &e.outcome.unwrap_or(Outcome::OK));
        }
    }

    fn assert(
        &mut self,
        id: Option<&str>,
        span: &RecordedSpan,
        key: &str,
        assertion: &Assertion,
        contract: Option<&str>,
    ) {
        let value = get(&span.attributes, key);
        let label = id.unwrap_or(span.name.as_str()).to_owned();
        let id = Some(label.as_str());
        let check = format!("attribute {key}");
        match assertion {
            Assertion::Optional(inner) => {
                if value.is_some() {
                    self.assert(id, span, key, inner, contract);
                }
            }
            Assertion::Absent => {
                self.check(
                    value.is_none(),
                    id,
                    check,
                    "absent",
                    || show(value),
                    contract,
                );
            }
            Assertion::Present => {
                self.check(
                    value.is_some(),
                    id,
                    check,
                    "present",
                    || show(value),
                    contract,
                );
            }
            Assertion::Equals(expected) => {
                let want = self.resolve(expected);
                let ok = want.as_ref().is_some_and(|w| equals(value, w));
                let expected =
                    want.map_or_else(|| "an unset variable".to_owned(), |w| w.to_string());
                self.check(ok, id, check, expected, || show(value), contract);
            }
            Assertion::Matches(regex) => {
                let ok = value
                    .and_then(Value::as_str)
                    .is_some_and(|s| regex.is_match(s));
                self.check(
                    ok,
                    id,
                    check,
                    format!("matches {}", regex.as_str()),
                    || show(value),
                    contract,
                );
            }
            Assertion::Contains(parts) => {
                let text = value.and_then(Value::as_str).unwrap_or("");
                let missing: Vec<&str> = parts
                    .iter()
                    .map(String::as_str)
                    .filter(|p| !text.contains(p))
                    .collect();
                self.check(
                    value.and_then(Value::as_str).is_some() && missing.is_empty(),
                    id,
                    check,
                    format!("a string containing {parts:?}"),
                    || {
                        if value.is_none() {
                            "absent".to_owned()
                        } else {
                            format!("missing {missing:?}")
                        }
                    },
                    contract,
                );
            }
            Assertion::Json(want) => {
                let parsed = value
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Json>(s).ok());
                self.check(
                    parsed.as_ref() == Some(want),
                    id,
                    check,
                    format!("JSON text equal to {want}"),
                    || show(value),
                    contract,
                );
            }
            Assertion::Usd(want) => {
                let got = match value {
                    Some(Value::String(s)) => NanoUsd::from_usd_decimal(s),
                    Some(Value::Double(d)) => Some(NanoUsd::from_usd_f64(*d)),
                    Some(Value::Int(i)) => Some(NanoUsd::from_usd_f64(*i as f64)),
                    _ => None,
                };
                self.check(
                    got == Some(*want),
                    id,
                    check,
                    format!("{} nano-USD", want.nanos()),
                    || show(value),
                    Some("3.4"),
                );
            }
            Assertion::MrtrHash(step, pointer) => {
                let observed = self
                    .response(step)
                    .and_then(|r| r.message.as_ref())
                    .and_then(|m| m.pointer(pointer))
                    .and_then(Json::as_str)
                    .map(str::to_owned);
                let want = observed
                    .as_deref()
                    .map(|s| mrtr_hash(&self.input.suite.constants.keys.secret_key, s));
                self.check(
                    want.is_some() && value.and_then(Value::as_str) == want.as_deref(),
                    id,
                    check,
                    want.unwrap_or_else(|| {
                        format!("the hash of {step}{pointer}, which the runner did not report")
                    }),
                    || show(value),
                    Some("9.6"),
                );
            }
            Assertion::SessionHash(step) => {
                let observed = self
                    .response(step)
                    .and_then(|r| r.session_id.clone())
                    .filter(|s| !s.is_empty());
                let want = observed.as_deref().map(session_hash);
                self.check(
                    want.is_some() && value.and_then(Value::as_str) == want.as_deref(),
                    id,
                    check,
                    want.unwrap_or_else(|| {
                        format!("the hash of the session id of step {step}, which the runner did not report")
                    }),
                    || show(value),
                    Some("9.4"),
                );
            }
        }
    }

    fn response(&self, step: &str) -> Option<&'a ResponseEvent> {
        self.input.responses.iter().find(|r| r.step == step)
    }

    fn field(
        &mut self,
        id: Option<&str>,
        span: &RecordedSpan,
        field: &str,
        expected: &Expected,
        contract: Option<&str>,
    ) {
        let got = self.registry.derive(field, span);
        let want = self.resolve(expected);
        let contract = contract
            .map(str::to_owned)
            .or_else(|| self.registry.fields.get(field).map(|(c, _)| c.clone()));
        let label = id.unwrap_or(span.name.as_str()).to_owned();
        // The same field in the record the real normalizer made of the span.
        let recorded = self.records.field(field, span);
        self.check(
            want.is_some() && got == want,
            Some(label.as_str()),
            format!("field {field}"),
            want.as_ref()
                .map_or_else(|| "an unset variable".to_owned(), Json::to_string),
            || got.map_or_else(|| "nothing derivable".to_owned(), |g| g.to_string()),
            contract.as_deref(),
        );
        if let Some(recorded) = recorded {
            self.check(
                want.is_some() && recorded == want,
                Some(label.as_str()),
                format!("field {field} of the normalizer's record"),
                want.map_or_else(|| "an unset variable".to_owned(), |w| w.to_string()),
                || recorded.map_or_else(|| "no value".to_owned(), |r| r.to_string()),
                contract.as_deref(),
            );
        }
    }

    fn classify(&self, span: &RecordedSpan) -> McpClassification {
        let attrs = &span.attributes;
        let text = |key: &str| get(attrs, key).and_then(Value::as_str);
        let error_type = text("error.type");
        let http_status = match get(attrs, "http.response.status_code") {
            Some(Value::Int(i)) => u16::try_from(*i).ok(),
            _ => None,
        };
        // A numeric `error.type` that is the HTTP status is that status, not a JSON-RPC code.
        let jsonrpc_code = text("rpc.response.status_code")
            .and_then(|s| s.parse::<i32>().ok())
            .or_else(|| {
                error_type
                    .and_then(|t| t.parse::<i32>().ok())
                    .filter(|code| http_status.is_none_or(|status| i32::from(status) != *code))
            });
        let explicit_class = text("crucible.mcp.error.class").and_then(ErrorClass::parse);
        let era = match self
            .registry
            .derive("era", span)
            .as_ref()
            .and_then(Json::as_str)
        {
            Some("modern") => RecordEra::Modern,
            _ => RecordEra::Legacy,
        };
        let side = match self
            .registry
            .derive("side", span)
            .as_ref()
            .and_then(Json::as_str)
        {
            Some("client") => McpSide::Client,
            _ => McpSide::Server,
        };
        let input = McpErrorInput {
            era,
            side,
            jsonrpc_code,
            is_error: error_type == Some("tool_error"),
            http_status,
            cancel_source: text("crucible.mcp.cancel.source")
                .and_then(CancelSource::parse)
                .unwrap_or(CancelSource::None),
            // Only the capture source can observe a deadline or a broken transport: its explicit class is
            // that observation.
            timed_out: matches!(
                get(attrs, "crucible.mcp.timed_out"),
                Some(Value::Bool(true))
            ) || explicit_class == Some(ErrorClass::Timeout),
            transport_failure: explicit_class == Some(ErrorClass::Transport),
            unhandled_exception: error_type
                .is_some_and(|t| t != "tool_error" && t.parse::<i64>().is_err()),
            annotations: McpErrorAnnotations {
                explicit_class,
                subclass: text("crucible.mcp.error.subclass")
                    .and_then(ErrorSubclass::parse)
                    .unwrap_or(ErrorSubclass::None),
                ..McpErrorAnnotations::NONE
            },
        };
        classify_mcp_error(&input)
    }

    /// Checks the classification of a span: the one in the record the real normalizer made of it. The
    /// driver's own reading of the span's keys must give the same, so that the suite and the normalizer cannot
    /// drift apart.
    fn outcome(&mut self, id: Option<&str>, span: &RecordedSpan, want: &Outcome) {
        let read = self.classify(span);
        let Some(got) = self.records.op(span).map(|op| McpClassification {
            error_class: op.error_class,
            error_subclass: op.error_subclass,
            status: op.status,
            caller_fault: op.caller_fault,
        }) else {
            // `normalizer` has reported the missing record.
            return;
        };
        self.check(
            got == read,
            id,
            "the normalizer's record classifies as the driver reads the span's keys",
            format!("{read:?}"),
            || format!("{got:?}"),
            Some("9.2"),
        );
        self.check(
            got.status == want.status,
            id,
            "outcome status of the normalizer's record (contract 9.2 classifier)",
            want.status.as_str(),
            || got.status.as_str().to_owned(),
            Some("9.2"),
        );
        if let Some(class) = want.error_class {
            self.check(
                got.error_class == class,
                id,
                "outcome error_class",
                class.as_str(),
                || got.error_class.as_str().to_owned(),
                Some("9.2"),
            );
        }
        if let Some(subclass) = want.error_subclass {
            self.check(
                got.error_subclass == subclass,
                id,
                "outcome error_subclass",
                subclass.as_str(),
                || got.error_subclass.as_str().to_owned(),
                Some("9.2"),
            );
        }
        if let Some(fault) = want.caller_fault {
            self.check(
                got.caller_fault == fault,
                id,
                "outcome caller_fault",
                fault.to_string(),
                || got.caller_fault.to_string(),
                Some("9.2"),
            );
        }
    }

    fn rule_applies(&self, rule: &Rule, span: &RecordedSpan) -> bool {
        let all = |conditions: &[(String, Json)]| {
            conditions
                .iter()
                .all(|(key, want)| equals(get(&span.attributes, key), want))
        };
        rule.when.holds(&self.facts)
            && all(&rule.only_if)
            && (rule.unless.is_empty() || !all(&rule.unless))
    }

    fn rules(&mut self) {
        let mcp = self.input.case.scenario.kind == Kind::Mcp;
        let registry = self.registry;
        let rules = if mcp {
            &registry.mcp_rules
        } else {
            &registry.agent_rules
        };
        let spans = self.spans.clone();
        for (_, span) in spans.iter().filter(|(_, s)| !mcp || is_mcp(s)) {
            for rule in rules {
                if !self.rule_applies(rule, span) {
                    continue;
                }
                match &rule.target {
                    Target::Key(key) => {
                        self.assert(
                            None,
                            span,
                            key,
                            &rule.assertion,
                            Some(rule.contract.as_str()),
                        );
                    }
                    Target::Field(field) => {
                        if let Assertion::Equals(expected) = &rule.assertion {
                            self.field(None, span, field, expected, Some(rule.contract.as_str()));
                        }
                    }
                }
            }
        }
    }

    fn golden_extras(&mut self, bound: &BTreeMap<String, &'a RecordedSpan>) {
        let Some(golden) = self.golden else {
            return;
        };
        let mcp = self.input.case.scenario.kind == Kind::Mcp;
        let spans = self.spans.clone();
        for (_, span) in spans.iter().filter(|(_, s)| !mcp || is_mcp(s)) {
            for (key, assertion) in &golden.every_span.attributes {
                self.assert(None, span, key, assertion, Some("10.6"));
            }
            for (field, expected) in &golden.every_span.fields {
                self.field(None, span, field, expected, None);
            }
        }
        for select in &golden.absent {
            let count = spans
                .iter()
                .filter(|(_, s)| self.selects(s, select))
                .count();
            let described = self.describe(select);
            self.check(
                count == 0,
                None,
                format!("no span with {described}"),
                "0",
                || count.to_string(),
                None,
            );
        }
        for (name, members) in &golden.checkpoints {
            let at = self
                .input
                .events
                .iter()
                .position(|e| matches!(e, Event::Checkpoint(n) if n == name));
            for member in members {
                let arrived = bound.get(member).and_then(|span| {
                    spans
                        .iter()
                        .find(|(_, s)| std::ptr::eq(*s, *span))
                        .map(|(i, _)| *i)
                });
                self.check(
                    matches!((arrived, at), (Some(a), Some(c)) if a < c),
                    Some(member.as_str()),
                    format!("arrived before checkpoint {name:?}"),
                    "exported before the checkpoint request",
                    || match (arrived, at) {
                        (_, None) => "the checkpoint never arrived".to_owned(),
                        (None, _) => "the span never arrived".to_owned(),
                        _ => "exported after the checkpoint".to_owned(),
                    },
                    None,
                );
            }
        }
    }
}

/// A record's content fields, for a finding.
fn describe_content(
    (captured, rate, truncation, args, result): (bool, f32, Truncation, &[u8], &[u8]),
) -> String {
    format!(
        "captured {captured}, sample rate {rate}, truncation {truncation}, arguments {:?}, result {:?}",
        String::from_utf8_lossy(args),
        String::from_utf8_lossy(result)
    )
}

/// Equal values: strings, booleans, numbers (by value) and string arrays.
fn equals(value: Option<&Value>, want: &Json) -> bool {
    match (value, want) {
        (Some(Value::String(s)), Json::String(w)) => s == w,
        (Some(Value::Bool(b)), Json::Bool(w)) => b == w,
        // Numbers compare by value: the registry type check is separate, and some exporters send an integral
        // double as an integer.
        (Some(Value::Int(i)), Json::Number(n)) if n.is_f64() => n.as_f64() == Some(*i as f64),
        (Some(Value::Int(i)), Json::Number(n)) => n.as_i64() == Some(*i),
        (Some(Value::Double(d)), Json::Number(n)) => n.as_f64() == Some(*d),
        (Some(Value::Array(items)), Json::Array(w)) => {
            items.len() == w.len()
                && items
                    .iter()
                    .zip(w)
                    .all(|(item, want)| equals(Some(item), want))
        }
        _ => false,
    }
}

/// Equal JSON values, numbers compared by value: an SDK may send the score `1` as `1.0`.
fn json_equal(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_equal(x, y))
        }
        (Json::Object(x), Json::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_equal(v, w)))
        }
        _ => a == b,
    }
}

/// Where in a span a string occurs, for the taint report.
fn locate(span: &RecordedSpan, needle: &str) -> Option<String> {
    let has = |value: &Value| value.to_json().to_string().contains(needle);
    if span.name.contains(needle) {
        return Some(format!("the name of span {:?}", span.name));
    }
    if span.status_message.contains(needle) {
        return Some(format!("the status message of span {:?}", span.name));
    }
    let keyed = |attrs: &[(String, Value)], what: &str| {
        attrs
            .iter()
            .find(|(k, v)| k.contains(needle) || has(v))
            .map(|(k, _)| format!("{what} {k} of span {:?}", span.name))
    };
    keyed(&span.attributes, "attribute")
        .or_else(|| keyed(&span.resource, "resource attribute"))
        .or_else(|| {
            span.events
                .iter()
                .find_map(|e| keyed(&e.attributes, &format!("event {:?} attribute", e.name)))
        })
}
