//! A JSON tree that keeps member order and repeated members, for the decoders that read inside a value or build
//! one (member paths, objects, indexed families, tool calls).
//!
//! - Parsing uses `serde_json`, so text nested beyond its recursion limit (128 levels) does not parse.
//! - Rendering is compact and follows the rules of [`crate::json`], so equal trees give equal bytes.
//! - An integer outside the 64-bit ranges is read as a double.

use core::fmt;

use crucible_otlp::AnyValueView;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::json;

/// One JSON value.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Node {
    Null,
    Bool(bool),
    Int(i64),
    /// An integer above `i64::MAX`.
    UInt(u64),
    Float(f64),
    Str(String),
    Arr(Vec<Self>),
    /// Members in wire order; a repeated name is kept.
    Obj(Vec<(String, Self)>),
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Node, E> {
        Ok(Node::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Node, E> {
        Ok(Node::Int(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Node, E> {
        Ok(i64::try_from(v).map_or(Node::UInt(v), Node::Int))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Node, E> {
        Ok(Node::Float(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Node, E> {
        Ok(Node::Str(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Node, E> {
        Ok(Node::Str(v))
    }

    fn visit_unit<E>(self) -> Result<Node, E> {
        Ok(Node::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Node::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let mut members = Vec::new();
        while let Some(member) = map.next_entry()? {
            members.push(member);
        }
        Ok(Node::Obj(members))
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(NodeVisitor)
    }
}

/// Whether `s` can start a JSON value, checked on its first byte after whitespace: a cheap reject for plain text.
fn may_be_json(s: &str) -> bool {
    s.bytes()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| {
            matches!(
                b,
                b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'
            )
        })
}

impl Node {
    /// The tree of `s` when it is JSON text.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        if !may_be_json(s) {
            return None;
        }
        serde_json::from_str(s).ok()
    }

    /// The tree of `s` when it is JSON text of an object or an array.
    pub(crate) fn parse_container(s: &str) -> Option<Self> {
        let first = s.bytes().find(|b| !b.is_ascii_whitespace())?;
        if !matches!(first, b'{' | b'[') {
            return None;
        }
        serde_json::from_str(s).ok()
    }

    /// The tree of an attribute value, by the residual-JSON rules: NaN and the infinities are strings, bytes are
    /// lowercase hex, an unset value is null.
    pub(crate) fn from_view(value: AnyValueView<'_>) -> Self {
        match value {
            AnyValueView::Empty => Self::Null,
            AnyValueView::String(s) => Self::Str(s.to_owned()),
            AnyValueView::Bool(b) => Self::Bool(b),
            AnyValueView::Int(i) => Self::Int(i),
            AnyValueView::Double(d) if d.is_finite() => Self::Float(d),
            AnyValueView::Double(d) => Self::Str(
                if d.is_nan() {
                    "NaN"
                } else if d < 0.0 {
                    "-Infinity"
                } else {
                    "Infinity"
                }
                .to_owned(),
            ),
            AnyValueView::Bytes(b) => {
                let mut hex = Vec::with_capacity(b.len() * 2);
                json::push_hex(&mut hex, b);
                Self::Str(String::from_utf8(hex).unwrap_or_default())
            }
            AnyValueView::Array(items) => Self::Arr(items.map(Self::from_view).collect()),
            AnyValueView::KvList(pairs) => Self::Obj(
                pairs
                    .map(|(k, v)| (k.to_owned(), Self::from_view(v)))
                    .collect(),
            ),
        }
    }

    /// The first member with this name, of an object.
    pub(crate) fn member(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Obj(members) => members.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The value at a member path. Each step selects an object member, or an array element when the step is
    /// decimal digits. A step that meets a string holding JSON text of an object or array reads inside it.
    pub(crate) fn at(&self, path: &[&str]) -> Option<Self> {
        let mut current = self.clone();
        for step in path {
            if let Self::Str(s) = &current {
                current = Self::parse_container(s)?;
            }
            current = match current {
                Self::Obj(members) => members.into_iter().find(|(k, _)| k == step)?.1,
                Self::Arr(mut items) => {
                    if step.is_empty() || !step.bytes().all(|b| b.is_ascii_digit()) {
                        return None;
                    }
                    let index: usize = step.parse().ok()?;
                    if index >= items.len() {
                        return None;
                    }
                    items.swap_remove(index)
                }
                _ => return None,
            };
        }
        Some(current)
    }

    /// The value at a member path, read without the double-encoding rule: every step is an object member.
    pub(crate) fn get(&self, path: &[&str]) -> Option<&Self> {
        path.iter().try_fold(self, |node, step| node.member(step))
    }

    /// The string of a string value.
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Appends the value as compact JSON text.
    pub(crate) fn push(&self, out: &mut Vec<u8>) {
        match self {
            Self::Null => out.extend_from_slice(b"null"),
            Self::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            Self::Int(i) => json::push_i64(out, *i),
            Self::UInt(u) => json::push_u64(out, *u),
            Self::Float(d) => json::push_f64(out, *d),
            Self::Str(s) => json::push_str(out, s),
            Self::Arr(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.push(out);
                }
                out.push(b']');
            }
            Self::Obj(members) => {
                out.push(b'{');
                for (i, (name, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    json::push_str(out, name);
                    out.push(b':');
                    value.push(out);
                }
                out.push(b'}');
            }
        }
    }

    /// The value as compact JSON text.
    pub(crate) fn render(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.push(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn text(node: &Node) -> String {
        String::from_utf8(node.render()).unwrap()
    }

    #[test]
    fn parsing_keeps_member_order_and_repeats() {
        let node = Node::parse(r#" {"b": 1, "a": [true, null, 1.5, "x"], "b": -2} "#).unwrap();
        assert_eq!(text(&node), r#"{"b":1,"a":[true,null,1.5,"x"],"b":-2}"#);
        assert_eq!(node.member("b"), Some(&Node::Int(1)));
        assert_eq!(
            Node::parse("18446744073709551615"),
            Some(Node::UInt(u64::MAX))
        );
        assert_eq!(Node::parse("plain text"), None);
        assert_eq!(Node::parse("{"), None);
        assert_eq!(Node::parse_container("3"), None);
        assert_eq!(
            Node::parse_container("[3]"),
            Some(Node::Arr(vec![Node::Int(3)]))
        );
    }

    #[test]
    fn deep_text_does_not_parse() {
        let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
        assert_eq!(Node::parse(&deep), None);
    }

    #[test]
    fn paths_step_through_members_indexes_and_encoded_text() {
        let node = Node::parse(r#"{"a":{"b":[10,20]},"s":"[{\"name\":\"p\"}]"}"#).unwrap();
        assert_eq!(node.at(&["a", "b", "1"]), Some(Node::Int(20)));
        assert_eq!(node.at(&["a", "b", "2"]), None);
        assert_eq!(node.at(&["a", "b", "x"]), None);
        assert_eq!(node.at(&["s", "0", "name"]), Some(Node::Str("p".into())));
        assert_eq!(node.at(&["missing"]), None);
        assert_eq!(node.at(&[]), Some(node.clone()));
        assert_eq!(
            node.get(&["a", "b"]),
            Some(&Node::Arr(vec![Node::Int(10), Node::Int(20)]))
        );
    }

    proptest! {
        /// Rendered text parses back to the same tree.
        #[test]
        fn rendering_round_trips(s in "\\PC{0,20}", i in any::<i64>(), b in any::<bool>()) {
            let node = Node::Obj(vec![
                (s.clone(), Node::Arr(vec![Node::Int(i), Node::Bool(b), Node::Null])),
                ("k".into(), Node::Str(s)),
            ]);
            prop_assert_eq!(Node::parse(&text(&node)), Some(node));
        }

        /// No text makes the parser panic.
        #[test]
        fn parsing_is_total(s in "\\PC{0,64}") {
            let _ = Node::parse(&s);
            let _ = Node::parse_container(&s);
        }
    }
}
