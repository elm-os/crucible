//! The jobs simulation suite (contract 11.7): the runners of `crucible-jobs` over `PgModel` under crashes,
//! freezes longer than a lease, clock skew, lost connections and lease steals, with the oracle for J1 to J3
//! and the three seeded defects it must catch (docs/testing/jobs-simulation.md).
//!
//! Replay one seed:
//!
//! ```sh
//! CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test jobs_dst replay_seed --run-ignored all --no-capture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
#![allow(clippy::disallowed_methods)]

use std::ops::Range;

use crucible_sim::jobs_sim::{Mutant, Options, Outcome, Stats, run_jobs_sim};

const REPLAY: &str =
    "cargo nextest run -p crucible-sim --test jobs_dst replay_seed --run-ignored all --no-capture";

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
        let outcome = run_jobs_sim(seed, options);
        assert_holds(&outcome);
        total.add(&outcome.stats);
    }
    println!("seeds {seeds:?} {options:?}: {total:?}");
    total
}

/// A sweep in which no node crashed, froze past a lease, lost its lease, retried a row or shut down cleanly
/// would prove nothing.
fn assert_exercised(total: &Stats) {
    assert!(
        total.crashes > 0 && total.restarts > 0 && total.long_freezes > 0,
        "{total:?}"
    );
    assert!(
        total.reserved_outages > 0 && total.failovers > 0 && total.takeovers > 0,
        "{total:?}"
    );
    assert!(
        total.cursor_writes > 0 && total.transactions > 0 && total.landed > 0,
        "{total:?}"
    );
    assert!(
        total.commit_unknown > 0 && total.failed_runs > 0,
        "{total:?}"
    );
    assert!(
        total.enqueued > 0 && total.retries > 0 && total.handler_failures > 0,
        "{total:?}"
    );
    assert!(
        total.completions > 0 && total.stale_completions > 0 && total.ticks > 0,
        "{total:?}"
    );
    assert!(total.tail_writes > 0 && total.released > 0, "{total:?}");
}

#[test]
fn the_runners_hold_j1_to_j3_under_every_fault() {
    let total = sweep(0..60, Options::FAULTY);
    assert_exercised(&total);
    // A stolen lease leaves its holder inside its local window: only the fence stops it.
    assert!(total.steals > 0 && total.lease_lost > 0, "{total:?}");
}

#[test]
fn the_runners_hold_j1_to_j3_without_lease_steals() {
    let total = sweep(1_000..1_060, Options::NO_STEALS);
    assert_exercised(&total);
    assert_eq!(total.steals, 0);
    // The local window is safe: while leases end only by expiry or release, the runner stops the work
    // before any fenced write of it can be refused.
    assert_eq!(total.lease_lost, 0, "{total:?}");
}

#[test]
fn five_nodes_share_the_leases_and_the_queue() {
    let options = Options {
        nodes: 5,
        ..Options::FAULTY
    };
    let total = sweep(2_000..2_020, options);
    assert_exercised(&total);
}

/// The seeds of `seeds` in which the oracle reports a violation that contains `needle`.
fn catching(seeds: Range<u64>, options: Options, needle: &str) -> Vec<u64> {
    seeds
        .filter(|seed| {
            run_jobs_sim(*seed, options)
                .violations
                .iter()
                .any(|violation| violation.contains(needle))
        })
        .collect()
}

/// The mutant "skip the local validity margin": a leader that trusts its lease for the whole TTL issues an
/// action that lands after the lease has expired.
#[test]
fn skipping_the_validity_margin_is_caught() {
    let options = Options {
        mutant: Some(Mutant::SkipValidityMargin),
        ..Options::NO_STEALS
    };
    let caught = catching(0..40, options, "J1: the action");
    println!("skip the validity margin: caught by seeds {caught:?} of 0..40");
    assert!(
        caught.contains(&3),
        "seed 3 no longer catches the mutant: {caught:?}"
    );
}

/// The mutant "renew without compare-and-set": a holder whose lease was taken writes its old token back.
#[test]
fn renewing_without_compare_and_set_is_caught() {
    let options = Options {
        mutant: Some(Mutant::RenewWithoutCompareAndSet),
        ..Options::FAULTY
    };
    let caught = catching(0..20, options, "J1: the lease row went from");
    println!("renew without compare-and-set: caught by seeds {caught:?} of 0..20");
    assert!(
        caught.contains(&0),
        "seed 0 no longer catches the mutant: {caught:?}"
    );
}

/// The mutant "complete without checking the claim": a claimer whose claim ran out completes the row of a
/// newer claim.
#[test]
fn completing_without_the_claim_check_is_caught() {
    let options = Options {
        mutant: Some(Mutant::CompleteWithoutClaimCheck),
        ..Options::FAULTY
    };
    let caught = catching(0..20, options, "J2: ");
    println!("complete without checking the claim: caught by seeds {caught:?} of 0..20");
    assert!(
        caught.contains(&0),
        "seed 0 no longer catches the mutant: {caught:?}"
    );
}

#[test]
fn a_run_is_a_function_of_its_seed() {
    let a = run_jobs_sim(7, Options::FAULTY);
    let b = run_jobs_sim(7, Options::FAULTY);
    assert_eq!(a.stats, b.stats);
    assert_ne!(a.stats, run_jobs_sim(8, Options::FAULTY).stats);
}

#[test]
#[ignore = "replays the seed in CRUCIBLE_SIM_SEED"]
fn replay_seed() {
    let seed = std::env::var("CRUCIBLE_SIM_SEED")
        .expect("set CRUCIBLE_SIM_SEED")
        .parse()
        .expect("a u64 seed");
    let options = match std::env::var("CRUCIBLE_SIM_PROFILE").as_deref() {
        Ok("no-steals") => Options::NO_STEALS,
        _ => Options::FAULTY,
    };
    let outcome = run_jobs_sim(seed, options);
    println!("{outcome:#?}");
    assert_holds(&outcome);
}
