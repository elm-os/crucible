//! JSON text for export lines: a writer that keeps members in the order it is given them, the embedding of a
//! stored body, and a reader of stored JSON that keeps members in order, duplicates included (residual attributes
//! keep wire order, contract 10.5).

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The largest integer a JSON number carries exactly (contract 1.3).
pub(crate) const SAFE_MAX: u64 = (1 << 53) - 1;

/// Appends the lowercase hex form of `bytes`.
pub(crate) fn hex(out: &mut Vec<u8>, bytes: &[u8]) {
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0x0f)]);
    }
}

/// Appends `s` as a JSON string: `"`, `\` and the control characters escaped, everything else as UTF-8.
pub(crate) fn string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let short: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x00..=0x1f => b"",
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        if short.is_empty() {
            out.extend_from_slice(b"\\u00");
            hex(out, &[b]);
        } else {
            out.extend_from_slice(short);
        }
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// Appends bytes that should be UTF-8 text as a JSON string; an invalid sequence becomes U+FFFD.
pub(crate) fn text(out: &mut Vec<u8>, bytes: &[u8]) {
    string(out, &String::from_utf8_lossy(bytes));
}

/// Appends an unsigned integer, saturated at 2^53 - 1. Returns whether it saturated.
pub(crate) fn uint(out: &mut Vec<u8>, value: u64) -> bool {
    out.extend_from_slice(value.min(SAFE_MAX).to_string().as_bytes());
    value > SAFE_MAX
}

/// Appends a signed integer, saturated at plus or minus 2^53 - 1. Returns whether it saturated.
pub(crate) fn int(out: &mut Vec<u8>, value: i64) -> bool {
    let max = SAFE_MAX as i64;
    out.extend_from_slice(value.clamp(-max, max).to_string().as_bytes());
    value.unsigned_abs() > SAFE_MAX
}

/// Appends a double as the shortest text that reads back as it; NaN and the infinities as their OTLP/JSON
/// spellings, as strings.
pub(crate) fn float(out: &mut Vec<u8>, value: f64) {
    if value.is_nan() {
        string(out, "NaN");
    } else if value.is_infinite() {
        string(out, if value < 0.0 { "-Infinity" } else { "Infinity" });
    } else if serde_json::to_writer(&mut *out, &value).is_err() {
        out.extend_from_slice(b"null");
    }
}

/// Appends a cost in nano-USD as a USD number (contract 1.3): `nano / 1e9`, the nearest double.
pub(crate) fn usd(out: &mut Vec<u8>, nanos: i64) {
    float(out, nanos as f64 / 1e9);
}

/// Appends a stored body (contract 12.8): as a JSON string when it is opaque (cut by a cap) or not valid JSON,
/// the empty string when it is empty, else the JSON itself without the whitespace between its tokens, so that it
/// stays on one line.
pub(crate) fn body(out: &mut Vec<u8>, bytes: &[u8], opaque: bool) {
    if opaque || bytes.is_empty() || !valid(bytes) {
        text(out, bytes);
    } else {
        minify(out, bytes);
    }
}

/// Whether `bytes` are one JSON value.
pub(crate) fn valid(bytes: &[u8]) -> bool {
    serde_json::from_slice::<de::IgnoredAny>(bytes).is_ok()
}

/// Appends valid JSON without the whitespace outside its strings.
pub(crate) fn minify(out: &mut Vec<u8>, json: &[u8]) {
    let (mut in_string, mut escaped) = (false, false);
    for &b in json {
        if in_string {
            out.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else if !matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
            out.push(b);
            in_string = b == b'"';
        }
    }
}

/// An object being written: members in the order they are added.
pub(crate) struct Obj<'a> {
    out: &'a mut Vec<u8>,
    first: bool,
}

impl<'a> Obj<'a> {
    pub(crate) fn new(out: &'a mut Vec<u8>) -> Self {
        out.push(b'{');
        Self { out, first: true }
    }

    /// Starts a member and returns the buffer its value is written to.
    pub(crate) fn key(&mut self, key: &str) -> &mut Vec<u8> {
        if !self.first {
            self.out.push(b',');
        }
        self.first = false;
        string(self.out, key);
        self.out.push(b':');
        self.out
    }

    pub(crate) fn str(&mut self, key: &str, value: &str) {
        string(self.key(key), value);
    }

    pub(crate) fn raw(&mut self, key: &str, json: &[u8]) {
        self.key(key).extend_from_slice(json);
    }

    pub(crate) fn bool(&mut self, key: &str, value: bool) {
        self.raw(key, if value { b"true" } else { b"false" });
    }

    pub(crate) fn null(&mut self, key: &str) {
        self.raw(key, b"null");
    }

    pub(crate) fn display(&mut self, key: &str, value: impl fmt::Display) {
        self.str(key, &value.to_string());
    }

    /// A list of strings.
    pub(crate) fn strs<S: AsRef<str>>(&mut self, key: &str, values: &[S]) {
        let out = self.key(key);
        out.push(b'[');
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            string(out, value.as_ref());
        }
        out.push(b']');
    }

    pub(crate) fn end(self) {
        self.out.push(b'}');
    }
}

/// A stored JSON value read with its object members in order, duplicates kept, and integers told apart from
/// doubles (the residual writer gives every double a fraction or an exponent).
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// The value of `bytes`, when they are one JSON value.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }

    /// Appends the value as JSON text, members in order.
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        match self {
            Self::Null => out.extend_from_slice(b"null"),
            Self::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            Self::Int(i) => out.extend_from_slice(i.to_string().as_bytes()),
            Self::UInt(u) => out.extend_from_slice(u.to_string().as_bytes()),
            Self::Float(f) => float(out, *f),
            Self::Str(s) => string(out, s),
            Self::Arr(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write(out);
                }
                out.push(b']');
            }
            Self::Obj(members) => {
                let mut obj = Obj::new(out);
                for (key, value) in members {
                    value.write(obj.key(key));
                }
                obj.end();
            }
        }
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Int(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(i64::try_from(v).map_or(Json::UInt(v), Json::Int))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
        Ok(Json::Float(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut members = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            members.push((key, value));
        }
        Ok(Json::Obj(members))
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn written(f: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        f(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_body_is_embedded_when_it_is_json_and_a_string_otherwise() {
        assert_eq!(written(|o| body(o, b"{ \"a\" : [1, 2] }", false)), r#"{"a":[1,2]}"#);
        assert_eq!(written(|o| body(o, b"{\n \"a b\": \"c\\\" d\"\n}", false)), r#"{"a b":"c\" d"}"#);
        assert_eq!(written(|o| body(o, b"plain text", false)), r#""plain text""#);
        assert_eq!(written(|o| body(o, b"", false)), r#""""#);
        assert_eq!(written(|o| body(o, b"{\"a\":1}", true)), r#""{\"a\":1}""#);
        assert_eq!(written(|o| body(o, b"{\"cut", false)), r#""{\"cut""#);
    }

    #[test]
    fn members_keep_their_order_and_duplicates() {
        let parsed = Json::parse(br#"{"b":1,"a":2.0,"b":"x","n":null,"l":[true,{"z":1e+300}]}"#).unwrap();
        assert_eq!(written(|o| parsed.write(o)), r#"{"b":1,"a":2.0,"b":"x","n":null,"l":[true,{"z":1e+300}]}"#);
        let Json::Obj(members) = parsed else { panic!("an object") };
        assert_eq!(members[0].1, Json::Int(1));
        assert_eq!(members[1].1, Json::Float(2.0));
    }

    proptest! {
        #[test]
        fn a_string_reads_back_as_itself(s in "\\PC*|[\\x00-\\x1f\"\\\\]*") {
            let text = written(|o| string(o, &s));
            prop_assert!(!text.contains('\n'));
            prop_assert_eq!(serde_json::from_str::<String>(&text).unwrap(), s);
        }

        #[test]
        fn any_body_becomes_one_line_of_valid_json(bytes in prop::collection::vec(any::<u8>(), 0..64), opaque in any::<bool>()) {
            let text = written(|o| body(o, &bytes, opaque));
            prop_assert!(!text.contains('\n'));
            prop_assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
        }

        #[test]
        fn minifying_keeps_the_value(value in prop::collection::btree_map("[a-z \\n]{0,4}", "[a-z \\n\"]{0,6}", 0..4)) {
            let pretty = serde_json::to_vec_pretty(&value).unwrap();
            let mut out = Vec::new();
            minify(&mut out, &pretty);
            prop_assert!(!out.contains(&b'\n'));
            let back: std::collections::BTreeMap<String, String> = serde_json::from_slice(&out).unwrap();
            prop_assert_eq!(back, value);
        }
    }
}
