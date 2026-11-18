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
use crucible_sim::ship_sim::{BARRIERS, Options, Outcome, Stats, Violation, run_ship_sim};

const REPLAY: &str =
    "cargo nextest run -p crucible-sim --test ship_dst replay_seed --run-ignored all --no-capture";

const MUTANTS: [(&str, Mutant); 6] = [
    ("skip-probe-after-unknown", Mutant::SkipProbeAfterUnknown),
    ("send-before-seal", Mutant::SendBeforeSeal),
    ("drop-token", Mutant::DropToken),
    (
        "checkpoint-before-resolution",
        Mutant::CheckpointBeforeResolution,
    ),
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
    for (total, n) in total.crashes_at.iter_mut().zip(stats.crashes_at) {
        *total += n;
    }
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
    ship.probes_expired += s.probes_expired;
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
    // A shard in which nothing was acknowledged, some barrier never saw a crash, nothing was shipped again after
    // a restart, probed present, probed too early to count, deduplicated, landed late, expired from a window or
    // was quarantined would prove nothing.
    let (ship, store) = (&total.shipper, &total.store);
    assert!(
        total.acked > 0 && total.crashes > 0 && total.stops > 0,
        "{total:?}"
    );
    assert!(total.outages > 0, "{total:?}");
    for (barrier, crashes) in BARRIERS.iter().zip(total.crashes_at) {
        assert!(crashes > 0, "no crash at `{barrier}`: {total:?}");
    }
    assert!(
        ship.sealed > 0 && ship.reshipped > 0 && ship.checkpoints > 0,
        "{total:?}"
    );
    assert!(ship.probes_present > 0 && ship.quarantined > 0, "{total:?}");
    assert!(
        ship.probes_expired > 0 && ship.unknown_outcomes > 0,
        "{total:?}"
    );
    assert!(
        store.landed > 0 && store.deduplicated > 0 && store.late > 0,
        "{total:?}"
    );
    assert!(
        store.expired > 0 && store.rejected > 0 && store.poisoned > 0,
        "{total:?}"
    );
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

/// What a mutant's defect must show up as: the invariant it breaks, not a side effect.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Evidence {
    /// I2: a block or a row landed twice.
    Duplicate,
    /// I2: two batch identities over the same frames were sent.
    IdentityOverlap,
    /// I1: an acknowledged span below the watermark is nowhere.
    Lost,
    /// Contract 5.6: a pair in doubt was quarantined without a probe.
    QuarantinedInDoubt,
    /// Anything else.
    Other,
}

fn evidence(violation: &Violation) -> Evidence {
    match violation {
        Violation::DuplicateRow { .. } => Evidence::Duplicate,
        Violation::Store(what) if what.starts_with("I2") => Evidence::Duplicate,
        Violation::IdentityOverlap { .. } => Evidence::IdentityOverlap,
        Violation::AckedSpanLost { .. } => Evidence::Lost,
        Violation::QuarantinedInDoubt { .. } => Evidence::QuarantinedInDoubt,
        _ => Evidence::Other,
    }
}

fn run_mutant(mutant: Mutant, seed: u64) -> Outcome {
    run_ship_sim(
        seed,
        Options {
            mutant: Some(mutant),
        },
    )
}

/// The per-pull-request seed budget within which every mutant must be caught.
const MUTANT_BUDGET: Range<u64> = 0..500;

/// Each mutant must be caught inside the budget through the invariant it breaks, and the first seed that does so
/// is recorded here. A change to the workload, the fault model or the product moves these seeds: update them
/// from the failure message, which prints the new first catching seed.
const CAUGHT_SKIP_PROBE_AFTER_UNKNOWN: u64 = 4;
const CAUGHT_SEND_BEFORE_SEAL: u64 = 31;
const CAUGHT_DROP_TOKEN: u64 = 1;
const CAUGHT_CHECKPOINT_BEFORE_RESOLUTION: u64 = 0;
const CAUGHT_QUARANTINE_WITHOUT_PROBE: u64 = 6;
const CAUGHT_REJECTION_CLEARS_DOUBT: u64 = 44;

fn assert_caught(mutant: Mutant, by: Evidence, recorded: u64) {
    // The recorded seed first: when nothing moved, one run settles it.
    let shows = |outcome: &Outcome| outcome.violations.iter().any(|v| evidence(v) == by);
    let at_recorded = run_mutant(mutant, recorded);
    let earlier = (MUTANT_BUDGET.start..recorded.min(MUTANT_BUDGET.end))
        .map(|seed| run_mutant(mutant, seed))
        .find(shows);
    let caught = match earlier {
        Some(outcome) => Some(outcome),
        None if shows(&at_recorded) => Some(at_recorded),
        None => (recorded + 1..MUTANT_BUDGET.end)
            .map(|seed| run_mutant(mutant, seed))
            .find(shows),
    };
    let Some(outcome) = caught else {
        panic!("{mutant:?} was not caught as {by:?} by seeds {MUTANT_BUDGET:?}");
    };
    println!("{mutant:?}: seed {} {:?}", outcome.seed, outcome.violations);
    assert_eq!(
        outcome.seed, recorded,
        "{mutant:?} is now first caught as {by:?} by seed {} ({:?}); update the recorded seed",
        outcome.seed, outcome.violations
    );
}

#[test]
fn mutant_skip_probe_after_unknown_is_caught() {
    assert_caught(
        Mutant::SkipProbeAfterUnknown,
        Evidence::Duplicate,
        CAUGHT_SKIP_PROBE_AFTER_UNKNOWN,
    );
}

#[test]
fn mutant_send_before_seal_is_caught() {
    assert_caught(
        Mutant::SendBeforeSeal,
        Evidence::IdentityOverlap,
        CAUGHT_SEND_BEFORE_SEAL,
    );
}

#[test]
fn mutant_drop_token_is_caught() {
    assert_caught(Mutant::DropToken, Evidence::Duplicate, CAUGHT_DROP_TOKEN);
}

#[test]
fn mutant_checkpoint_before_resolution_is_caught() {
    assert_caught(
        Mutant::CheckpointBeforeResolution,
        Evidence::Lost,
        CAUGHT_CHECKPOINT_BEFORE_RESOLUTION,
    );
}

#[test]
fn mutant_quarantine_without_probe_is_caught() {
    assert_caught(
        Mutant::QuarantineWithoutProbe,
        Evidence::QuarantinedInDoubt,
        CAUGHT_QUARANTINE_WITHOUT_PROBE,
    );
}

/// The finding of the ingest model (`docs/testing/formal-model.md`): a rejection after an unknown outcome must
/// leave the pair in doubt, or a block lands twice once its window entry has expired.
#[test]
fn mutant_rejection_clears_doubt_is_caught() {
    assert_caught(
        Mutant::RejectionClearsDoubt,
        Evidence::Duplicate,
        CAUGHT_REJECTION_CLEARS_DOUBT,
    );
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

/// Prints, for each mutant, how many seeds of the budget show each kind of evidence, and the first seeds.
/// `CRUCIBLE_SIM_MUTANT` limits it to one mutant.
#[test]
#[ignore = "census tool: lists the catching seeds of every mutant"]
fn mutant_census() {
    let only = std::env::var("CRUCIBLE_SIM_MUTANT").ok();
    for (name, mutant) in MUTANTS {
        if only.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let mut caught: std::collections::BTreeMap<Evidence, Vec<u64>> = Default::default();
        for seed in MUTANT_BUDGET {
            let outcome = run_mutant(mutant, seed);
            let kinds: std::collections::BTreeSet<Evidence> =
                outcome.violations.iter().map(evidence).collect();
            for kind in kinds {
                caught.entry(kind).or_default().push(seed);
            }
        }
        println!("{name}:");
        for (kind, seeds) in caught {
            println!(
                "  {kind:?}: {} of {} seeds, first {:?}",
                seeds.len(),
                MUTANT_BUDGET.end,
                seeds.iter().take(8).collect::<Vec<_>>()
            );
        }
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
