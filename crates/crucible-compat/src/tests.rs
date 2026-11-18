use crucible_records::UsageEntry;

use super::*;

#[test]
fn registry_ids_are_dense_and_described() {
    for (i, d) in DIALECTS.iter().enumerate() {
        assert_eq!(usize::from(d.id), i);
        assert!(!d.description.is_empty() && !d.detected_by.is_empty());
        assert_eq!(dialect(d.id), Some(d));
    }
    assert_eq!(dialect(8), None);
    assert_eq!(dialect(u8::MAX), None);
}

#[test]
fn key_matching_is_exact_or_prefix() {
    assert!(KeyMatch::Exact("a.b").matches("a.b"));
    assert!(!KeyMatch::Exact("a.b").matches("a.bc"));
    assert!(!KeyMatch::Exact("a.b").matches("A.B"));
    assert!(KeyMatch::Prefix("a.").matches("a.b"));
    assert!(!KeyMatch::Prefix("a.").matches("b.a"));
    assert_eq!(KeyMatch::Prefix("p").text(), "p");
}

#[test]
fn layers_are_the_tables_in_contract_order() {
    for (i, table) in TABLES.iter().enumerate() {
        assert_eq!(usize::from(table.dialect), i);
        assert_eq!(
            LAYERS.iter().filter(|l| core::ptr::eq(**l, *table)).count(),
            1
        );
    }
    let order: Vec<u8> = LAYERS.iter().map(|l| l.dialect).collect();
    assert_eq!(order, [0, 1, 7, 0, 2, 4, 3, 5, 6]);
    assert!(core::ptr::eq(LAYERS[0], &CRUCIBLE_KEYS));
    assert!(CRUCIBLE_KEYS.detect.is_empty());
    assert!(DIALECT_0.detect.is_empty());
    for rule in CRUCIBLE_KEYS.fields {
        assert!(rule.sources.iter().all(|s| s.scope == Scope::Any));
    }
}

fn usage_map(decoder: Decoder) -> Option<&'static UsageMap> {
    match decoder {
        Decoder::UsageObject(map) | Decoder::PrefixUsage(map) | Decoder::CostObject(map) => {
            Some(map)
        }
        _ => None,
    }
}

fn snake_case(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The identity of a source: where it reads, the key text, whether it is a prefix, and the member path.
type At = (Location, &'static str, bool, &'static [&'static str]);

fn at(source: &Source) -> At {
    (
        source.location,
        source.key.text(),
        matches!(source.key, KeyMatch::Prefix(_)),
        source.path,
    )
}

/// Every table is well formed:
///
/// - within one table, each target has one rule, every source fits its target, and each (location, key, path)
///   feeds one target, [`SHARED_KEYS`] excepted (an exact key may also lie under a prefix source: fan-out);
/// - across tables, one (location, key, path) may appear in several layers only for the same target;
/// - an `Own` source is a bare span key outside the namespaces its table detects by;
/// - a prefix source uses a family decoder unless its target takes one value (first match wins), and an exact
///   source never does; a member path needs an exact span key;
/// - type rules are unique per (key, path) within a table, and across tables the value lists of one key are
///   disjoint;
/// - every usage alias names a normalized key or a snake_case key, and level names are in lower case.
#[test]
fn tables_are_well_formed() {
    let mut seen: Vec<(At, Target)> = Vec::new();
    let mut types: Vec<(&str, &[&str], &str)> = Vec::new();
    for (i, table) in LAYERS.iter().enumerate() {
        assert!(dialect(table.dialect).is_some());
        let namespaces: Vec<&str> = table
            .detect
            .iter()
            .filter_map(|d| match d.on {
                DetectOn::SpanKey(KeyMatch::Prefix(p)) => Some(p),
                _ => None,
            })
            .collect();
        let mut keys: Vec<At> = Vec::new();
        for (j, rule) in table.fields.iter().enumerate() {
            assert!(
                table.fields[..j].iter().all(|r| r.target != rule.target),
                "{:?} has two rules in layer {i}",
                rule.target
            );
            assert!(!rule.sources.is_empty());
            for source in rule.sources {
                assert!(rule.target.accepts(source.decoder), "{source:?}");
                assert!(!source.key.text().is_empty());
                let here = at(source);
                let shared = SHARED_KEYS.contains(&(source.location, source.key.text()));
                assert!(!keys.contains(&here) || shared, "{here:?} feeds two fields");
                keys.push(here);
                for (theirs, target) in &seen {
                    assert!(
                        *theirs != here || *target == rule.target || shared,
                        "{here:?} feeds {target:?} and {:?}",
                        rule.target
                    );
                }
                seen.push((here, rule.target));
                if source.scope == Scope::Own {
                    assert_eq!(source.location, Location::Span, "{source:?}");
                    assert!(matches!(source.key, KeyMatch::Exact(_)), "{source:?}");
                    assert!(
                        namespaces.iter().all(|p| !source.key.text().starts_with(p)),
                        "{source:?}"
                    );
                }
                let scalar = !rule.target.is_map()
                    && !matches!(
                        rule.target,
                        Target::UsageAll
                            | Target::CostAll
                            | Target::Input
                            | Target::Output
                            | Target::ToolDefinitions
                            | Target::ToolCalls
                            | Target::SystemInstructions
                    );
                match source.key {
                    KeyMatch::Prefix(_) => assert!(
                        source.decoder.is_family()
                            || scalar
                            || (rule.target == Target::CostAll && source.decoder == Decoder::Usd),
                        "{source:?}"
                    ),
                    KeyMatch::Exact(_) => assert!(!source.decoder.is_family(), "{source:?}"),
                }
                if !source.path.is_empty() {
                    assert_eq!(source.location, Location::Span, "{source:?}");
                    assert!(matches!(source.key, KeyMatch::Exact(_)), "{source:?}");
                }
                if let Location::EventTime(name) = source.location {
                    assert!(!name.is_empty());
                    assert_eq!(source.key, KeyMatch::Exact(EVENT_TIME_KEY));
                    assert_eq!(source.decoder, Decoder::Timestamp);
                }
                if let Some(map) = usage_map(source.decoder) {
                    for (member, key) in map.aliases {
                        assert!(!member.is_empty() && snake_case(key), "{member} -> {key}");
                    }
                    for (key, addends) in map.sums {
                        assert!(snake_case(key) && !addends.is_empty());
                    }
                }
                if let Decoder::Level(map) = source.decoder {
                    assert!(!map.names.is_empty() || !map.numbers.is_empty());
                    for (name, _) in map.names {
                        assert_eq!(*name, name.trim().to_ascii_lowercase());
                    }
                }
                if let Target::Usage(key) | Target::Cost(key) = rule.target {
                    assert!(snake_case(key), "{key}");
                }
            }
        }
        for (j, rule) in table.observation_types.iter().enumerate() {
            assert!(!rule.key.is_empty() && !rule.values.is_empty());
            assert!((1..=10).contains(&rule.rank));
            assert!(rule.path.is_empty() || !rule.consume);
            assert!(
                table.observation_types[..j]
                    .iter()
                    .all(|r| (r.key, r.path) != (rule.key, rule.path))
            );
            for (k, (value, _)) in rule.values.iter().enumerate() {
                assert!(rule.values[..k].iter().all(|(v, _)| v != value));
                assert!(
                    !types.contains(&(rule.key, rule.path, value)),
                    "{} = {value} has two rules",
                    rule.key
                );
                types.push((rule.key, rule.path, value));
            }
        }
        for token in table.media {
            assert!(!token.prefix.is_empty());
        }
    }
}

/// Type rules rank in the order of contract 10.4, which is the layer order except that the span kind of
/// dialect 2 ranks before the semantic-convention operation name.
#[test]
fn type_rules_rank_in_contract_order() {
    let ranks = |table: &DialectTable| -> Vec<u8> {
        table.observation_types.iter().map(|r| r.rank).collect()
    };
    assert_eq!(ranks(&CRUCIBLE_KEYS), [1]);
    assert_eq!(ranks(&DIALECT_1), [2]);
    assert_eq!(ranks(&DIALECT_7), [3, 3, 3]);
    assert_eq!(ranks(&DIALECT_0), [4, 6]);
    assert_eq!(ranks(&DIALECT_2), [5]);
    assert_eq!(ranks(&DIALECT_4), [7]);
    assert_eq!(ranks(&DIALECT_3), [8, 8, 8]);
    assert_eq!(ranks(&DIALECT_5), [9, 9]);
    assert_eq!(ranks(&DIALECT_6), [10]);
}

/// The size of each table: detection rules, type values (a member rule counts once) and sources.
#[test]
fn tables_have_the_planned_rows() {
    let rows = |table: &DialectTable| {
        let types: usize = table
            .observation_types
            .iter()
            .map(|r| if r.path.is_empty() { r.values.len() } else { 1 })
            .sum();
        let sources: usize = table.fields.iter().map(|r| r.sources.len()).sum();
        (table.detect.len(), types, sources)
    };
    assert_eq!(rows(&DIALECT_1), (3, 10, 44));
    assert_eq!(rows(&DIALECT_2), (8, 12, 42));
    assert_eq!(rows(&DIALECT_3), (14, 16, 32));
    assert_eq!(rows(&DIALECT_4), (2, 21, 55));
    assert_eq!(rows(&DIALECT_5), (2, 21, 12));
    assert_eq!(rows(&DIALECT_6), (3, 2, 11));
    assert_eq!(rows(&DIALECT_7), (22, 12, 27));
}

#[test]
fn the_explicit_type_key_names_every_type_by_its_wire_name() {
    let explicit = CRUCIBLE_KEYS.observation_types[0];
    assert_eq!(explicit.key, "crucible.observation.type");
    assert_eq!(explicit.values.len(), ObservationType::ALL.len());
    for t in ObservationType::ALL {
        assert!(explicit.values.contains(&(t.as_str(), *t)), "{t}");
    }
}

#[test]
fn native_usage_targets_use_normalized_keys() {
    let normalized = [
        UsageEntry::INPUT,
        UsageEntry::OUTPUT,
        UsageEntry::TOTAL,
        UsageEntry::CACHE_READ,
        UsageEntry::CACHE_WRITE,
        UsageEntry::REASONING,
    ];
    for rule in DIALECT_0.fields {
        if let Target::Usage(key) = rule.target {
            assert!(normalized.contains(&key), "{key}");
        }
    }
}

#[test]
fn detection_confidences_are_on_the_one_scale() {
    assert_eq!(Confidence::HINT, Confidence(60));
    for table in TABLES {
        for rule in table.detect {
            assert!([100, 90, 70, 40].contains(&rule.confidence.0), "{rule:?}");
        }
    }
}

#[test]
fn routes_name_registered_dialects_and_distinct_paths() {
    for (i, route) in ROUTES.iter().enumerate() {
        assert!(dialect(route.dialect).is_some() && route.dialect != 0);
        assert!(route.path.starts_with('/') && route.path.ends_with("/v1/traces"));
        assert!(route.basic || route.bearer);
        assert!(ROUTES[..i].iter().all(|r| r.path != route.path));
    }
}
