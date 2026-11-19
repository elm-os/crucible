//! The alerts simulation suite (contracts 11.7 and 12.6): the real scheduler, evaluations consumer and outbox
//! consumer over `PgModel` and over rollup rows in `ChModel`, under crashes, downtime, clock skips, store
//! faults and receivers that are down, with the oracle for A1 to A3 and the three seeded defects it must
//! catch (docs/testing/alerts-simulation.md).
//!
//! Replay one seed:
//!
//! ```sh
//! CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test alerts_dst replay_seed --run-ignored all --no-capture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
#![allow(clippy::disallowed_methods)]

use std::ops::Range;

use crucible_sim::alerts_sim::{Mutant, Options, Outcome, Stats, run_alerts_sim};

const REPLAY: &str = "cargo nextest run -p crucible-sim --test alerts_dst replay_seed --run-ignored all --no-capture";

fn assert_holds(outcome: &Outcome) {
    assert!(
        outcome.violations.is_empty(),
        "seed {seed} violated: {violations:#?}\n{stats:?}\nreplay: CRUCIBLE_SIM_SEED={seed} {REPLAY}",
        seed = outcome.seed,
        violations = outcome.violations,
        stats = outcome.stats,
    );
}

fn sweep(seeds: Range<u64>, options: Options) -> Stats {
    let mut total = Stats::default();
    for seed in seeds.clone() {
        let outcome = run_alerts_sim(seed, options);
        assert_holds(&outcome);
        total.add(&outcome.stats);
    }
    println!("seeds {seeds:?} {options:?}: {total:?}");
    total
}

/// A sweep in which no node crashed, no tick was skipped, no read or delivery failed, no channel was turned
/// off and nothing fired would prove nothing.
fn assert_exercised(total: &Stats) {
    assert!(
        total.crashes > 0 && total.long_downtimes > 0 && total.time_skips > 0,
        "{total:?}"
    );
    assert!(
        total.failovers > 0 && total.store_outages > 0 && total.read_failures > 0,
        "{total:?}"
    );
    assert!(
        total.reads > 0 && total.events > 0 && total.renotifies > 0 && total.skipped_ticks > 0,
        "{total:?}"
    );
    assert!(
        total.outbox_rows > 0 && total.deliveries > 0 && total.delivery_failures > 0,
        "{total:?}"
    );
    assert!(
        total.mails > 0 && total.channels_disabled > 0 && total.dropped > 0,
        "{total:?}"
    );
    assert!(
        total.firing_at_end > 0 && total.no_data_at_end > 0 && total.redeliveries > 0,
        "{total:?}"
    );
    assert!(total.evaluation_failing > 0, "{total:?}");
}

#[test]
fn alerting_holds_a1_to_a3_under_every_fault() {
    let total = sweep(0..40, Options::FAULTY);
    assert_exercised(&total);
}

#[test]
fn three_nodes_share_the_ticks_and_the_outbox() {
    let options = Options {
        nodes: 3,
        ..Options::FAULTY
    };
    let total = sweep(1_000..1_012, options);
    assert!(total.events > 0 && total.deliveries > 0, "{total:?}");
}

/// The seeds of `seeds` in which the oracle reports a violation that contains `needle`.
fn catching(seeds: Range<u64>, mutant: Mutant, needle: &str) -> Vec<u64> {
    let options = Options {
        mutant: Some(mutant),
        ..Options::FAULTY
    };
    seeds
        .filter(|seed| {
            run_alerts_sim(*seed, options)
                .violations
                .iter()
                .any(|violation| violation.contains(needle))
        })
        .collect()
}

/// The mutant "notify without the outbox transaction": the outbox rows are written after the transition
/// commits, so a crash, a lost connection or a failed statement between the two loses the notification.
#[test]
fn notifying_outside_the_transaction_is_caught() {
    let caught = catching(
        0..12,
        Mutant::NotifyOutsideTransaction,
        "was notified and the outbox has no row for it",
    );
    println!("notify outside the transaction: caught by seeds {caught:?} of 0..12");
    assert!(!caught.is_empty(), "no seed catches the mutant");
}

/// The mutant "skip the for-duration": a breach fires at once.
#[test]
fn skipping_the_for_duration_is_caught() {
    let caught = catching(0..6, Mutant::SkipForDuration, "and the rule's `for` is");
    println!("skip the for-duration: caught by seeds {caught:?} of 0..6");
    assert!(!caught.is_empty(), "no seed catches the mutant");
}

/// The mutant "ignore the no-data policy": a series without rows keeps its state.
#[test]
fn ignoring_the_no_data_policy_is_caught() {
    let caught = catching(
        0..6,
        Mutant::IgnoreNoDataPolicy,
        "its scenario and no-data policy give",
    );
    println!("ignore the no-data policy: caught by seeds {caught:?} of 0..6");
    assert!(!caught.is_empty(), "no seed catches the mutant");
}

#[test]
fn a_run_is_a_function_of_its_seed() {
    let a = run_alerts_sim(7, Options::FAULTY);
    let b = run_alerts_sim(7, Options::FAULTY);
    assert_eq!(a.stats, b.stats);
    assert_ne!(a.stats, run_alerts_sim(8, Options::FAULTY).stats);
}

#[test]
#[ignore = "replays the seed in CRUCIBLE_SIM_SEED"]
fn replay_seed() {
    let seed = std::env::var("CRUCIBLE_SIM_SEED")
        .expect("set CRUCIBLE_SIM_SEED")
        .parse()
        .expect("a u64 seed");
    let outcome = run_alerts_sim(seed, Options::FAULTY);
    println!("{outcome:#?}");
    assert_holds(&outcome);
}
