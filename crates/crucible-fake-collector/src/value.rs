//! Owned OTLP attribute values, as the collector records them.

use crucible_otlp::{AnyValueView, Attributes as AttributeViews};

/// One attribute value as received.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// No value set.
    Empty,
    /// A string.
    String(String),
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// A double, bit for bit as sent.
    Double(f64),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// An array of values.
    Array(Vec<Value>),
    /// A list of key-value pairs.
    KvList(Attributes),
}

/// Key-value pairs in wire order, duplicates kept.
pub type Attributes = Vec<(String, Value)>;

impl Value {
    pub(crate) fn from_view(view: AnyValueView<'_>) -> Self {
        match view {
            AnyValueView::Empty => Self::Empty,
            AnyValueView::String(s) => Self::String(s.to_owned()),
            AnyValueView::Bool(b) => Self::Bool(b),
            AnyValueView::Int(i) => Self::Int(i),
            AnyValueView::Double(d) => Self::Double(d),
            AnyValueView::Bytes(b) => Self::Bytes(b.to_vec()),
            AnyValueView::Array(items) => Self::Array(items.map(Self::from_view).collect()),
            AnyValueView::KvList(pairs) => Self::KvList(owned(pairs)),
        }
    }

    /// The type name used by the key registry: `string`, `bool`, `int`, `double`, `bytes`, `array`, `kvlist` or
    /// `empty`. An array whose items are all strings is a `string_array`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::String(_) => "string",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Double(_) => "double",
            Self::Bytes(_) => "bytes",
            Self::Array(items) if items.iter().all(|v| matches!(v, Self::String(_))) => {
                "string_array"
            }
            Self::Array(_) => "array",
            Self::KvList(_) => "kvlist",
        }
    }

    /// The string, when the value is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value as JSON, for reports and comparisons: scalars as themselves, bytes as lowercase hex, a double that
    /// JSON cannot hold as its text, arrays as arrays and key-value lists as objects (the first of duplicate keys
    /// wins).
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Self::Empty => J::Null,
            Self::String(s) => J::String(s.clone()),
            Self::Bool(b) => J::Bool(*b),
            Self::Int(i) => J::from(*i),
            Self::Double(d) => {
                serde_json::Number::from_f64(*d).map_or_else(|| J::String(d.to_string()), J::Number)
            }
            Self::Bytes(b) => J::String(b.iter().map(|byte| format!("{byte:02x}")).collect()),
            Self::Array(items) => J::Array(items.iter().map(Self::to_json).collect()),
            Self::KvList(pairs) => {
                let mut object = serde_json::Map::new();
                for (key, value) in pairs {
                    object.entry(key.clone()).or_insert_with(|| value.to_json());
                }
                J::Object(object)
            }
        }
    }
}

/// Copies borrowed attributes.
pub(crate) fn owned(views: AttributeViews<'_>) -> Attributes {
    views
        .map(|(key, value)| (key.to_owned(), Value::from_view(value)))
        .collect()
}

/// The first value of `key`, as a receiver that keeps the first of duplicate keys reads it.
pub fn get<'a>(attributes: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}
