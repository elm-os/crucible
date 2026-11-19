//! The lifecycle simulation suite (contract 13, invariant I9): the retention executor, the mask scheduler, the
//! deletion executor and the node timer of `crucible-lifecycle` over `ChModel` and `PgModel` under crashes,
//! shipper stalls, store faults and lease steals, with the oracle for L1 to L3 and I9 and the three seeded
//! defects it must catch (docs/testing/lifecycle-simulation.md).
//!
//! Replay one seed:
//!
//! ```sh
//! CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test lifecycle_dst replay_seed --run-ignored all --no-capture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
#![allow(clippy::disallowed_methods)]

use std::ops::Range;

use crucible_sim::lifecycle_sim::{Mutant, Options, Outcome, Stats, run_lifecycle_sim};

const REPLAY: &str = "cargo nextest run -p crucible-sim --test lifecycle_dst replay_seed --run-ignored all --no-capture";

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
        let outcome = run_lifecycle_sim(seed, options);
        assert_holds(&outcome);
        total.add(&outcome.stats);
    }
    println!("seeds {seeds:?} {options:?}: {total:?}");
    total
}

/// A sweep in which nothing was deleted, dropped, masked or hidden would prove nothing.
fn assert_exercised(total: &Stats) {
    assert!(
        total.frames > 0 && total.shipped > 0 && total.rows_checked > 0,
        "{total:?}"
    );
    assert!(
        total.by_trace > 0 && total.by_user > 0 && total.by_session > 0,
        "{total:?}"
    );
    assert!(
        total.completed > 0 && total.completed == total.by_trace + total.by_user + total.by_session,
        "{total:?}"
    );
    assert!(
        total.retention_changes > 0 && total.deletes > 0 && total.drops > 0 && total.masks > 0,
        "{total:?}"
    );
    assert!(total.hidden_reads > 0 && total.stalls > 0, "{total:?}");
}

#[test]
fn retention_and_deletion_are_complete_under_every_fault() {
    let total = sweep(0..40, Options::FAULTY);
    assert_exercised(&total);
    assert!(
        total.crashes > 0 && total.long_downtimes > 0 && total.steals > 0,
        "{total:?}"
    );
    assert!(
        total.failovers > 0 && total.outages > 0 && total.stage_b_drops > 0,
        "{total:?}"
    );
}

#[test]
fn retention_and_deletion_are_complete_without_faults() {
    let total = sweep(1_000..1_020, Options::CALM);
    assert_exercised(&total);
    assert_eq!(total.crashes + total.steals + total.failovers, 0);
}

#[test]
fn five_nodes_share_the_lease_and_every_node_logs_each_request() {
    let options = Options {
        nodes: 5,
        ..Options::FAULTY
    };
    let total = sweep(2_000..2_010, options);
    assert_exercised(&total);
}

/// The seeds of `seeds` in which the oracle reports a violation that starts with `needle`.
fn catching(seeds: Range<u64>, options: Options, needle: &str) -> Vec<u64> {
    seeds
        .filter(|seed| {
            run_lifecycle_sim(*seed, options)
                .violations
                .iter()
                .any(|violation| violation.starts_with(needle))
        })
        .collect()
}

/// The mutant "skip the checkpoint wait": the deletes are issued while a live node still has frames to ship
/// that were received before the request, so their rows land after the delete and stay.
#[test]
fn skipping_the_checkpoint_wait_is_caught() {
    let options = Options {
        mutant: Some(Mutant::SkipCheckpointWait),
        ..Options::CALM
    };
    let caught = catching(3_000..3_020, options, "I9:");
    println!("skip the checkpoint wait: caught in seeds {caught:?}");
    assert!(!caught.is_empty());
    // The same seeds hold without the defect.
    for seed in caught {
        assert_holds(&run_lifecycle_sim(seed, Options::CALM));
    }
}

/// The mutant "drop the mask cycle": deleted rows stay on disk under their masks and no request is verified.
#[test]
fn dropping_the_mask_cycle_is_caught() {
    let options = Options {
        mutant: Some(Mutant::DropMaskCycle),
        ..Options::CALM
    };
    let masked = catching(4_000..4_005, options, "I9:");
    let stuck = catching(4_000..4_005, options, "L3:");
    println!("drop the mask cycle: rows left in seeds {masked:?}, requests open in seeds {stuck:?}");
    assert_eq!(masked.len(), 5);
    assert_eq!(stuck.len(), 5);
}

/// The mutant "delete without fencing": an action reaches the store with no record of it as issued.
#[test]
fn deleting_without_fencing_is_caught() {
    let options = Options {
        mutant: Some(Mutant::DeleteWithoutFencing),
        ..Options::FAULTY
    };
    let caught = catching(5_000..5_005, options, "L1:");
    println!("delete without fencing: caught in seeds {caught:?}");
    assert_eq!(caught.len(), 5);
}

#[test]
#[ignore = "replays the seed in CRUCIBLE_SIM_SEED"]
fn replay_seed() {
    let seed = std::env::var("CRUCIBLE_SIM_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .expect("set CRUCIBLE_SIM_SEED");
    let outcome = run_lifecycle_sim(seed, Options::FAULTY);
    println!("{outcome:#?}");
    assert_holds(&outcome);
}
