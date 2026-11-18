//! Usage and cost objects (contract 10.5): the members of one object, or the attributes under one prefix, mapped
//! onto usage keys by a table's [`UsageMap`].
//!
//! Steps, in order:
//!
//! 1. Flatten details ([`USAGE_DETAILS`]): a `<base>_details` object becomes one member per entry, named
//!    `input_<entry>` or `output_<entry>` by the side of its base; a listed object is flattened by name.
//! 2. Keep counts: a member whose value is a non-negative integer, or a string of decimal digits. Ignored
//!    members, floats, negatives and other values are dropped.
//! 3. Alias: for each usage key the table names, the first alias in table order that is present supplies the
//!    value. A member without an alias keeps its own name when the table keeps other members.
//! 4. Sums: an absent target key is the saturating sum of its addends that are present; the addends stay.
//! 5. Disjoint buckets, for a table with exclusive bases: unless a details object was present or the member that
//!    supplied `input` is not an exclusive base, `input` becomes `input + cache_read + cache_write +` every other
//!    kept `input_` key that is not an addend of a sum; likewise `output` with `reasoning` and the other
//!    `output_` keys. `total` is never changed.
//!
//! Costs follow steps 2 to 4 with USD amounts in place of counts.

use std::borrow::Cow;

use crucible_compat::{USAGE_DETAILS, UsageMap};
use crucible_records::{NanoUsd, UsageEntry};

use crate::node::Node;

/// One resulting key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Entry<V, O> {
    pub(crate) key: Cow<'static, str>,
    pub(crate) value: V,
    /// What supplied the value; `None` for a sum.
    pub(crate) origin: Option<O>,
    /// The member that supplied the value, for an aliased key.
    member: Option<String>,
    /// Whether the key is a member kept under its own name.
    other: bool,
}

/// Step 1.
fn flatten<O: Copy>(members: Vec<(String, Node, O)>) -> (Vec<(String, Node, O)>, bool) {
    let mut flat = Vec::with_capacity(members.len());
    let mut details = false;
    for (name, value, origin) in members {
        if let Node::Obj(entries) = &value {
            if let Some(base) = name.strip_suffix(USAGE_DETAILS.suffix) {
                let starts = |bases: &[&str]| bases.iter().any(|b| base.starts_with(b));
                let side = if starts(USAGE_DETAILS.input_bases) {
                    Some(UsageEntry::INPUT)
                } else if starts(USAGE_DETAILS.output_bases) {
                    Some(UsageEntry::OUTPUT)
                } else {
                    None
                };
                if let Some(side) = side {
                    details = true;
                    for (entry, v) in entries {
                        flat.push((format!("{side}_{entry}"), v.clone(), origin));
                    }
                    continue;
                }
            }
            if let Some((_, parts)) = USAGE_DETAILS.objects.iter().find(|(n, _)| *n == name) {
                for (from, to) in *parts {
                    if let Some(v) = value.member(from) {
                        flat.push(((*to).to_owned(), v.clone(), origin));
                    }
                }
                continue;
            }
        }
        flat.push((name, value, origin));
    }
    (flat, details)
}

/// Steps 3 and 4 over the kept members.
fn alias_and_sum<V: Copy, O: Copy>(
    map: &UsageMap,
    kept: Vec<(String, V, O)>,
    sum: fn(&mut dyn Iterator<Item = V>) -> V,
) -> Vec<Entry<V, O>> {
    let mut out: Vec<Entry<V, O>> = Vec::new();
    for (member, key) in map.aliases {
        if out.iter().any(|e| e.key == *key) {
            continue;
        }
        if let Some((name, value, origin)) = kept.iter().find(|(name, ..)| name == member) {
            out.push(Entry {
                key: Cow::Borrowed(key),
                value: *value,
                origin: Some(*origin),
                member: Some(name.clone()),
                other: false,
            });
        }
    }
    if map.keep_other {
        for (name, value, origin) in kept {
            let aliased = map.aliases.iter().any(|(member, _)| *member == name);
            if aliased || out.iter().any(|e| e.key == name) {
                continue;
            }
            out.push(Entry {
                key: Cow::Owned(name),
                value,
                origin: Some(origin),
                member: None,
                other: true,
            });
        }
    }
    for (target, addends) in map.sums {
        if out.iter().any(|e| e.key == *target) {
            continue;
        }
        let mut present = out
            .iter()
            .filter(|e| addends.contains(&&*e.key))
            .map(|e| e.value)
            .peekable();
        if present.peek().is_none() {
            continue;
        }
        let value = sum(&mut present);
        out.push(Entry {
            key: Cow::Borrowed(target),
            value,
            origin: None,
            member: None,
            other: false,
        });
    }
    out
}

/// A count: a non-negative integer, or a string of decimal digits.
fn count(value: &Node) -> Option<u64> {
    match value {
        Node::Int(i) => u64::try_from(*i).ok(),
        Node::UInt(u) => Some(*u),
        Node::Str(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok(),
        _ => None,
    }
}

/// A USD amount: a finite number, or a string holding a decimal number.
fn usd(value: &Node) -> Option<NanoUsd> {
    match value {
        Node::Int(i) => Some(NanoUsd::from_nanos(
            (*i).max(0).saturating_mul(1_000_000_000),
        )),
        Node::UInt(_) => Some(NanoUsd::from_nanos(i64::MAX)),
        Node::Float(d) if d.is_finite() => Some(NanoUsd::from_usd_f64(*d)),
        Node::Str(s) => NanoUsd::from_usd_decimal(s),
        _ => None,
    }
}

/// Step 5 for one side.
fn widen<O>(map: &UsageMap, out: &mut [Entry<u64, O>], side: &str, parts: &[&str]) {
    let exclusive = out.iter().any(|e| {
        e.key == side
            && e.member
                .as_deref()
                .is_some_and(|m| map.exclusive_bases.contains(&m))
    });
    if !exclusive {
        return;
    }
    let addend = |key: &str| map.sums.iter().any(|(_, addends)| addends.contains(&key));
    let extra = out
        .iter()
        .filter(|e| {
            parts.contains(&&*e.key)
                || (e.other
                    && e.key.len() > side.len()
                    && e.key.starts_with(side)
                    && e.key.as_bytes().get(side.len()) == Some(&b'_')
                    && !addend(&e.key))
        })
        .fold(0u64, |sum, e| sum.saturating_add(e.value));
    for entry in out.iter_mut().filter(|e| e.key == side) {
        entry.value = entry.value.saturating_add(extra);
    }
}

/// The usage keys of a usage object. `members` are its members in wire order, each with what supplied it.
pub(crate) fn usage<O: Copy>(
    map: &UsageMap,
    members: Vec<(String, Node, O)>,
) -> Vec<Entry<u64, O>> {
    let (flat, details) = flatten(members);
    let kept = flat
        .into_iter()
        .filter(|(name, ..)| !map.ignore.contains(&name.as_str()))
        .filter_map(|(name, value, origin)| Some((name, count(&value)?, origin)))
        .collect();
    let mut out = alias_and_sum(map, kept, |values| values.fold(0u64, u64::saturating_add));
    if !map.exclusive_bases.is_empty() && !details {
        widen(
            map,
            &mut out,
            UsageEntry::INPUT,
            &[UsageEntry::CACHE_READ, UsageEntry::CACHE_WRITE],
        );
        widen(map, &mut out, UsageEntry::OUTPUT, &[UsageEntry::REASONING]);
    }
    out
}

/// The cost keys of a cost object.
pub(crate) fn cost<O: Copy>(
    map: &UsageMap,
    members: Vec<(String, Node, O)>,
) -> Vec<Entry<NanoUsd, O>> {
    let kept = members
        .into_iter()
        .filter(|(name, ..)| !map.ignore.contains(&name.as_str()))
        .filter_map(|(name, value, origin)| Some((name, usd(&value)?, origin)))
        .collect();
    alias_and_sum(map, kept, |values| NanoUsd::saturating_sum(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map with the features of the tables: aliases, a sum, exclusive bases and an ignored member.
    static MAP: UsageMap = UsageMap {
        aliases: &[
            ("input", "input"),
            ("prompt_tokens", "input"),
            ("output", "output"),
            ("completion_tokens", "output"),
            ("total", "total"),
            ("total_tokens", "total"),
            ("input_cached_tokens", "cache_read"),
            ("output_reasoning_tokens", "reasoning"),
        ],
        ignore: &["duration"],
        keep_other: true,
        sums: &[(
            "cache_write",
            &["input_cache_creation_5m", "input_cache_creation_1h"],
        )],
        exclusive_bases: &["input", "output"],
    };

    fn run(map: &UsageMap, text: &str) -> Vec<(String, u64)> {
        let Some(Node::Obj(members)) = Node::parse(text) else {
            panic!("not an object")
        };
        let members = members.into_iter().map(|(k, v)| (k, v, ())).collect();
        usage(map, members)
            .into_iter()
            .map(|e| (e.key.into_owned(), e.value))
            .collect()
    }

    fn pairs(list: &[(&str, u64)]) -> Vec<(String, u64)> {
        list.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    #[test]
    fn a_details_object_marks_the_map_inclusive() {
        let got = run(
            &MAP,
            r#"{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11,
                "prompt_tokens_details":{"cached_tokens":4,"audio_tokens":0}}"#,
        );
        assert_eq!(
            got,
            pairs(&[
                ("input", 10),
                ("output", 1),
                ("total", 11),
                ("cache_read", 4),
                ("input_audio_tokens", 0)
            ])
        );
    }

    #[test]
    fn exclusive_bases_are_widened_by_their_parts() {
        assert_eq!(
            run(
                &MAP,
                r#"{"input":6,"output":1,"total":11,"input_cached_tokens":4}"#
            ),
            pairs(&[
                ("input", 10),
                ("output", 1),
                ("total", 11),
                ("cache_read", 4)
            ])
        );
        // The addends of a sum are inside `cache_write` already and are not added twice.
        assert_eq!(
            run(
                &MAP,
                r#"{"input":10,"output":5,"input_cached_tokens":25,
                    "input_cache_creation_5m":40,"input_cache_creation_1h":60}"#
            ),
            pairs(&[
                ("input", 135),
                ("output", 5),
                ("cache_read", 25),
                ("input_cache_creation_5m", 40),
                ("input_cache_creation_1h", 60),
                ("cache_write", 100)
            ])
        );
        // An inclusive base (not listed as exclusive) is left alone.
        assert_eq!(
            run(
                &MAP,
                r#"{"prompt_tokens":10,"input_cached_tokens":4,"output":3,"output_reasoning_tokens":2}"#
            ),
            pairs(&[
                ("input", 10),
                ("output", 5),
                ("cache_read", 4),
                ("reasoning", 2)
            ])
        );
    }

    #[test]
    fn the_first_listed_alias_wins_and_counts_are_strict() {
        let got = run(
            &MAP,
            r#"{"prompt_tokens":7,"input":3,"duration":9,"ratio":0.5,"negative":-1,
                "digits":"12","text":"x","nested":{"a":1},"extra":2}"#,
        );
        assert_eq!(got, pairs(&[("input", 3), ("digits", 12), ("extra", 2)]));
    }

    #[test]
    fn the_cache_creation_object_is_flattened_and_summed() {
        let got = run(
            &MAP,
            r#"{"prompt_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":3}}"#,
        );
        assert_eq!(
            got,
            pairs(&[
                ("input", 1),
                ("input_cache_creation_5m", 2),
                ("input_cache_creation_1h", 3),
                ("cache_write", 5)
            ])
        );
    }

    #[test]
    fn costs_take_numbers_and_numeric_strings() {
        static COSTS: UsageMap = UsageMap {
            aliases: &[("input_cost", "input"), ("total_cost", "total")],
            ignore: &[],
            keep_other: false,
            sums: &[],
            exclusive_bases: &[],
        };
        let Some(Node::Obj(members)) =
            Node::parse(r#"{"input_cost":0.0001,"total_cost":"0.5","currency":"USD","other":1}"#)
        else {
            panic!("not an object")
        };
        let got: Vec<(String, i64)> = cost(
            &COSTS,
            members.into_iter().map(|(k, v)| (k, v, ())).collect(),
        )
        .into_iter()
        .map(|e| (e.key.into_owned(), e.value.nanos()))
        .collect();
        assert_eq!(
            got,
            [
                ("input".to_owned(), 100_000),
                ("total".to_owned(), 500_000_000)
            ]
        );
    }
}
