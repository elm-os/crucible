//! Indexed families (contract 10.5): flattened keys `<prefix><i>.<rest>` rebuilt as the JSON array they came from.
//!
//! - The first segment after the prefix is the element index, in decimal digits; a key without one is not part of
//!   the family. The rest is split on `.`: a segment of decimal digits indexes an array, any other names an
//!   object member. A wrapper segment of the layout is dropped when it directly follows an index.
//! - Elements and array items are stored dense in ascending index order (holes are skipped); object members keep
//!   first-seen wire order. Leaves keep their type; a leaf the layout names is parsed when it holds JSON text.
//! - A key whose path meets a value of another shape (a leaf where an object is, or the reverse) is left out, as
//!   is a key with more than [`MAX_DEPTH`] segments. At most [`MAX_KEYS`] keys are read.

use std::collections::BTreeMap;

use crucible_compat::IndexedLayout;
use crucible_otlp::AnyValueView;

use crate::node::Node;

/// The most keys one family reads; further keys stay in the residual attributes.
pub(crate) const MAX_KEYS: usize = 4_096;

/// The most path segments a key may have after its element index.
pub(crate) const MAX_DEPTH: usize = 32;

#[derive(Debug)]
enum Build {
    Empty,
    Leaf(Node),
    Obj(Vec<(String, Self)>),
    Arr(BTreeMap<u64, Self>),
}

#[derive(Clone, Copy, Debug)]
enum Segment<'k> {
    Index(u64),
    Name(&'k str),
}

/// The element index a key suffix starts with, and what follows its dot.
pub(crate) fn split_index(suffix: &str) -> Option<(u64, &str)> {
    let (index, rest) = suffix.split_once('.')?;
    if rest.is_empty() {
        return None;
    }
    Some((number(index)?, rest))
}

fn number(segment: &str) -> Option<u64> {
    if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    segment.parse().ok()
}

fn insert(node: &mut Build, path: &[Segment<'_>], leaf: Node) -> bool {
    let Some((first, rest)) = path.split_first() else {
        if matches!(node, Build::Empty) {
            *node = Build::Leaf(leaf);
            return true;
        }
        return false;
    };
    match *first {
        Segment::Name(name) => {
            if matches!(node, Build::Empty) {
                *node = Build::Obj(Vec::new());
            }
            let Build::Obj(members) = node else {
                return false;
            };
            let at = members
                .iter()
                .position(|(k, _)| k == name)
                .unwrap_or_else(|| {
                    members.push((name.to_owned(), Build::Empty));
                    members.len() - 1
                });
            members
                .get_mut(at)
                .is_some_and(|(_, child)| insert(child, rest, leaf))
        }
        Segment::Index(index) => {
            if matches!(node, Build::Empty) {
                *node = Build::Arr(BTreeMap::new());
            }
            let Build::Arr(items) = node else {
                return false;
            };
            insert(items.entry(index).or_insert(Build::Empty), rest, leaf)
        }
    }
}

fn finish(build: Build) -> Node {
    match build {
        Build::Empty => Node::Null,
        Build::Leaf(node) => node,
        Build::Obj(members) => {
            Node::Obj(members.into_iter().map(|(k, v)| (k, finish(v))).collect())
        }
        Build::Arr(items) => Node::Arr(items.into_values().map(finish).collect()),
    }
}

/// The rebuilt array of a family, and which of its keys were used.
#[derive(Debug)]
pub(crate) struct Rebuilt<O> {
    /// The elements, in ascending index order.
    pub(crate) elements: Vec<Node>,
    /// What supplied each key that entered the array.
    pub(crate) used: Vec<O>,
    /// Whether keys beyond [`MAX_KEYS`] were left out.
    pub(crate) truncated: bool,
}

/// Rebuilds a family. `keys` are `(element index, rest of the key, value, origin)` in wire order, as
/// [`split_index`] splits a key suffix. `None` when no key entered the array.
pub(crate) fn rebuild<'a, O: Copy>(
    layout: &IndexedLayout,
    keys: &[(u64, &'a str, AnyValueView<'a>, O)],
) -> Option<Rebuilt<O>> {
    let mut elements: BTreeMap<u64, Build> = BTreeMap::new();
    let mut used = Vec::new();
    let truncated = keys.len() > MAX_KEYS;
    for (index, rest, value, origin) in keys.iter().take(MAX_KEYS) {
        let mut path: smallvec::SmallVec<[Segment<'_>; 8]> = smallvec::SmallVec::new();
        let mut after_index = true;
        let mut too_deep = false;
        for segment in rest.split('.') {
            if after_index && layout.wrappers.contains(&segment) {
                after_index = false;
                continue;
            }
            if path.len() == MAX_DEPTH {
                too_deep = true;
                break;
            }
            match number(segment) {
                Some(i) => {
                    path.push(Segment::Index(i));
                    after_index = true;
                }
                None => {
                    path.push(Segment::Name(segment));
                    after_index = false;
                }
            }
        }
        if too_deep {
            continue;
        }
        let named = match path.last() {
            Some(Segment::Name(name)) => layout.json_leaves.contains(name),
            _ => false,
        };
        let leaf = match value {
            AnyValueView::String(s) if named => {
                Node::parse(s).unwrap_or_else(|| Node::Str((*s).to_owned()))
            }
            other => Node::from_view(other.clone()),
        };
        if insert(elements.entry(*index).or_insert(Build::Empty), &path, leaf) {
            used.push(*origin);
        }
    }
    // A failed insert can leave an element it created empty; such an element holds nothing.
    let elements: Vec<Node> = elements
        .into_values()
        .filter(|e| !matches!(e, Build::Empty))
        .map(finish)
        .collect();
    (!elements.is_empty()).then_some(Rebuilt {
        elements,
        used,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static PLAIN: IndexedLayout = IndexedLayout {
        wrappers: &[],
        json_leaves: &[],
        tool_calls: None,
        finish_reason: None,
    };

    static WRAPPED: IndexedLayout = IndexedLayout {
        wrappers: &["message", "call"],
        json_leaves: &["schema"],
        tool_calls: None,
        finish_reason: None,
    };

    fn run(
        layout: &IndexedLayout,
        keys: &[(&'static str, AnyValueView<'static>)],
    ) -> (String, Vec<usize>) {
        let split: Vec<_> = keys
            .iter()
            .enumerate()
            .filter_map(|(i, (key, value))| {
                let (index, rest) = split_index(key)?;
                Some((index, rest, value.clone(), i))
            })
            .collect();
        match rebuild(layout, &split) {
            Some(r) => (
                String::from_utf8(Node::Arr(r.elements).render()).unwrap(),
                r.used,
            ),
            None => (String::new(), Vec::new()),
        }
    }

    fn s(text: &'static str) -> AnyValueView<'static> {
        AnyValueView::String(text)
    }

    #[test]
    fn elements_are_dense_in_index_order_with_members_in_wire_order() {
        let (json, used) = run(
            &PLAIN,
            &[
                ("2.role", s("assistant")),
                ("0.role", s("user")),
                ("0.content", s("hi")),
                ("2.n", AnyValueView::Int(3)),
                ("name", s("not indexed")),
                ("x.role", s("not indexed")),
            ],
        );
        assert_eq!(
            json,
            r#"[{"role":"user","content":"hi"},{"role":"assistant","n":3}]"#
        );
        assert_eq!(used, [0, 1, 2, 3]);
    }

    #[test]
    fn wrappers_after_an_index_are_dropped_and_named_leaves_are_parsed() {
        let (json, _) = run(
            &WRAPPED,
            &[
                ("0.message.role", s("assistant")),
                ("0.message.tool_calls.1.call.name", s("b")),
                ("0.message.tool_calls.0.call.name", s("a")),
                ("0.message.message", s("kept: not after an index")),
                ("1.schema", s(r#"{"type":"object"}"#)),
                ("2.schema", s("not json")),
            ],
        );
        assert_eq!(
            json,
            r#"[{"role":"assistant","tool_calls":[{"name":"a"},{"name":"b"}],"message":"kept: not after an index"},{"schema":{"type":"object"}},{"schema":"not json"}]"#
        );
    }

    #[test]
    fn a_path_conflict_keeps_the_first_seen() {
        let (json, used) = run(
            &PLAIN,
            &[
                ("0.a", s("leaf")),
                ("0.a.b", s("under a leaf")),
                ("0.c.d", s("object")),
                ("0.c", s("leaf over an object")),
                ("0.c.0", s("index into an object")),
            ],
        );
        assert_eq!(json, r#"[{"a":"leaf","c":{"d":"object"}}]"#);
        assert_eq!(used, [0, 2]);
    }

    #[test]
    fn deep_keys_and_empty_families_are_left_out() {
        let deep = Box::leak(format!("0.{}", vec!["a"; MAX_DEPTH + 1].join(".")).into_boxed_str());
        assert_eq!(run(&PLAIN, &[(deep, s("x"))]).0, "");
        assert_eq!(run(&PLAIN, &[]).0, "");
        assert_eq!(split_index("0"), None);
        assert_eq!(split_index("00.a"), Some((0, "a")));
        assert_eq!(split_index(".a"), None);
        assert_eq!(split_index("99999999999999999999999.a"), None);
    }
}
