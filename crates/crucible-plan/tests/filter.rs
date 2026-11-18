//! The list filter AST (contracts 8.8 and 12.4): the canonical form and the hash that binds a cursor to its
//! request's filters, and the compilation of a filter for each table it can be evaluated on.

// Test code throughout: lets the helpers below use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

use crucible_plan::{Filter, FilterField, FilterTarget, PlanError, Pred, TextField};
use crucible_records::{Level, ObservationType};
use proptest::prelude::*;
use sha2::{Digest, Sha256};

const TEXT_FIELDS: [TextField; 5] = [
    TextField::Environment,
    TextField::Release,
    TextField::Version,
    TextField::UserId,
    TextField::SessionId,
];
const TARGETS: [FilterTarget; 4] = [
    FilterTarget::TraceHeads,
    FilterTarget::SpanCore,
    FilterTarget::SpanPayload,
    FilterTarget::SpanMetrics,
];

fn text() -> impl Strategy<Value = String> {
    prop_oneof![Just(String::new()), "[ab]{1,3}", ".{0,6}"]
}

fn texts() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(text(), 0..4)
}

fn condition() -> impl Strategy<Value = Filter> {
    prop_oneof![
        (0..TEXT_FIELDS.len(), texts()).prop_map(|(f, v)| Filter::Text(TEXT_FIELDS[f], v)),
        prop::collection::vec(0..Level::ALL.len(), 0..4)
            .prop_map(|v| Filter::Level(v.into_iter().map(|i| Level::ALL[i]).collect())),
        prop::collection::vec(0..ObservationType::ALL.len(), 0..4).prop_map(|v| {
            Filter::ObservationType(v.into_iter().map(|i| ObservationType::ALL[i]).collect())
        }),
        texts().prop_map(Filter::Tags),
        (text(), text()).prop_map(|(key, value)| Filter::Metadata { key, value }),
        text().prop_map(Filter::Name),
    ]
}

fn filter() -> impl Strategy<Value = Filter> {
    condition().prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Filter::And),
            prop::collection::vec(inner.clone(), 0..4).prop_map(Filter::Or),
            inner.prop_map(|f| Filter::Not(Box::new(f))),
        ]
    })
}

fn draw(seed: &mut u64, n: u64) -> u64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (*seed >> 33) % n
}

/// The items in another order, sometimes with one repeated.
fn shuffled<T: Clone>(items: &[T], seed: &mut u64) -> Vec<T> {
    let mut out = items.to_vec();
    for i in (1..out.len()).rev() {
        out.swap(i, draw(seed, i as u64 + 1) as usize);
    }
    if let Some(first) = out.first().cloned()
        && draw(seed, 2) == 0
    {
        out.push(first);
    }
    out
}

/// A filter that the canonical form must not tell from `filter`: operands and values reordered and repeated,
/// conjunctions and disjunctions nested and wrapped, negations doubled. `seed` picks which.
fn disguised(filter: &Filter, seed: &mut u64) -> Filter {
    let rewritten = match filter {
        Filter::And(operands) | Filter::Or(operands) => {
            let conjunction = matches!(filter, Filter::And(_));
            let wrap = |operands: Vec<Filter>| {
                if conjunction {
                    Filter::And(operands)
                } else {
                    Filter::Or(operands)
                }
            };
            let operands: Vec<Filter> = operands
                .iter()
                .map(|operand| disguised(operand, seed))
                .collect();
            let mut operands = shuffled(&operands, seed);
            // Nest a tail of the operands in an operator of the same kind.
            if operands.len() > 1 && draw(seed, 2) == 0 {
                let tail = operands.split_off(operands.len() / 2);
                operands.push(wrap(tail));
            }
            // Wrapping this in a single-operand operator of the other kind is covered below.
            wrap(operands)
        }
        Filter::Not(operand) => Filter::Not(Box::new(disguised(operand, seed))),
        Filter::Text(field, values) => Filter::Text(*field, shuffled(values, seed)),
        Filter::Level(values) => Filter::Level(shuffled(values, seed)),
        Filter::ObservationType(values) => Filter::ObservationType(shuffled(values, seed)),
        Filter::Tags(values) => Filter::Tags(shuffled(values, seed)),
        other => other.clone(),
    };
    match draw(seed, 4) {
        0 => Filter::Not(Box::new(Filter::Not(Box::new(rewritten)))),
        1 => Filter::And(vec![rewritten]),
        2 => Filter::Or(vec![rewritten]),
        _ => rewritten,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Equivalent filters (as the module documentation of `filter` states equivalence) have one canonical
    /// form, one byte string and one hash, and compile to one predicate.
    #[test]
    fn equivalent_filters_have_one_hash(filter in filter(), seed in any::<u64>()) {
        let mut seed = seed;
        let other = disguised(&filter, &mut seed);
        prop_assert_eq!(filter.canonical(), other.canonical());
        prop_assert_eq!(filter.canonical_bytes(), other.canonical_bytes());
        prop_assert_eq!(filter.hash(), other.hash());
        // The canonical form is a fixed point.
        prop_assert_eq!(filter.canonical().canonical(), filter.canonical());
        for target in TARGETS {
            prop_assert_eq!(filter.pred(target), other.pred(target));
        }
    }

    /// Filters with different canonical forms have different bytes and different hashes: the encoding tags
    /// and length-prefixes everything, so no two forms share their bytes.
    #[test]
    fn different_filters_have_different_hashes(a in filter(), b in filter()) {
        let same = a.canonical() == b.canonical();
        prop_assert_eq!(a.canonical_bytes() == b.canonical_bytes(), same);
        prop_assert_eq!(a.hash() == b.hash(), same);
    }

    /// The hash is the first 16 bytes of SHA-256 over the canonical bytes: what `FilterHash::of` computes.
    #[test]
    fn the_hash_is_sha256_of_the_canonical_bytes(filter in filter()) {
        let digest = Sha256::digest(filter.canonical_bytes());
        prop_assert_eq!(&filter.hash()[..], &digest[..16]);
    }

    /// A filter compiles for a target exactly when the target evaluates every field it names; the refusal
    /// names the first field it does not. No condition is dropped: the predicate holds one leaf per condition
    /// of the canonical form.
    #[test]
    fn a_filter_compiles_only_where_every_field_is_evaluated(filter in filter()) {
        fn conditions(filter: &Filter) -> usize {
            match filter {
                Filter::And(operands) | Filter::Or(operands) => operands.iter().map(conditions).sum(),
                Filter::Not(operand) => conditions(operand),
                _ => 1,
            }
        }
        fn leaves(pred: &Pred) -> usize {
            match pred {
                Pred::And(operands) | Pred::Or(operands) => operands.iter().map(leaves).sum(),
                Pred::Not(operand) => leaves(operand),
                _ => 1,
            }
        }
        for target in TARGETS {
            match (filter.pred(target), filter.canonical().unsupported(target)) {
                (Ok(pred), None) => prop_assert_eq!(leaves(&pred), conditions(&filter.canonical())),
                (Err(PlanError::UnsupportedFilter { field, target: named }), Some(unsupported)) => {
                    prop_assert_eq!(field, unsupported.as_str());
                    prop_assert_eq!(named, target.as_str());
                }
                (result, unsupported) => prop_assert!(false, "{result:?} with {unsupported:?}"),
            }
            // The two halves of a split are the filter: the first compiles, and together they keep every
            // condition.
            let (here, rest) = filter.split(target);
            prop_assert!(here.pred(target).is_ok());
            prop_assert_eq!(
                Filter::And(vec![here, rest]).canonical(),
                filter.canonical()
            );
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_filter_that_keeps_every_row_has_the_empty_form() {
    for all in [
        Filter::all(),
        Filter::And(vec![Filter::And(Vec::new())]),
        Filter::Not(Box::new(Filter::Not(Box::new(Filter::all())))),
    ] {
        assert!(all.is_all());
        assert_eq!(all.canonical_bytes(), Vec::<u8>::new());
        // SHA-256 of the empty string: the hash of the trace list's cursors before it took filters.
        assert_eq!(hex(&all.hash()), "e3b0c44298fc1c149afbf4c8996fb924");
    }
    // The empty disjunction keeps no row and is another filter.
    assert!(!Filter::Or(Vec::new()).is_all());
    assert_ne!(Filter::Or(Vec::new()).hash(), Filter::all().hash());
}

#[test]
fn the_canonical_bytes_are_pinned() {
    let filter = Filter::And(vec![
        Filter::Tags(vec!["b".to_owned(), "a".to_owned(), "b".to_owned()]),
        Filter::Not(Box::new(Filter::Level(vec![Level::Error, Level::Warning]))),
        Filter::Text(TextField::Environment, vec!["prod".to_owned()]),
    ]);
    // And, three operands; then, sorted by their bytes: Not(Level [2, 3]), environment in ["prod"],
    // tags any of ["a", "b"].
    let expected = concat!(
        "01",
        "0000000000000003",
        "03",
        "14",
        "0000000000000002",
        "0203",
        "0a",
        "0000000000000001",
        "0000000000000004",
        "70726f64",
        "16",
        "0000000000000002",
        "0000000000000001",
        "61",
        "0000000000000001",
        "62",
    );
    assert_eq!(hex(&filter.canonical_bytes()), expected);
    let digest = Sha256::digest(filter.canonical_bytes());
    assert_eq!(filter.hash()[..], digest[..16]);
    // Lists and strings carry their lengths: a list of two values is not one value of their concatenation.
    let two = Filter::Tags(vec!["a".to_owned(), "b".to_owned()]);
    let one = Filter::Tags(vec!["ab".to_owned()]);
    assert_ne!(two.hash(), one.hash());
    let split = |key: &str, value: &str| Filter::Metadata {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    assert_ne!(split("a", "bc").hash(), split("ab", "c").hash());
    // What is not stated as equivalent is not: De Morgan's forms are two filters.
    let (a, b) = (Filter::Name("a".to_owned()), Filter::Name("b".to_owned()));
    let not = |f: Filter| Filter::Not(Box::new(f));
    assert_ne!(
        not(Filter::Or(vec![a.clone(), b.clone()])).hash(),
        Filter::And(vec![not(a), not(b)]).hash()
    );
}

#[test]
fn each_target_evaluates_its_own_fields() {
    let names = |target: FilterTarget| -> Vec<&str> {
        target
            .fields()
            .into_iter()
            .map(FilterField::as_str)
            .collect()
    };
    assert_eq!(
        names(FilterTarget::TraceHeads),
        [
            "environment",
            "release",
            "version",
            "level",
            "user_id",
            "session_id",
            "tags",
            "name"
        ]
    );
    assert_eq!(
        names(FilterTarget::SpanCore),
        [
            "environment",
            "release",
            "version",
            "level",
            "observation_type",
            "user_id",
            "session_id",
            "tags",
            "name"
        ]
    );
    assert_eq!(
        names(FilterTarget::SpanPayload),
        ["user_id", "session_id", "metadata"]
    );
    assert_eq!(
        names(FilterTarget::SpanMetrics),
        ["environment", "release", "level", "observation_type"]
    );
    // Every field compiles on every target that lists it, and is refused by name on the others.
    let condition = |field: FilterField| match field {
        FilterField::Text(text) => Filter::Text(text, vec!["x".to_owned()]),
        FilterField::Level => Filter::Level(vec![Level::Error]),
        FilterField::ObservationType => Filter::ObservationType(vec![ObservationType::Tool]),
        FilterField::Tags => Filter::Tags(vec!["x".to_owned()]),
        FilterField::Metadata => Filter::Metadata {
            key: "k".to_owned(),
            value: "v".to_owned(),
        },
        FilterField::Name => Filter::Name("x".to_owned()),
    };
    for target in TARGETS {
        for field in FilterField::ALL {
            // Also under a negation inside a disjunction: nothing is dropped at any depth.
            let nested = Filter::Or(vec![
                Filter::Not(Box::new(condition(field))),
                Filter::Text(TextField::UserId, vec!["u".to_owned()]),
            ]);
            for filter in [condition(field), nested] {
                let supported = target.supports(field)
                    && (filter == condition(field)
                        || target.supports(FilterField::Text(TextField::UserId)));
                match filter.pred(target) {
                    Ok(_) => assert!(supported, "{field:?} compiled for {target:?}"),
                    Err(PlanError::UnsupportedFilter { field: named, .. }) => {
                        assert!(!supported, "{field:?} refused for {target:?}");
                        assert!(
                            named == field.as_str() || named == "user_id",
                            "{named} for {field:?}"
                        );
                    }
                    Err(other) => panic!("{other:?}"),
                }
            }
        }
    }
}

#[test]
fn a_split_keeps_whole_conjuncts_together() {
    let metadata = Filter::Metadata {
        key: "k".to_owned(),
        value: "v".to_owned(),
    };
    let environment = Filter::Text(TextField::Environment, vec!["prod".to_owned()]);
    let user = Filter::Text(TextField::UserId, vec!["u".to_owned()]);
    let filter = Filter::And(vec![metadata.clone(), environment.clone(), user.clone()]);
    let (core, rest) = filter.split(FilterTarget::SpanCore);
    assert_eq!(
        core.canonical(),
        Filter::And(vec![environment.clone(), user.clone()]).canonical()
    );
    assert_eq!(rest.canonical(), metadata);
    let (payload, rest) = filter.split(FilterTarget::SpanPayload);
    assert_eq!(
        payload.canonical(),
        Filter::And(vec![metadata.clone(), user]).canonical()
    );
    assert_eq!(rest.canonical(), environment);
    // A disjunction over two tables cannot be split: it stays whole, in the rest.
    let mixed = Filter::Or(vec![metadata, environment]);
    let (core, rest) = mixed.split(FilterTarget::SpanCore);
    assert!(core.is_all());
    assert_eq!(rest.canonical(), mixed.canonical());
}
