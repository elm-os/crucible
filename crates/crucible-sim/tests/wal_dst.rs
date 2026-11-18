//! The WAL simulation suite (invariants I1, I12, I13 and I14 over `crucible-wal` on `SimDisk`).
//!
//! Replay one seed:
//!
//! ```sh
//! CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test wal_dst replay_seed --run-ignored all --no-capture
//! ```
//!
//! Add `CRUCIBLE_SIM_TIER=stress`, or `CRUCIBLE_SIM_MUTANT=ack-before-sync`, `gc-above-checkpoint` or
//! `keep-torn-tail`, to replay the seed in that mode. The long run takes `CRUCIBLE_SIM_SEEDS=<count>` and
//! `CRUCIBLE_SIM_FIRST_SEED=<n>`.

#![allow(clippy::disallowed_methods)]

use std::ops::Range;

use crucible_sim::wal_sim::{Options, Outcome, Stats, Violation, run_wal_sim};
use crucible_wal::Mutant;

const REPLAY: &str = "cargo nextest run -p crucible-sim --test wal_dst replay_seed --run-ignored all --no-capture";

fn assert_holds(outcome: &Outcome) {
    assert!(
        outcome.violations.is_empty(),
        "seed {seed} violated: {violations:?}\nreplay: CRUCIBLE_SIM_SEED={seed} {REPLAY}",
        seed = outcome.seed,
        violations = outcome.violations,
    );
}

fn add(total: &mut Stats, stats: &Stats) {
    total.incarnations += stats.incarnations;
    total.crashes += stats.crashes;
    total.recovery_attempts += stats.recovery_attempts;
    total.recovery_retries += stats.recovery_retries;
    total.submitted += stats.submitted;
    total.acked += stats.acked;
    total.failed_appends += stats.failed_appends;
    total.segments_removed += stats.segments_removed;
    total.tail_repairs += stats.tail_repairs;
    total.virtual_micros += stats.virtual_micros;
}

/// Runs the realistic tier over `seeds`; every invariant must hold for every seed.
fn realistic(seeds: Range<u64>) -> Stats {
    let mut total = Stats::default();
    for seed in seeds.clone() {
        let outcome = run_wal_sim(seed, Options::REALISTIC);
        assert_holds(&outcome);
        add(&mut total, &outcome.stats);
    }
    println!("realistic seeds {seeds:?}: {total:?}");
    // A shard that never acknowledged, crashed, retried a recovery, repaired a tail or reclaimed a
    // segment would prove nothing.
    assert!(total.acked > 0 && total.crashes > 0, "{total:?}");
    assert!(total.recovery_retries > 0 && total.tail_repairs > 0, "{total:?}");
    assert!(total.failed_appends > 0 && total.segments_removed > 0, "{total:?}");
    total
}

macro_rules! realistic_shards {
    ($($name:ident: $range:expr,)*) => {$(
        #[test]
        fn $name() {
            realistic($range);
        }
    )*};
}

// The per-pull-request budget: 1,000 seeds, sharded so that nextest runs them on every core.
realistic_shards! {
    realistic_seeds_000: 0..100,
    realistic_seeds_100: 100..200,
    realistic_seeds_200: 200..300,
    realistic_seeds_300: 300..400,
    realistic_seeds_400: 400..500,
    realistic_seeds_500: 500..600,
    realistic_seeds_600: 600..700,
    realistic_seeds_700: 700..800,
    realistic_seeds_800: 800..900,
    realistic_seeds_900: 900..1000,
}

/// The stress tier turns the fsync lie on. A lying disk loses acknowledged frames, and the oracle must
/// say so: this is what shows that the realistic tier's silence means something.
#[test]
fn stress_tier_fsync_lie_is_detected() {
    let mut detected = Vec::new();
    let mut lost = 0;
    for seed in 0..300 {
        let outcome = run_wal_sim(seed, Options::STRESS);
        if !outcome.violations.is_empty() {
            detected.push(seed);
        }
        if outcome
            .violations
            .iter()
            .any(|v| matches!(v, Violation::AckedFrameLost { .. }))
        {
            lost += 1;
        }
    }
    println!(
        "stress seeds 0..300: {} runs with a violation, {lost} with a lost acknowledged frame; first {:?}",
        detected.len(),
        detected.iter().take(8).collect::<Vec<_>>()
    );
    assert!(lost > 0, "the oracle never noticed a lying fsync");
}

fn first_catching_seed(mutant: Mutant, seeds: Range<u64>) -> Option<(u64, Vec<Violation>)> {
    seeds
        .map(|seed| run_wal_sim(seed, Options::mutant(mutant)))
        .find(|outcome| !outcome.violations.is_empty())
        .map(|outcome| (outcome.seed, outcome.violations))
}

/// The per-pull-request seed budget within which every mutant must be caught.
const MUTANT_BUDGET: Range<u64> = 0..200;

/// Each mutant must be caught inside the budget, and its first catching seed is recorded here. A change
/// to the workload, the fault model or the product moves these seeds: update them from the failure
/// message, which prints the new first catching seed.
const CAUGHT_ACK_BEFORE_SYNC: u64 = 0;
const CAUGHT_GC_ABOVE_CHECKPOINT: u64 = 0;
const CAUGHT_KEEP_TORN_TAIL: u64 = 0;

fn assert_caught(mutant: Mutant, recorded: u64) {
    let caught = first_catching_seed(mutant, MUTANT_BUDGET);
    println!("{mutant:?}: {caught:?}");
    let Some((seed, violations)) = caught else {
        panic!("{mutant:?} survived seeds {MUTANT_BUDGET:?}");
    };
    assert_eq!(
        seed, recorded,
        "{mutant:?} is now first caught by seed {seed} ({violations:?}); update the recorded seed"
    );
}

#[test]
fn mutant_ack_before_sync_is_caught() {
    assert_caught(Mutant::AckBeforeSync, CAUGHT_ACK_BEFORE_SYNC);
}

#[test]
fn mutant_gc_above_checkpoint_is_caught() {
    assert_caught(Mutant::GcAboveCheckpoint, CAUGHT_GC_ABOVE_CHECKPOINT);
}

#[test]
fn mutant_keep_torn_tail_is_caught() {
    assert_caught(Mutant::KeepTornTail, CAUGHT_KEEP_TORN_TAIL);
}

/// I14: one seed gives one trace hash, and different seeds give different hashes.
#[test]
fn same_seed_same_trace_hash() {
    let mut hashes = std::collections::BTreeSet::new();
    for seed in 0..40 {
        for options in [Options::REALISTIC, Options::STRESS] {
            let first = run_wal_sim(seed, options);
            let second = run_wal_sim(seed, options);
            assert_eq!(first, second, "seed {seed} is not deterministic");
        }
        hashes.insert(run_wal_sim(seed, Options::REALISTIC).trace_hash);
    }
    assert_eq!(hashes.len(), 40, "two seeds share a trace hash");
}

fn env_u64(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number")),
    )
}

/// The long run: many seeds of the realistic tier.
#[test]
#[ignore = "long run: CRUCIBLE_SIM_SEEDS=<count> (default 100000), CRUCIBLE_SIM_FIRST_SEED=<n> (default 1000)"]
fn realistic_long_run() {
    let first = env_u64("CRUCIBLE_SIM_FIRST_SEED").unwrap_or(1_000);
    let count = env_u64("CRUCIBLE_SIM_SEEDS").unwrap_or(100_000);
    realistic(first..first + count);
}

/// Replays the seed named by `CRUCIBLE_SIM_SEED` and prints its outcome.
#[test]
#[ignore = "replay tool: set CRUCIBLE_SIM_SEED"]
fn replay_seed() {
    let Some(seed) = env_u64("CRUCIBLE_SIM_SEED") else {
        println!("CRUCIBLE_SIM_SEED is not set; nothing to replay");
        return;
    };
    let mut options = match std::env::var("CRUCIBLE_SIM_TIER").as_deref() {
        Ok("stress") => Options::STRESS,
        _ => Options::REALISTIC,
    };
    options.mutant = match std::env::var("CRUCIBLE_SIM_MUTANT").as_deref() {
        Ok("ack-before-sync") => Some(Mutant::AckBeforeSync),
        Ok("gc-above-checkpoint") => Some(Mutant::GcAboveCheckpoint),
        Ok("keep-torn-tail") => Some(Mutant::KeepTornTail),
        Ok(other) => panic!("unknown CRUCIBLE_SIM_MUTANT {other}"),
        Err(_) => None,
    };
    let outcome = run_wal_sim(seed, options);
    println!("{options:?}\n{outcome:#?}");
}
