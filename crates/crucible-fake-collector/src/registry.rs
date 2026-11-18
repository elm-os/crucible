//! `registry.json`: every wire key with its type, the record fields the normalizer derives from them, the forbidden
//! keys, the export contract, the hash vectors and the rules that apply to every span of a case.

use std::collections::BTreeMap;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value as Json;

use crate::collector::RecordedSpan;
use crate::golden::{Assertion, When};
use crate::suite::{SuiteError, check_schema, parse, schema};
use crate::value::{Value, get};

/// The modern era starts at this protocol version (contract 9.1).
pub const MODERN_VERSION: &str = "2026-07-28";

/// One registry key or key prefix.
#[derive(Debug)]
pub struct KeySpec {
    /// The key, or the prefix when `prefix` is true.
    pub name: String,
    /// Whether `name` is a prefix.
    pub prefix: bool,
    /// Accepted types: `string`, `int`, `double`, `number` (int or double), `bool`, `string_array`.
    pub types: Vec<String>,
    /// Accepted string values; empty means any.
    pub values: Vec<String>,
    /// A pattern every string value matches.
    pub pattern: Option<Regex>,
    /// Inclusive numeric bounds.
    pub min: Option<f64>,
    /// Inclusive numeric bounds.
    pub max: Option<f64>,
    /// The most bytes a string value may hold.
    pub max_bytes: Option<usize>,
    /// The contract section.
    pub contract: String,
}

/// Where a field's value comes from, tried in order.
#[derive(Debug)]
pub enum Source {
    /// A span attribute, optionally mapped.
    Span(String, Option<BTreeMap<String, String>>),
    /// A resource attribute.
    Resource(String),
    /// The span kind, mapped by name.
    SpanKind(BTreeMap<String, String>),
    /// A constant.
    Default(Json),
    /// Another field.
    Field(String),
    /// `modern` when the version key is at or after [`MODERN_VERSION`], else `legacy`.
    EraFromVersion(String),
    /// A constant when a span attribute is present.
    IfPresent(String, Json),
}

/// A rule applied to every span of a kind of case.
#[derive(Debug)]
pub struct Rule {
    /// The key or the field it checks.
    pub target: Target,
    /// The assertion.
    pub assertion: Assertion,
    /// Case conditions.
    pub when: When,
    /// Span attributes that must all equal these values for the rule to apply.
    pub only_if: Vec<(String, Json)>,
    /// Span attributes that, when all equal to these values, exempt the span.
    pub unless: Vec<(String, Json)>,
    /// The contract section.
    pub contract: String,
}

/// What a rule or a golden field check reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A span attribute.
    Key(String),
    /// A derived field.
    Field(String),
}

/// The export contract (contract 9.10).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSpec {
    /// The contract section.
    pub contract: String,
    /// The path.
    pub path: String,
    /// The media type.
    pub media_type: String,
    /// Accepted `Content-Encoding` values (`identity` means none).
    pub encodings: Vec<String>,
}

/// Forbidden keys (contract 9.1).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forbidden {
    /// The contract section.
    pub contract: String,
    /// Exact keys.
    pub keys: Vec<String>,
    /// Key prefixes.
    pub prefixes: Vec<String>,
}

/// A hash rule with test vectors.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HashRule {
    /// The contract section.
    pub contract: String,
    /// The rule in words.
    pub rule: String,
    /// Inputs and outputs (and keys, for keyed hashes).
    pub vectors: Vec<HashVector>,
}

/// One hash test vector.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HashVector {
    /// The key of a keyed hash.
    #[serde(default)]
    pub key: Option<String>,
    /// The input.
    pub input: String,
    /// The expected output.
    pub output: String,
}

/// The hash rules.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hashes {
    /// Contract 9.4.
    pub session_id: HashRule,
    /// Contract 9.6.
    pub mrtr_state: HashRule,
}

/// The parsed registry.
#[derive(Debug)]
pub struct Registry {
    /// Keys and prefixes.
    pub keys: Vec<KeySpec>,
    /// Derived fields and their sources.
    pub fields: BTreeMap<String, (String, Vec<Source>)>,
    /// Forbidden keys.
    pub forbidden: Forbidden,
    /// The export contract.
    pub export: ExportSpec,
    /// Hash rules.
    pub hashes: Hashes,
    /// Rules for every MCP span of an MCP case.
    pub mcp_rules: Vec<Rule>,
    /// Rules for every span of an agent case.
    pub agent_rules: Vec<Rule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    schema: String,
    keys: Vec<KeyFile>,
    fields: BTreeMap<String, FieldFile>,
    forbidden: Forbidden,
    export: ExportSpec,
    hashes: Hashes,
    mcp_rules: Vec<RuleFile>,
    agent_rules: Vec<RuleFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(rename = "type")]
    types: OneOrMany,
    #[serde(default, rename = "enum")]
    values: Vec<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    max_bytes: Option<usize>,
    contract: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldFile {
    contract: String,
    sources: Vec<SourceFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFile {
    #[serde(default)]
    span: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    span_kind: Option<BTreeMap<String, String>>,
    #[serde(default)]
    default: Option<Json>,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    era_from_version: Option<String>,
    #[serde(default)]
    if_present: Option<String>,
    #[serde(default)]
    value: Option<Json>,
    #[serde(default)]
    map: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    field: Option<String>,
    assert: Json,
    #[serde(default)]
    when: Option<Json>,
    #[serde(default, rename = "if")]
    only_if: BTreeMap<String, Json>,
    #[serde(default)]
    unless: BTreeMap<String, Json>,
    contract: String,
}

const TYPES: [&str; 6] = ["string", "int", "double", "number", "bool", "string_array"];

impl Registry {
    /// Parses and checks `registry.json`.
    pub fn parse(file: &str, bytes: &[u8]) -> Result<Self, SuiteError> {
        let raw: RegistryFile = parse(file, bytes)?;
        check_schema(file, &raw.schema, schema::REGISTRY)?;
        let err = |message: String| SuiteError::new(file, message);
        let mut keys = Vec::with_capacity(raw.keys.len());
        for k in raw.keys {
            let (name, prefix) = match (k.key, k.prefix) {
                (Some(key), None) => (key, false),
                (None, Some(prefix)) => (prefix, true),
                _ => {
                    return Err(err(
                        "each keys entry has exactly one of key and prefix".to_owned()
                    ));
                }
            };
            let types = match k.types {
                OneOrMany::One(t) => vec![t],
                OneOrMany::Many(ts) => ts,
            };
            if let Some(bad) = types.iter().find(|t| !TYPES.contains(&t.as_str())) {
                return Err(err(format!("key {name:?} has unknown type {bad:?}")));
            }
            let pattern = k
                .pattern
                .map(|p| Regex::new(&p))
                .transpose()
                .map_err(|e| err(format!("key {name:?}: pattern: {e}")))?;
            keys.push(KeySpec {
                name,
                prefix,
                types,
                values: k.values,
                pattern,
                min: k.min,
                max: k.max,
                max_bytes: k.max_bytes,
                contract: k.contract,
            });
        }
        let mut fields = BTreeMap::new();
        for (name, f) in raw.fields {
            let sources = f
                .sources
                .into_iter()
                .map(|s| source(&name, s))
                .collect::<Result<Vec<_>, _>>()
                .map_err(err)?;
            fields.insert(name, (f.contract, sources));
        }
        for (name, (_, sources)) in &fields {
            for s in sources {
                if let Source::Field(other) = s
                    && (!fields.contains_key(other) || other == name)
                {
                    return Err(err(format!("field {name:?} reads unknown field {other:?}")));
                }
            }
        }
        let mut registry = Self {
            keys,
            fields,
            forbidden: raw.forbidden,
            export: raw.export,
            hashes: raw.hashes,
            mcp_rules: Vec::new(),
            agent_rules: Vec::new(),
        };
        registry.mcp_rules = raw
            .mcp_rules
            .into_iter()
            .map(|r| registry.rule(r, true))
            .collect::<Result<_, _>>()
            .map_err(err)?;
        registry.agent_rules = raw
            .agent_rules
            .into_iter()
            .map(|r| registry.rule(r, false))
            .collect::<Result<_, _>>()
            .map_err(err)?;
        Ok(registry)
    }

    fn rule(&self, r: RuleFile, mcp: bool) -> Result<Rule, String> {
        let target = match (r.key, r.field) {
            (Some(key), None) => Target::Key(key),
            (None, Some(field)) if self.fields.contains_key(&field) => Target::Field(field),
            (None, Some(field)) => return Err(format!("a rule reads unknown field {field:?}")),
            _ => return Err("each rule has exactly one of key and field".to_owned()),
        };
        let assertion =
            Assertion::parse(&r.assert).map_err(|e| format!("rule on {target:?}: {e}"))?;
        if matches!(target, Target::Field(_)) && !matches!(assertion, Assertion::Equals(_)) {
            return Err(format!("rule on {target:?}: a field rule asserts a value"));
        }
        let when =
            When::parse(r.when.as_ref(), mcp).map_err(|e| format!("rule on {target:?}: {e}"))?;
        Ok(Rule {
            target,
            assertion,
            when,
            only_if: r.only_if.into_iter().collect(),
            unless: r.unless.into_iter().collect(),
            contract: r.contract,
        })
    }

    /// The spec of `key`: the exact entry, else the longest matching prefix.
    pub fn key(&self, key: &str) -> Option<&KeySpec> {
        self.keys
            .iter()
            .find(|k| !k.prefix && k.name == key)
            .or_else(|| {
                self.keys
                    .iter()
                    .filter(|k| k.prefix && key.starts_with(&k.name))
                    .max_by_key(|k| k.name.len())
            })
    }

    /// Whether the field exists.
    pub fn has_field(&self, field: &str) -> bool {
        self.fields.contains_key(field)
    }

    /// The value the normalizer would derive for `field` from `span`: a JSON string or boolean, or `None`.
    pub fn derive(&self, field: &str, span: &RecordedSpan) -> Option<Json> {
        self.derive_depth(field, span, 0)
    }

    fn derive_depth(&self, field: &str, span: &RecordedSpan, depth: u8) -> Option<Json> {
        let (_, sources) = self.fields.get(field)?;
        if depth > 8 {
            return None;
        }
        sources.iter().find_map(|source| match source {
            Source::Span(key, map) => mapped(get(&span.attributes, key), map.as_ref()),
            Source::Resource(key) => mapped(get(&span.resource, key), None),
            Source::SpanKind(map) => map.get(span.kind.as_str()).map(|v| Json::String(v.clone())),
            Source::Default(value) => Some(value.clone()),
            Source::Field(other) => self.derive_depth(other, span, depth + 1),
            Source::EraFromVersion(key) => get(&span.attributes, key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(|v| {
                    Json::from(if v >= MODERN_VERSION {
                        "modern"
                    } else {
                        "legacy"
                    })
                }),
            Source::IfPresent(key, value) => get(&span.attributes, key).map(|_| value.clone()),
        })
    }

    /// Checks one attribute against its registry entry. `None` when the key is unknown or the value fits.
    pub fn type_error(&self, key: &str, value: &Value) -> Option<String> {
        let spec = self.key(key)?;
        let type_name = value.type_name();
        let fits = spec.types.iter().any(|t| match t.as_str() {
            "number" => matches!(value, Value::Int(_) | Value::Double(_)),
            other => other == type_name,
        });
        if !fits {
            return Some(format!(
                "{key} is {type_name}; the registry allows {}",
                spec.types.join(" or ")
            ));
        }
        if let Value::String(s) = value {
            if !spec.values.is_empty() && !spec.values.iter().any(|v| v == s) {
                return Some(format!("{key} = {s:?} is not one of {:?}", spec.values));
            }
            if let Some(pattern) = &spec.pattern
                && !pattern.is_match(s)
            {
                return Some(format!("{key} = {s:?} does not match {}", pattern.as_str()));
            }
            if let Some(max) = spec.max_bytes
                && s.len() > max
            {
                return Some(format!("{key} holds {} bytes; the cap is {max}", s.len()));
            }
        }
        let number = match value {
            Value::Int(i) => Some(*i as f64),
            Value::Double(d) => Some(*d),
            _ => None,
        };
        if let Some(n) = number
            && (spec.min.is_some_and(|min| n < min) || spec.max.is_some_and(|max| n > max))
        {
            return Some(format!(
                "{key} = {n} is outside [{:?}, {:?}]",
                spec.min, spec.max
            ));
        }
        None
    }

    /// Whether `key` is forbidden on the wire.
    pub fn is_forbidden(&self, key: &str) -> bool {
        let lower = key.to_ascii_lowercase();
        self.forbidden.keys.contains(&lower)
            || self
                .forbidden
                .prefixes
                .iter()
                .any(|p| lower.starts_with(p.as_str()))
    }
}

fn mapped(value: Option<&Value>, map: Option<&BTreeMap<String, String>>) -> Option<Json> {
    let json = match value? {
        Value::String(s) if s.is_empty() => return None,
        Value::String(s) => Json::String(s.clone()),
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        _ => return None,
    };
    match map {
        None => Some(json),
        Some(map) => json
            .as_str()
            .and_then(|s| map.get(s))
            .map(|v| Json::String(v.clone())),
    }
}

fn source(field: &str, s: SourceFile) -> Result<Source, String> {
    let err = || format!("field {field:?}: each source has exactly one kind");
    let kinds = [
        s.span.is_some(),
        s.resource.is_some(),
        s.span_kind.is_some(),
        s.default.is_some(),
        s.field.is_some(),
        s.era_from_version.is_some(),
        s.if_present.is_some(),
    ];
    if kinds.iter().filter(|k| **k).count() != 1 {
        return Err(err());
    }
    if s.map.is_some() && s.span.is_none() {
        return Err(format!("field {field:?}: map applies to span sources only"));
    }
    if s.value.is_some() != s.if_present.is_some() {
        return Err(format!("field {field:?}: value goes with if_present"));
    }
    Ok(if let Some(key) = s.span {
        Source::Span(key, s.map)
    } else if let Some(key) = s.resource {
        Source::Resource(key)
    } else if let Some(map) = s.span_kind {
        Source::SpanKind(map)
    } else if let Some(value) = s.default {
        Source::Default(value)
    } else if let Some(other) = s.field {
        Source::Field(other)
    } else if let Some(key) = s.era_from_version {
        Source::EraFromVersion(key)
    } else if let (Some(key), Some(value)) = (s.if_present, s.value) {
        Source::IfPresent(key, value)
    } else {
        return Err(err());
    })
}
