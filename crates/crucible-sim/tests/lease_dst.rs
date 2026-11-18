//! The lease simulation suite (contract 11.7): leader election over `PgModel` under failovers, commits with
//! an unknown outcome, aborted transactions, stalled leaders and database clock steps.
//!
//! Replay one seed:
//!
//! ```sh
//! CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test lease_dst replay_seed --run-ignored all --no-capture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
#![allow(clippy::disallowed_methods)]

use std::ops::Range;

use crucible_sim::lease_sim::{Options, Outcome, Stats, run_lease_sim};

const REPLAY: &str =
    "cargo nextest run -p crucible-sim --test lease_dst replay_seed --run-ignored all --no-capture";

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
        let outcome = run_lease_sim(seed, options);
        assert_holds(&outcome);
        total.add(&outcome.stats);
    }
    println!("seeds {seeds:?} {options:?}: {total:?}");
    total
}

/// A sweep that never took a lease over, stalled past its window, was fenced, lost a commit or failed over
/// would prove nothing.
fn assert_exercised(total: &Stats) {
    assert!(
        total.acquired > 0 && total.takeovers > 0 && total.renewed > 0 && total.released > 0,
        "{total:?}"
    );
    assert!(
        total.cursor_writes > 0 && total.transactions > 0,
        "{total:?}"
    );
    assert!(total.stale_attempts > 0 && total.fenced > 0, "{total:?}");
    assert!(
        total.commit_unknown > 0 && total.acquire_unknown > 0 && total.unavailable > 0,
        "{total:?}"
    );
    assert!(
        total.serialization_failures > 0 && total.failovers > 0,
        "{total:?}"
    );
}

#[test]
fn one_leader_acts_per_fencing_token_under_faults_and_clock_steps() {
    let total = sweep(0..150, Options::FAULTY);
    assert_exercised(&total);
    assert!(total.clock_steps > 0, "{total:?}");
}

#[test]
fn the_local_window_is_safe_while_the_database_clock_is_steady() {
    let total = sweep(1_000..1_150, Options::STEADY_CLOCK);
    assert_exercised(&total);
    assert_eq!(total.clock_steps, 0);
}

#[test]
fn five_nodes_contend_for_one_lease() {
    let options = Options {
        nodes: 5,
        ..Options::FAULTY
    };
    let total = sweep(2_000..2_050, options);
    assert_exercised(&total);
}

/// The mutant: a leader transaction that skips its fence. The oracle must catch a stale leader's write.
#[test]
fn an_unfenced_transaction_is_caught() {
    let options = Options {
        unfenced: true,
        ..Options::FAULTY
    };
    let caught = (0..20)
        .filter(|seed| {
            run_lease_sim(*seed, options)
                .violations
                .iter()
                .any(|violation| violation.contains("had been acknowledged"))
        })
        .count();
    assert!(caught > 0, "no seed of 20 caught the unfenced write");
    println!("the unfenced mutant was caught in {caught} of 20 seeds");
}

#[test]
fn a_run_is_a_function_of_its_seed() {
    let a = run_lease_sim(7, Options::FAULTY);
    let b = run_lease_sim(7, Options::FAULTY);
    assert_eq!(a.stats, b.stats);
    assert_ne!(a.stats, run_lease_sim(8, Options::FAULTY).stats);
}

#[test]
#[ignore = "replays the seed in CRUCIBLE_SIM_SEED"]
fn replay_seed() {
    let seed = std::env::var("CRUCIBLE_SIM_SEED")
        .expect("set CRUCIBLE_SIM_SEED")
        .parse()
        .expect("a u64 seed");
    let outcome = run_lease_sim(seed, Options::FAULTY);
    println!("{outcome:#?}");
    assert_holds(&outcome);
}
