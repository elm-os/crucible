//! The system simulation suite: the smoke budget that runs with the workspace tests, the determinism meta-test
//! (I14), the mutants with their recorded catching runs, and the regression corpus.
//!
//! The per-pull-request budget (the corpus plus 200 fresh configurations with 3 seeds each) runs in
//! `.github/workflows/sim.yml` through `cargo xtask sim run`. Replay one run:
//!
//! ```sh
//! cargo xtask sim replay --seed <n> --config <mask> [--tier stress] [--mutant <name>]
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeSet;

use crucible_sim::swarm::{ConfigMask, Dim, RunRecord, budget, config_of_seed};
use crucible_sim::system_sim::{Mutant, Options, Outcome, Tier, run_system_sim};

/// The configurations of the per-pull-request budget and the seeds of each, from configuration seed 0. The
/// mutants must be caught inside it.
const BUDGET: (u64, u64) = (200, 3);

/// The configurations the workspace test run covers, with one seed each.
const SMOKE_CONFIGS: u64 = 40;

/// Each mutant with the first run of the budget that catches it and the invariant that run reports.
const MUTANTS: [(Mutant, u64, &str); 15] = [
    (Mutant::SkipWalSync, 0, "liveness"),
    (Mutant::DropToken, 0, "I2"),
    (Mutant::GcAboveCheckpoint, 0, "I12"),
    (Mutant::SilentEviction, 1, "I3"),
    (Mutant::SkipProbeAfterUnknown, 0, "I2"),
    (Mutant::AckBeforeAppend, 0, "I1"),
    (Mutant::DoubleCountRollup, 0, "I7"),
    (Mutant::DropFactOnRetry, 3001, "I1"),
    (Mutant::FrameQuarantineWithoutCopy, 0, "I1"),
    (Mutant::CountAggregateUsage, 1002, "I7"),
    (Mutant::CollectWhatFramesRelyOn, 14_000, "I2"),
    (Mutant::MisclassifyCallerFault, 2001, "I8"),
    (Mutant::SumCaptureSources, 0, "I7"),
    (Mutant::DropRollupMarker, 0, "I2"),
    (Mutant::MarkAfterDuplicateDrop, 107_000, "I7"),
];

fn replay(outcome: &Outcome) -> String {
    let mutant = outcome
        .options
        .mutant
        .map(|m| format!(" --mutant {}", m.name()))
        .unwrap_or_default();
    format!(
        "cargo xtask sim replay --seed {} --config {:#x} --tier {}{mutant}",
        outcome.seed,
        outcome.options.mask.0,
        outcome.options.tier.name()
    )
}

fn assert_holds(outcome: &Outcome) {
    assert!(
        !outcome.failed(),
        "seed {} violated: {:?}\nreplay: {}",
        outcome.seed,
        outcome.violations,
        replay(outcome)
    );
}

#[test]
fn the_smoke_budget_holds_and_reaches_its_targets() {
    let mut targets: BTreeSet<String> = BTreeSet::new();
    let (mut reads, mut whole, mut acked) = (0, 0, 0);
    for (mask, seed) in budget(0, SMOKE_CONFIGS, 1) {
        let outcome = run_system_sim(seed, Options::realistic(mask));
        assert_holds(&outcome);
        targets.extend(outcome.stats.targets());
        // One frame that does not normalize marks every later read of its project `partial: quarantined`, and
        // a row budget of a few KiB holds stage B back behind the batches in flight, so those runs say
        // nothing about how often a read needs a marker.
        if !mask.has(Dim::FrameFaults) && !mask.has(Dim::SmallBudget) {
            reads += outcome.stats.live_reads;
            whole += outcome.stats.live_whole;
        }
        acked += outcome.stats.acked;
    }
    println!("{acked} requests acknowledged; {whole} of {reads} live trace reads whole and strong");
    // The oracle accepts a marker in place of a span while the workload runs, and `lagging` always, so most
    // live reads must need none.
    assert!(acked > 500 && whole * 2 > reads, "{whole} of {reads} reads");
    for target in [
        "frame quarantined",
        "fact write acknowledged",
        "fact write refused",
        "facts resolved against the reference fold",
        "rollups compared with the reference",
        "request throttled (429)",
        "span re-read through its locator",
        "request with a repeated span acknowledged",
        "mcp operation compared with its exchange",
        "mcp operation classified as an error",
        "mcp caller fault",
        "mcp operation captured by a proxy",
        "mcp user subject hashed",
        "mcp aggregates compared with the reference",
        "frame waited for its project salt",
        "directory syncs failed",
        "json request acknowledged",
        "repeated request acknowledged",
        "request refused",
        "span read from the recent index",
        "span read from the store",
        "read answered lagging",
        "read answered partial (store_unavailable)",
        "read answered partial (quarantined)",
        "read refused (store unavailable)",
        "store outage",
        "quarantine",
        "crash during wal recovery",
        "process stop",
        "sealed batch shipped again after a restart",
        "probe read present",
        "insert deduplicated",
        "insert landed late",
        "deduplication entry expired early",
        "parts merged in the store",
        "store read fault",
    ] {
        assert!(targets.contains(target), "no smoke run reached: {target}");
    }
    assert!(
        targets
            .iter()
            .filter(|t| t.starts_with("crash at "))
            .count()
            >= 4,
        "crashes at fewer than four of the shipper's barriers"
    );
}

/// `lagging` is always accepted while the workload runs. Without the store faults that stall the shipper or
/// drop index entries, a live read has little reason to carry any marker.
#[test]
fn live_reads_are_whole_without_store_stalls() {
    let stalls = [
        Dim::Outages,
        Dim::LongOutages,
        Dim::ChReadErrors,
        Dim::TinyRecent,
        Dim::ChTimeouts,
        Dim::ChHangOverload,
        Dim::ChPoison,
        Dim::ChRejects,
        Dim::ChServerErrors,
        Dim::FrameFaults,
        Dim::SmallBudget,
    ];
    let (mut reads, mut whole) = (0, 0);
    for (mask, seed) in budget(0, 12, 1) {
        let calm = stalls.iter().fold(mask, |mask, &dim| mask.without(dim));
        let outcome = run_system_sim(seed, Options::realistic(calm));
        assert_holds(&outcome);
        reads += outcome.stats.live_reads;
        whole += outcome.stats.live_whole;
    }
    println!("{whole} of {reads} live trace reads whole and strong");
    assert!(reads > 500 && whole * 10 > reads * 9, "{whole} of {reads}");
}

#[test]
fn every_dimension_on_and_every_dimension_off_hold() {
    for mask in [
        ConfigMask::default(),
        ConfigMask::FULL.without(Dim::FsyncLie),
    ] {
        for seed in 0..2 {
            assert_holds(&run_system_sim(seed, Options::realistic(mask)));
        }
    }
}

#[test]
fn the_same_inputs_give_the_same_trace_hash() {
    let mut hashes = BTreeSet::new();
    for (mask, seed) in budget(0, 6, 1) {
        let first = run_system_sim(seed, Options::realistic(mask));
        let second = run_system_sim(seed, Options::realistic(mask));
        assert_eq!(first, second, "seed {seed} is not deterministic");
        hashes.insert(first.trace_hash);
    }
    assert_eq!(hashes.len(), 6, "different seeds gave one trace hash");
    // The mask and the tier are inputs too.
    let mask = config_of_seed(0);
    let base = run_system_sim(0, Options::realistic(mask));
    let flipped = run_system_sim(0, Options::realistic(ConfigMask(mask.0 ^ 1)));
    let stress = Options {
        tier: Tier::Stress,
        ..Options::realistic(mask)
    };
    assert_ne!(base.trace_hash, flipped.trace_hash);
    assert_ne!(base.trace_hash, run_system_sim(0, stress).trace_hash);
}

#[test]
fn the_stress_tier_labels_losses_under_the_fsync_lie() {
    let mut lied = 0;
    for (mask, seed) in budget(0, 12, 1) {
        let options = Options {
            tier: Tier::Stress,
            ..Options::realistic(mask)
        };
        let outcome = run_system_sim(seed, options);
        assert_holds(&outcome);
        assert_eq!(outcome.fsync_lie, mask.has(Dim::FsyncLie));
        lied += u64::from(outcome.fsync_lie);
        if !outcome.fsync_lie {
            assert!(outcome.violations.is_empty());
        }
    }
    assert!(lied > 0, "no stress run had the fsync lie");
}

fn with_mutant(mutant: Mutant, seed: u64) -> Outcome {
    let options = Options {
        mutant: Some(mutant),
        ..Options::realistic(config_of_seed(seed))
    };
    run_system_sim(seed, options)
}

#[test]
fn each_mutant_is_caught_by_its_recorded_run() {
    for (mutant, seed, invariant) in MUTANTS {
        let clean = run_system_sim(seed, Options::realistic(config_of_seed(seed)));
        assert_holds(&clean);
        let outcome = with_mutant(mutant, seed);
        let found: Vec<&str> = outcome.violations.iter().map(|v| v.invariant()).collect();
        assert!(
            found.first() == Some(&invariant),
            "mutant {} at seed {seed}: expected a violation of {invariant}, found {:?}\n\
             find the catching run again with the ignored test `mutant_census`",
            mutant.name(),
            outcome.violations
        );
    }
}

/// Lists, per mutant, the first run of the per-pull-request budget that catches it, and how many do.
#[test]
#[ignore = "a search over the per-pull-request budget"]
fn mutant_census() {
    let only = std::env::var("CRUCIBLE_SIM_MUTANT").ok();
    for mutant in Mutant::ALL {
        if only.as_deref().is_some_and(|name| name != mutant.name()) {
            continue;
        }
        let mut first = None;
        let mut caught = 0;
        let mut invariants: BTreeSet<&str> = BTreeSet::new();
        for (_, seed) in budget(0, BUDGET.0, BUDGET.1) {
            let outcome = with_mutant(mutant, seed);
            if let Some(violation) = outcome.violations.first() {
                caught += 1;
                invariants.insert(violation.invariant());
                first.get_or_insert((seed, violation.invariant(), replay(&outcome)));
            }
        }
        println!(
            "{}: caught by {caught} of {} runs as {invariants:?}; first: {first:?}",
            mutant.name(),
            BUDGET.0 * BUDGET.1
        );
    }
}

fn corpus() -> Vec<RunRecord> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../sim/corpus/seeds.jsonl");
    let text = std::fs::read_to_string(path).expect("the corpus file");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| RunRecord::parse(line).expect("a corpus line"))
        .collect()
}

#[test]
fn the_corpus_parses_and_its_failed_seeds_pass() {
    let corpus = corpus();
    assert!(!corpus.is_empty());
    for record in &corpus {
        let tier = Tier::parse(&record.tier).expect("a tier");
        assert!(record.note == "sample" || record.note.starts_with("failed"));
        // Every seed that ever failed runs with the workspace tests; the samples run in sim.yml.
        if record.note.starts_with("failed") {
            let options = Options {
                mask: ConfigMask(record.config),
                tier,
                mutant: None,
            };
            assert_holds(&run_system_sim(record.seed, options));
        }
    }
}
