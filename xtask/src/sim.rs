//! `cargo xtask sim`: the swarm runner of the system simulation, the replay tool and the swarm report.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crucible_sim::swarm::{ConfigMask, RunRecord, budget, config_of_seed, report};
use crucible_sim::system_sim::{Mutant, Options, Outcome, Tier, run_system_sim};

pub const USAGE: &str = "  sim run [--configs N] [--seeds M] [--tier realistic|stress] [--base B] [--shard I/N]
          [--jobs J] [--corpus FILE] [--out FILE] [--mutant NAME] [--record]
                       run the regression corpus, then N fresh configurations (from configuration seed B)
                       with M seeds each; write one JSON line per run to FILE (default target/sim/runs.jsonl);
                       --record appends every failing run to the corpus
  sim replay --seed N [--config MASK] [--tier T] [--mutant NAME]
                       run one seed again and print what the oracle found
  sim report [--in FILE] [--out FILE]
                       write the per-dimension trigger and suppressor summary of a run log
                       (default docs/testing/swarm-report.md)";

const CORPUS: &str = "sim/corpus/seeds.jsonl";
const RUN_LOG: &str = "target/sim/runs.jsonl";
const REPORT: &str = "docs/testing/swarm-report.md";

/// The flags of a subcommand: `--name value` pairs and bare `--name` switches.
struct Flags(BTreeMap<String, String>);

impl Flags {
    fn parse(args: impl Iterator<Item = String>, switches: &[&str]) -> Result<Self, String> {
        let mut flags = BTreeMap::new();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let name = arg
                .strip_prefix("--")
                .ok_or_else(|| format!("unexpected argument `{arg}`"))?;
            let value = if switches.contains(&name) {
                String::new()
            } else {
                args.next()
                    .ok_or_else(|| format!("--{name} needs a value"))?
            };
            flags.insert(name.to_owned(), value);
        }
        Ok(Self(flags))
    }

    fn number(&self, name: &str, default: u64) -> Result<u64, String> {
        match self.0.get(name) {
            None => Ok(default),
            Some(text) => {
                parse_number(text).ok_or_else(|| format!("--{name}: `{text}` is not a number"))
            }
        }
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    fn tier(&self) -> Result<Tier, String> {
        let name = self.text("tier").unwrap_or("realistic");
        Tier::parse(name).ok_or_else(|| format!("--tier: `{name}` is not realistic or stress"))
    }

    fn mutant(&self) -> Result<Option<Mutant>, String> {
        match self.text("mutant") {
            None => Ok(None),
            Some(name) => Mutant::parse(name).map(Some).ok_or_else(|| {
                let names: Vec<&str> = Mutant::ALL.iter().map(|m| m.name()).collect();
                format!("--mutant: `{name}` is not one of {}", names.join(", "))
            }),
        }
    }
}

fn parse_number(text: &str) -> Option<u64> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// One run to make: its inputs and why it is in the budget.
#[derive(Clone)]
struct Job {
    seed: u64,
    options: Options,
    note: &'static str,
}

fn replay_command(seed: u64, options: &Options) -> String {
    let mut command = format!(
        "cargo xtask sim replay --seed {seed} --config {:#x} --tier {}",
        options.mask.0,
        options.tier.name()
    );
    if let Some(mutant) = options.mutant {
        command.push_str(" --mutant ");
        command.push_str(mutant.name());
    }
    command
}

/// Runs one job. A panic of the product or of the harness is a failed run.
fn execute(job: &Job) -> RunRecord {
    let Job {
        seed,
        options,
        note,
    } = job.clone();
    match catch_unwind(AssertUnwindSafe(|| run_system_sim(seed, options))) {
        Ok(outcome) => outcome.record(note),
        Err(_) => RunRecord {
            config: options.mask.0,
            seed,
            tier: options.tier.name().to_owned(),
            outcome: "fail".to_owned(),
            invariant: Some("panic".to_owned()),
            trace_hash: String::new(),
            targets: Vec::new(),
            note: note.to_owned(),
        },
    }
}

fn read_records(path: &Path) -> Result<Vec<RunRecord>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(n, line)| {
            RunRecord::parse(line).map_err(|e| format!("{}:{}: {e}", path.display(), n + 1))
        })
        .collect()
}

fn run(flags: &Flags) -> Result<bool, String> {
    let tier = flags.tier()?;
    let mutant = flags.mutant()?;
    let configs = flags.number("configs", 200)?;
    let seeds = flags.number("seeds", 3)?;
    let base = flags.number("base", 0)?;
    let jobs = flags.number(
        "jobs",
        std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
    )?;
    let (shard, shards) = match flags.text("shard") {
        None => (0, 1),
        Some(text) => text
            .split_once('/')
            .and_then(|(i, n)| Some((i.parse::<usize>().ok()?, n.parse::<usize>().ok()?)))
            .filter(|&(i, n)| i < n)
            .ok_or_else(|| format!("--shard: `{text}` is not I/N with I below N"))?,
    };
    let corpus_path = PathBuf::from(flags.text("corpus").unwrap_or(CORPUS));
    let out_path = PathBuf::from(flags.text("out").unwrap_or(RUN_LOG));

    // The corpus first (its realistic lines in the realistic tier, every line in the stress tier), then the
    // fresh configurations. A mutant run skips the corpus: its seeds are known to pass without the defect.
    let mut all = Vec::new();
    if mutant.is_none() && corpus_path.exists() {
        for record in read_records(&corpus_path)? {
            let Some(line_tier) = Tier::parse(&record.tier) else {
                return Err(format!(
                    "{}: unknown tier `{}`",
                    corpus_path.display(),
                    record.tier
                ));
            };
            if line_tier == Tier::Stress && tier == Tier::Realistic {
                continue;
            }
            all.push(Job {
                seed: record.seed,
                options: Options {
                    mask: ConfigMask(record.config),
                    tier: line_tier,
                    mutant: None,
                },
                note: "corpus",
            });
        }
    }
    let corpus_runs = all.len();
    for (mask, seed) in budget(base, configs, seeds) {
        all.push(Job {
            seed,
            options: Options { mask, tier, mutant },
            note: "fresh",
        });
    }
    let mine: Vec<Job> = all
        .into_iter()
        .enumerate()
        .filter(|(n, _)| n % shards == shard)
        .map(|(_, job)| job)
        .collect();
    println!(
        "sim run: {} runs in shard {shard}/{shards} ({corpus_runs} corpus lines and {configs} x {seeds} fresh \
         from configuration seed {base} in total), tier {}, {jobs} threads",
        mine.len(),
        tier.name()
    );

    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, RunRecord)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| {
                loop {
                    let n = next.fetch_add(1, Ordering::SeqCst);
                    let Some(job) = mine.get(n) else { break };
                    let record = execute(job);
                    done.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((n, record));
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap_or_else(|e| e.into_inner());
    done.sort_by_key(|(n, _)| *n);

    if let Some(dir) = out_path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut log = String::new();
    for (_, record) in &done {
        log.push_str(&record.to_json());
        log.push('\n');
    }
    fs::write(&out_path, log).map_err(|e| format!("{}: {e}", out_path.display()))?;

    let failed: Vec<&(usize, RunRecord)> =
        done.iter().filter(|(_, r)| r.outcome == "fail").collect();
    let expected = done
        .iter()
        .filter(|(_, r)| r.outcome == "loss-expected")
        .count();
    println!(
        "sim run: {} passed, {} failed, {expected} with losses expected under the fsync lie, in {:.1} s; log in {}",
        done.len() - failed.len() - expected,
        failed.len(),
        started.elapsed().as_secs_f64(),
        out_path.display()
    );
    for (n, record) in &failed {
        let job = &mine[*n];
        println!(
            "FAILED seed {} config {:#x} ({}): {}\n  replay: {}\n  corpus line: {}",
            record.seed,
            record.config,
            job.note,
            record.invariant.as_deref().unwrap_or("?"),
            replay_command(job.seed, &job.options),
            RunRecord {
                note: "failed".to_owned(),
                ..record.clone()
            }
            .to_json()
        );
    }
    if flags.text("record").is_some() && mutant.is_none() && !failed.is_empty() {
        let mut corpus = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&corpus_path)
            .map_err(|e| format!("{}: {e}", corpus_path.display()))?;
        for (n, record) in &failed {
            if mine[*n].note == "corpus" {
                continue;
            }
            let line = RunRecord {
                note: "failed".to_owned(),
                ..record.clone()
            }
            .to_json();
            writeln!(corpus, "{line}").map_err(|e| format!("{}: {e}", corpus_path.display()))?;
        }
    }
    // With a mutant the run succeeds when the defect was caught.
    Ok(if mutant.is_some() {
        !failed.is_empty()
    } else {
        failed.is_empty()
    })
}

fn print_outcome(outcome: &Outcome) {
    println!(
        "seed {} config {:#x} tier {}: {}",
        outcome.seed,
        outcome.options.mask.0,
        outcome.options.tier.name(),
        outcome.outcome()
    );
    println!("dimensions: {}", outcome.options.mask.names().join(", "));
    println!("trace hash: {}", outcome.trace_hash);
    for violation in &outcome.violations {
        println!("violation ({}): {violation:?}", violation.invariant());
    }
    println!("targets: {}", outcome.stats.targets().join("; "));
    println!("{:#?}", outcome.stats);
}

fn replay(flags: &Flags) -> Result<bool, String> {
    let seed = flags
        .text("seed")
        .and_then(parse_number)
        .ok_or("sim replay needs --seed <n>")?;
    let mask = match flags.text("config") {
        None => config_of_seed(seed),
        Some(text) => ConfigMask(
            parse_number(text).ok_or_else(|| format!("--config: `{text}` is not a mask"))?,
        ),
    };
    let options = Options {
        mask,
        tier: flags.tier()?,
        mutant: flags.mutant()?,
    };
    let outcome = run_system_sim(seed, options);
    print_outcome(&outcome);
    Ok(!outcome.failed())
}

fn write_report(flags: &Flags) -> Result<bool, String> {
    let input = PathBuf::from(flags.text("in").unwrap_or(RUN_LOG));
    let output = PathBuf::from(flags.text("out").unwrap_or(REPORT));
    let records = read_records(&input)?;
    fs::write(&output, report(&records)).map_err(|e| format!("{}: {e}", output.display()))?;
    println!(
        "sim report: {} runs from {} summarized in {}",
        records.len(),
        input.display(),
        output.display()
    );
    Ok(true)
}

/// Runs `cargo xtask sim <args>`. `Ok(false)` when the runs found a failure.
pub fn main(mut args: impl Iterator<Item = String>) -> Result<bool, String> {
    match args.next().as_deref() {
        Some("run") => run(&Flags::parse(args, &["record"])?),
        Some("replay") => replay(&Flags::parse(args, &[])?),
        Some("report") => write_report(&Flags::parse(args, &[])?),
        _ => Err("usage:\n".to_owned() + USAGE),
    }
}
