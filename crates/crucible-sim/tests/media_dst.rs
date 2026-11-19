//! The media simulation (`crucible_sim::media_sim`): seeds with faults and crashes, with a lax store, and each
//! seeded defect of the collector, which the oracle must catch.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use crucible_media::gc::Mutant;
use crucible_sim::media_sim::{Options, run_media_sim};

fn check(seed: u64, options: Options) -> crucible_sim::media_sim::Stats {
    let outcome = run_media_sim(seed, options);
    assert!(
        outcome.violations.is_empty(),
        "seed {seed} {options:?}: {:#?}\n{:?}",
        outcome.violations,
        outcome.stats
    );
    outcome.stats
}

#[test]
fn media_survives_faults_crashes_and_clients_that_never_finish() {
    let mut totals = crucible_sim::media_sim::Stats::default();
    for seed in 1..=6 {
        let stats = check(seed, Options::realistic());
        totals.creates += stats.creates;
        totals.deduplicated += stats.deduplicated;
        totals.never_uploaded += stats.never_uploaded;
        totals.confirmed += stats.confirmed;
        totals.deletions += stats.deletions;
        totals.downloads += stats.downloads;
        totals.crashes += stats.crashes;
    }
    // The workload reached every path the invariants are about.
    assert!(totals.creates > 100, "{totals:?}");
    assert!(
        totals.deduplicated > 0 && totals.never_uploaded > 0,
        "{totals:?}"
    );
    assert!(totals.confirmed > 0 && totals.deletions > 0, "{totals:?}");
    assert!(totals.downloads > 0 && totals.crashes > 0, "{totals:?}");
}

#[test]
fn a_lax_store_is_caught_by_the_length_check_of_the_confirm() {
    let mut mismatches = 0;
    for seed in 11..=14 {
        let options = Options {
            lax: true,
            ..Options::realistic()
        };
        mismatches += check(seed, options).mismatches;
    }
    assert!(mismatches > 0);
}

#[test]
fn every_seeded_defect_is_caught() {
    for mutant in [
        Mutant::ObjectBeforeRow,
        Mutant::KeepUnused,
        Mutant::SkipRetention,
    ] {
        let caught = (21..=26).any(|seed| {
            let options = Options {
                mutant: Some(mutant),
                faulty_for: Duration::from_secs(1_800),
                ..Options::realistic()
            };
            !run_media_sim(seed, options).violations.is_empty()
        });
        assert!(caught, "{mutant:?} was not caught");
    }
}
