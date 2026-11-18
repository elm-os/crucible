//! The shipper simulation suite (invariants I1, I2, I12, I13 and I14 over `crucible-shipper`, `crucible-wal` on
//! `SimDisk`, and the store model).
//!
//! Replay one seed:
//!
//! ```sh
//! CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test ship_dst replay_seed --run-ignored all --no-capture
//! ```
//!
//! Add `CRUCIBLE_SIM_MUTANT=skip-probe-after-unknown`, `send-before-seal`, `drop-token`,
//! `checkpoint-before-resolution`, `quarantine-without-probe` or `rejection-clears-doubt` to replay the seed with
//! that defect. The long run takes `CRUCIBLE_SIM_SEEDS=<count>` and `CRUCIBLE_SIM_FIRST_SEED=<n>`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
#![allow(clippy::disallowed_methods)]

use std::ops::Range;

use crucible_shipper::Mutant;
use crucible_sim::ship_sim::{Options, Outcome, Stats, run_ship_sim};

const REPLAY: &str =
    "cargo nextest run -p crucible-sim --test ship_dst replay_seed --run-ignored all --no-capture";

const MUTANTS: [(&str, Mutant); 6] = [
    ("skip-probe-after-unknown", Mutant::SkipProbeAfterUnknown),
    ("send-before-seal", Mutant::SendBeforeSeal),
    ("drop-token", Mutant::DropToken),
    ("checkpoint-before-resolution", Mutant::CheckpointBeforeResolution),
    ("quarantine-without-probe", Mutant::QuarantineWithoutProbe),
    ("rejection-clears-doubt", Mutant::RejectionClearsDoubt),
];

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
    total.barrier_crashes += stats.barrier_crashes;
    total.stops += stats.stops;
    total.submitted += stats.submitted;
    total.acked += stats.acked;
    total.resent += stats.resent;
    total.outages += stats.outages;
    total.quarantines += stats.quarantines;
    total.virtual_micros += stats.virtual_micros;
    let (ship, s) = (&mut total.shipper, &stats.shipper);
    ship.sealed += s.sealed;
    ship.reshipped += s.reshipped;
    ship.checkpoints += s.checkpoints;
    ship.inserts += s.inserts;
    ship.unknown_outcomes += s.unknown_outcomes;
    ship.probes += s.probes;
    ship.probes_present += s.probes_present;
    ship.quarantined += s.quarantined;
    let (store, s) = (&mut total.store, &stats.store);
    store.inserts += s.inserts;
    store.landed += s.landed;
    store.deduplicated += s.deduplicated;
    store.unknown += s.unknown;
    store.late += s.late;
    store.rejected += s.rejected;
    store.poisoned += s.poisoned;
    store.probes += s.probes;
    store.probes_present += s.probes_present;
    store.expired += s.expired;
}

/// Runs `seeds` without a mutant; every invariant must hold for every seed.
fn realistic(seeds: Range<u64>) -> Stats {
    let mut total = Stats::default();
    for seed in seeds.clone() {
        let outcome = run_ship_sim(seed, Options::default());
        assert_holds(&outcome);
        add(&mut total, &outcome.stats);
    }
    println!("ship seeds {seeds:?}: {total:#?}");
    // A shard in which nothing was acknowledged, crashed at a barrier, shipped again after a restart, probed
    // present, deduplicated, landed late, expired from a window or was quarantined would prove nothing.
    let (ship, store) = (&total.shipper, &total.store);
    assert!(total.acked > 0 && total.crashes > 0 && total.stops > 0, "{total:?}");
    assert!(total.barrier_crashes > 0 && total.outages > 0, "{total:?}");
    assert!(ship.sealed > 0 && ship.reshipped > 0 && ship.checkpoints > 0, "{total:?}");
    assert!(ship.probes_present > 0 && ship.quarantined > 0, "{total:?}");
    assert!(store.landed > 0 && store.deduplicated > 0 && store.late > 0, "{total:?}");
    assert!(store.expired > 0 && store.rejected > 0 && store.poisoned > 0, "{total:?}");
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

// The per-pull-request budget: 500 seeds, sharded so that nextest runs them on every core.
realistic_shards! {
    ship_seeds_000: 0..100,
    ship_seeds_100: 100..200,
    ship_seeds_200: 200..300,
    ship_seeds_300: 300..400,
    ship_seeds_400: 400..500,
}

fn first_catching_seed(mutant: Mutant, seeds: Range<u64>) -> Option<Outcome> {
    seeds
        .map(|seed| {
            run_ship_sim(
                seed,
                Options {
                    mutant: Some(mutant),
                },
            )
        })
        .find(|outcome| !outcome.violations.is_empty())
}

/// The per-pull-request seed budget within which every mutant must be caught.
const MUTANT_BUDGET: Range<u64> = 0..500;

/// Each mutant must be caught inside the budget, and its first catching seed is recorded here. A change to the
/// workload, the fault model or the product moves these seeds: update them from the failure message, which
/// prints the new first catching seed.
const CAUGHT_SKIP_PROBE_AFTER_UNKNOWN: u64 = 0;
const CAUGHT_SEND_BEFORE_SEAL: u64 = 0;
const CAUGHT_DROP_TOKEN: u64 = 0;
const CAUGHT_CHECKPOINT_BEFORE_RESOLUTION: u64 = 0;
const CAUGHT_QUARANTINE_WITHOUT_PROBE: u64 = 0;
const CAUGHT_REJECTION_CLEARS_DOUBT: u64 = 0;

fn assert_caught(mutant: Mutant, recorded: u64) {
    let caught = first_catching_seed(mutant, MUTANT_BUDGET);
    let Some(outcome) = caught else {
        panic!("{mutant:?} survived seeds {MUTANT_BUDGET:?}");
    };
    println!("{mutant:?}: seed {} {:?}", outcome.seed, outcome.violations);
    assert_eq!(
        outcome.seed, recorded,
        "{mutant:?} is now first caught by seed {} ({:?}); update the recorded seed",
        outcome.seed, outcome.violations
    );
}

#[test]
fn mutant_skip_probe_after_unknown_is_caught() {
    assert_caught(Mutant::SkipProbeAfterUnknown, CAUGHT_SKIP_PROBE_AFTER_UNKNOWN);
}

#[test]
fn mutant_send_before_seal_is_caught() {
    assert_caught(Mutant::SendBeforeSeal, CAUGHT_SEND_BEFORE_SEAL);
}

#[test]
fn mutant_drop_token_is_caught() {
    assert_caught(Mutant::DropToken, CAUGHT_DROP_TOKEN);
}

#[test]
fn mutant_checkpoint_before_resolution_is_caught() {
    assert_caught(
        Mutant::CheckpointBeforeResolution,
        CAUGHT_CHECKPOINT_BEFORE_RESOLUTION,
    );
}

#[test]
fn mutant_quarantine_without_probe_is_caught() {
    assert_caught(Mutant::QuarantineWithoutProbe, CAUGHT_QUARANTINE_WITHOUT_PROBE);
}

#[test]
fn mutant_rejection_clears_doubt_is_caught() {
    assert_caught(Mutant::RejectionClearsDoubt, CAUGHT_REJECTION_CLEARS_DOUBT);
}

/// I14: one seed gives one outcome and one trace hash, and different seeds give different hashes.
#[test]
fn same_seed_same_trace_hash() {
    let mut hashes = std::collections::BTreeSet::new();
    for seed in 0..12 {
        let first = run_ship_sim(seed, Options::default());
        let second = run_ship_sim(seed, Options::default());
        assert_eq!(first, second, "seed {seed} is not deterministic");
        hashes.insert(first.trace_hash);
    }
    assert_eq!(hashes.len(), 12, "two seeds share a trace hash");
}

/// Prints every seed of the budget that catches each mutant.
#[test]
#[ignore = "census tool: lists the catching seeds of every mutant"]
fn mutant_census() {
    for (name, mutant) in MUTANTS {
        let mut caught = Vec::new();
        let mut kinds = std::collections::BTreeSet::new();
        for seed in MUTANT_BUDGET {
            let outcome = run_ship_sim(
                seed,
                Options {
                    mutant: Some(mutant),
                },
            );
            if let Some(first) = outcome.violations.first() {
                caught.push(seed);
                let text = format!("{first:?}");
                kinds.insert(text.split([' ', '(', '{']).next().unwrap_or("").to_owned());
            }
        }
        println!(
            "{name}: {} of {} seeds, first {:?}, as {kinds:?}",
            caught.len(),
            MUTANT_BUDGET.end,
            caught.iter().take(12).collect::<Vec<_>>()
        );
    }
}

fn env_u64(name: &str) -> Option<u64> {
    let value = std::env::var(name).ok()?;
    Some(
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a number")),
    )
}

/// The long run: many seeds without a mutant.
#[test]
#[ignore = "long run: CRUCIBLE_SIM_SEEDS=<count> (default 20000), CRUCIBLE_SIM_FIRST_SEED=<n> (default 500)"]
fn long_run() {
    let first = env_u64("CRUCIBLE_SIM_FIRST_SEED").unwrap_or(500);
    let count = env_u64("CRUCIBLE_SIM_SEEDS").unwrap_or(20_000);
    let mut total = Stats::default();
    for seed in first..first + count {
        let outcome = run_ship_sim(seed, Options::default());
        assert_holds(&outcome);
        add(&mut total, &outcome.stats);
    }
    println!("ship seeds {first}..{}: {total:#?}", first + count);
}

/// Replays the seed named by `CRUCIBLE_SIM_SEED` and prints its outcome.
#[test]
#[ignore = "replay tool: set CRUCIBLE_SIM_SEED"]
fn replay_seed() {
    let Some(seed) = env_u64("CRUCIBLE_SIM_SEED") else {
        println!("CRUCIBLE_SIM_SEED is not set; nothing to replay");
        return;
    };
    let mutant = match std::env::var("CRUCIBLE_SIM_MUTANT") {
        Ok(name) => Some(
            MUTANTS
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap_or_else(|| panic!("unknown CRUCIBLE_SIM_MUTANT {name}"))
                .1,
        ),
        Err(_) => None,
    };
    let options = Options { mutant };
    let outcome = run_ship_sim(seed, options);
    println!("{options:?}\n{outcome:#?}");
}
