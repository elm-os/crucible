# System simulation

How the whole node (milestone 1 and the write path of milestone 2) is verified in `crucible-sim` with deterministic simulation and swarm testing,
and how to replay a failing run. The method is in [`methodology.md`](methodology.md); the invariants are in
[`invariants.md`](invariants.md); the pieces this suite builds on are in [`wal-simulation.md`](wal-simulation.md)
and [`shipper-simulation.md`](shipper-simulation.md).

## The model

One simulated node, on one thread with a paused virtual clock and seeded random streams
(`crucible_sim::system_sim`):

| Piece | Real or model | What runs |
|---|---|---|
| Clients | Model | Seeded OTLP clients that build protobuf and JSON bodies and call the ingest router in process. Spans report a session, a user, an environment, a release, a model, usage, a provided cost and an error status from small domains. With `facts`, the clients also write scores, tag and flag edits and corrections |
| Ingest | Real | `crucible_ingest::Ingest::router()`: authentication, admission, the scan, the WAL append, the ack; and `Ingest::append_facts`, the entry point the fact routes will call |
| WAL | Real | `crucible-wal` on `SimDisk`, the disk fault model of contract 2.4 |
| Shipper | Real | `crucible-shipper`: stage A and B, the span tables, the rollups and the fact tables, seals, inserts, probes, pair and frame quarantine, checkpoints, garbage collection |
| Recent-span index | Real | `crucible-recent`, fed by stage B through `crucible_shipper::RecentHook` |
| Query service | Real | `crucible_query::QueryService`: trace by id and the trace list |
| ClickHouse | Model | `ChModel`: inserts, deduplication, probes, the typed-plan interpreter, merges, and seeded faults |
| Keys | Real | `crucible_auth::StaticKeys`, one secret key per project |

### The boundary

The ClickHouse adapter (`crucible-store-clickhouse`) is outside the simulation: the `clickhouse` crate spawns
tasks, reads time and draws randomness on its own. `ChModel` stands in for it behind `TelemetryStore`. What keeps
the model honest is the conformance suite: `crucible-conformance` runs the same typed-plan reads against `ChModel`
and against real ClickHouse (26.3, 26.8 and the latest release) and compares the rows. The model may be stricter
than a server and is never more permissive. That holds for the query-class guardrails too (contract 8.4): a
read runs under the limits its plan carries, and the model refuses it as `query_too_broad` above the rows or
bytes to read, as `query_timeout` above the execution time and as `query_memory_exceeded` above the memory. The
model has no parts index, so each figure is an upper bound of a server's: the rows and bytes of every row that
ever landed in the tables the plan reads (less dropped partitions), that scan at 10,000,000 rows a second, and
the bytes of every row that passed the plan's filters plus the buckets a zero fill adds. The conformance suite
compares the row and byte refusals with a server; the time and memory refusals of a server are its C12 row, and
those of the model are tested in `crates/crucible-conformance/tests/model_reads.rs`. The simulated data sets
are far below the class limits.

Also outside: the network (requests enter at the router, so HTTP framing and TLS are not simulated), Postgres and
the control plane (later milestones), and several nodes (milestone 1 is one node).

The simulation's `WalView` re-reads a span through its locator as the binary does (`Wal::read_at`, then stage A),
and the oracle follows the locator of a span that a read returned from the index: a frame that reads must hold
that span at the locator's ordinal. The fact routes do not exist yet, so facts are checked in the store model's
parts, not through a read.

### A run

A run is a function of three inputs: the **seed**, the **configuration mask** and the **tier**.

1. Incarnations. Each one recovers the WAL (through seeded crashes and faults, compared with an uninterrupted
   reference recovery), rebuilds the index at the recovered watermark, starts the shipper, the router and the
   query service, and runs clients and readers. It ends at the first of: a seeded number of disk operations, a
   crash barrier of the shipper (contract 5.9), a seeded virtual time (which aborts every task at whatever await
   point it has reached), or a process stop. Inserts in flight outlive the crash and may land later.
2. Quiescence. Every fault is healed and the log is drained. After the replay retention, the visibility grace
   and the last insert in flight have passed, the store, the WAL, the index and every read are checked.
3. One more restart, after which nothing may change and every read must still hold.

## Dimensions

A configuration is a bit mask with one fair coin per dimension (`crucible_sim::swarm::Dim`). The mask decides
which features exist in a run; the seed decides everything else (magnitudes, schedules, payload bytes).
Configuration seed `c` gives the mask and the run seeds `c * 1000 + s`.

| Kind | Dimensions |
|---|---|
| Workload | `json`, `large_payloads`, `deep_nesting`, `bad_unicode`, `rootless`, `concurrent_reads`, `client_retries` (whole requests again, and single spans again inside a new request), `multi_project`, `bursts`, `multi_day`, `facts` |
| Faults | `disk_crash_points`, `barrier_crashes`, `crash_loops`, `crash_in_recovery`, `clean_stops`, `disk_write_errors`, `disk_read_faults`, `disk_sync_errors`, `disk_full`, `slow_sync`, `ch_timeouts`, `ch_ack_lost`, `ch_server_errors`, `ch_rejects`, `ch_hang_overload`, `ch_poison`, `ch_probe_errors`, `ch_read_errors`, `window_expiry`, `outages`, `long_outages`, `wall_clock_steps`, `fsync_lie` (stress tier only), `frame_faults` (a seeded share of the durable frames does not normalize) |
| Config | `wal_compression_off`, `small_batches`, `week_partitions`, `tiny_recent`, `small_segments`, `clustered`, `one_connection`, `replay_retention`, `project_rate` (a per-project byte rate), `small_budget` (a `shipper.encoded_bytes` budget of 20 or 200 KiB) |

That is 45 coins. New dimensions take the next bit of the mask and draw their settings after every earlier
draw, so a recorded mask and seed keep their meaning. With 200 configurations, any 6 chosen dimensions are all on in at least one of them with
probability 95.7% (`1 - (1 - 2^-6)^200`).

## Tiers

| Tier | What it adds | What a violation means |
|---|---|---|
| `realistic` | Faults at rates and lengths a deployment can meet. `fsync_lie` has no effect | A failure. It blocks the merge |
| `stress` | Fault rates times three, twice the incarnations, store outages up to 10 minutes, and with `fsync_lie` a disk that reports syncs that did not happen | Without the lie, a failure. With the lie, acknowledged data can be lost by construction: the run is recorded as `loss-expected`, and only a run that does not finish fails |

## Invariants

| Invariant | Continuous (after every ack and at every restart) | Quiescence |
|---|---|---|
| I1 | At every restart: every span of every acknowledged request is in `span_core` and `span_payload`, or in a durable frame above the recovered watermark, or in a quarantine copy: the frames of a quarantined pair, or the copy of a frame that does not normalize, which must hold the frame its manifest names. A frame quarantined as a frame has no row anywhere | The same after the drain, and the watermark has passed the last frame that warrants a seal |
| I2 | The model refuses a malformed block and records a second block of one (batch, table, partition); no (table, LSN, span) row twice; no span with more rows than requests that carried it; every row belongs to a request that was sent; no two batch identities over the same frames; a pair the store had accepted is quarantined only after a probe that read absent | In runs without a quarantine directory, repeats included: every root span has its head row and the `trace_stats` span counts sum to the `span_core` rows |
| I4 | Every fact row is a fact that was written and lands once (I2); every acknowledged fact is in its table, in a durable `FactBatch` frame above the watermark, or in a quarantine copy (I1). The rows resolve through `crucible_records::fold_facts` to what the facts as written resolve to, in either order of the rows | The same after the drain |
| I5 | A refusal for load is a 429 or a 503 with `Retry-After`, a 429 only with `project_rate`; a fact write is refused only for load. The rows the shipper holds stay within `shipper.encoded_bytes` plus one frame | |
| I7 | Every `span_core` row carries the tokens, the cost in nano-USD and the error status its span reported | Without a quarantined pair: `trace_stats`, `session_rollup`, `user_rollup` and `span_metrics`, merged by key, equal the reference aggregation over `span_core` exactly (counts, trace counts, error counts, token and cost sums, first and last seen, both histograms). One binary runs the whole simulation, so the upgrade exception of contract 5.3 does not arise |
| I3 | A trace-by-id read through the query service after every ack, and from concurrent readers for requests of any incarnation. A response that omits an acknowledged span must carry `partial` or `consistency: lagging`. `not_found` for an acknowledged trace is a violation. A marker must be warranted by the configuration: `store_unavailable` only with `outages`, `long_outages`, `ch_read_errors` or `tiny_recent`; `quarantined` only with `ch_poison` or `frame_faults` | Every acknowledged request is read back, and the whole trace list of every project must hold every acknowledged trace that has a root span. No marker is accepted, except `quarantined` when a quarantine directory exists. Repeated after the last restart |
| I6 | No trace-by-id response holds a span the project did not send, and no list holds a trace of another project (with `multi_project` the projects share trace ids) | The same |
| I12 | The recovered log equals the uninterrupted reference recovery although crashes cut the real one | A restart after the drain sends no insert and leaves the store and the watermark as they were |
| I13 | | No segment wholly at or below the watermark is left, and the index is empty once the grace has passed |
| I14 | | The trace hash covers every event of the run, read results included. The same seed, mask and tier give the same hash and outcome (`system_dst.rs`, on every pull request) |

While the workload runs, a marker excuses a missing span, and `consistency: lagging` is always accepted there:
the oracle cannot tell from outside whether stage B had a reason to trail. So the suite also requires that live
reads mostly need no marker: over the smoke budget more than half of the live trace reads return every span,
whole and strong, and in configurations with the store faults switched off (outages, read faults, insert
faults, poison pairs, the tiny index bound, frame faults, the small row budget) more than 90% do (measured: 698
of 708). The smoke ratio leaves out the runs with `frame_faults` or `small_budget`: one quarantined frame marks
every later read of its project, and a small row budget holds stage B behind the batches in flight.

### Repeats and restarts

A batch may land some pairs before a crash and the rest after it, so the restart must normalize the batch to
the same spans: the same repeats dropped, the same provisional heads. Stage B is rebuilt from the retained
prefix of the log, and garbage collection shortens that prefix. Three rules of the shipper and stage B keep
the two attempts equal, and I7 and the quiescence half of I2 check them in every run with `client_retries`:

- every sighting of a span key or a trace moves it to the newest position of its set, and after each
  collection stage B forgets what was last sighted below the first retained LSN, so the live state is what a
  rebuild from the retained prefix gives (a property test of `crucible-normalize`);
- a frame whose admission used an earlier sighting keeps the log from that sighting on until the frame's batch
  is checkpointed, and nothing is collected while a batch that recovery found waits for its frames
  (`ship_directed.rs`, `a_restart_drops_the_repeats_that_the_first_attempt_dropped`);
- after a failed collection no frame is admitted until one succeeds, because a removal that is not durable
  leaves open what a restart will find (`ship_directed.rs`: a hint that cannot be replaced, and a removal
  whose directory sync fails). While it retries at every tail poll, the failure is logged once per
  `gc_interval` with a count.

Nine corpus lines (invariant I7, seeds 44368508872000 and following) are runs that failed before these rules:
the rollups counted a repeat that `span_core` did not hold, or the reverse.

## Mutants

Each defect is behind a `test-mutants` cargo feature that only `crucible-sim` enables. Each must be caught inside
the per-pull-request budget (200 configurations from configuration seed 0, 3 seeds each), and its first catching
run is recorded in `crates/crucible-sim/tests/system_dst.rs`. The same run must pass without the defect.

| Mutant | Crate | Defect | Caught as | Seed | Config |
|---|---|---|---|---|---|
| `skip-wal-sync` | `crucible-wal` | A group's waiters complete before its `sync_data` | I3: a trace read right after the ack answers `not_found` (86 other runs of the budget report I1 after a crash) | 0 | `0xaeca437a47` |
| `drop-token` | `crucible-shipper` | Every insert attempt carries a new deduplication identity | I2: a second block of one batch lands | 1000 | `0x1e574beef1d` |
| `gc-above-checkpoint` | `crucible-wal` | Garbage collection removes segments above the checkpoint watermark | I12: recovery finds a gap between the watermark and the oldest segment | 14000 | `0x17dced818fe` |
| `silent-eviction` | `crucible-recent` | Entries dropped at the index bound leave no gap | I3: a read omits an acknowledged span and looks whole | 5000 | `0x48461657e2` |
| `skip-probe-after-unknown` | `crucible-shipper` | A retry after an unknown outcome skips the probe | I2: a second block of one batch lands | 7000 | `0x1c5fa7b02ff` |
| `ack-before-append` | `crucible-ingest` | The client gets 200 before the WAL append resolves | I3: a trace read right after the ack answers `not_found` (110 other runs report I1) | 0 | `0xaeca437a47` |
| `collect-what-frames-rely-on` | `crucible-shipper` | Garbage collection removes segments that the admission of an unchecked frame relied on | I7: after a restart the rollups count a repeat that `span_core` does not hold | 910904000 | `0x911e418a948` |

Of the 600 runs of the budget, the first six mutants are caught by 589, 131, 38, 35, 96 and 480 runs, in the
order of the table. No run of the budget catches `collect-what-frames-rely-on`: it needs a repeat, a collection
and a crash inside one unchecked batch, which about one run in 200 has. Its recorded run is a line of the
regression corpus, and six of the nine corpus lines of this defect catch it.

A change to the workload, the fault model or the product moves the recorded runs. The failing assertion says so;
the ignored test `mutant_census` lists the first catching run of each mutant again.

## The regression corpus

`sim/corpus/seeds.jsonl` holds one JSON line per run: the configuration mask, the seed, the tier, the outcome,
the violated invariant if any, the trace hash at the time, the coverage targets the run reached, and a note
(`sample` or `failed: <what it found>`). It holds every run that ever failed and a representative sample, and it
is never pruned. Every pull request runs the whole corpus; the failed lines also run with the workspace tests.

`cargo xtask sim run` writes the same line format for every run to `target/sim/runs.jsonl`, and `cargo xtask sim
report` turns such a log into [`swarm-report.md`](swarm-report.md): for each coverage target and each violated
invariant, the dimensions that raise its rate (triggers) and the ones that lower it (suppressors).

## Commands

```sh
# The per-pull-request budget: the corpus, then 200 fresh configurations with 3 seeds each
cargo xtask sim run --configs 200 --seeds 3

# One shard of four, from another configuration seed, in the stress tier
cargo xtask sim run --configs 5000 --seeds 1 --tier stress --base 7000 --shard 0/4

# Replay one run and print what the oracle found
cargo xtask sim replay --seed 17002 --config 0x139c20f0394
cargo xtask sim replay --seed 17002                       # the mask the budget pairs with the seed
cargo xtask sim replay --seed 17002 --tier stress --mutant drop-token

# The per-dimension trigger and suppressor summary of the last run log
cargo xtask sim report

# Check that the budget catches a mutant (succeeds when it does)
cargo xtask sim run --mutant silent-eviction

# The smoke budget, the determinism test and the mutants, as the workspace test run has them
cargo nextest run -p crucible-sim --test system_dst
```

Mutant names: `skip-wal-sync`, `drop-token`, `gc-above-checkpoint`, `silent-eviction`,
`skip-probe-after-unknown`, `ack-before-append`.

A failing run prints its replay command and the line to append to the corpus; `--record` appends it.

## Continuous integration

`.github/workflows/sim.yml` has three lanes. Each runs the corpus first (the realistic lanes its realistic lines,
the stress lane every line, each line in the tier it records), then fresh configurations that start at the
workflow run id, so each run explores configurations no earlier run has seen.

| Lane | When | Fresh runs | Shards | A failed run |
|---|---|---|---|---|
| `swarm` | Every pull request | 200 configurations with 3 seeds each, realistic tier | 4 | Fails the check |
| `nightly realistic` | Every night (02:41 UTC) | 3,400 configurations with 3 seeds each (10,200 runs), realistic tier | 16 | Is filed and fails the workflow |
| `nightly stress` | Every night (02:41 UTC) | 5,000 configurations with 1 seed each, stress tier | 4 | Is filed and does not fail the workflow |

With 3,400 configurations, any 9 chosen dimensions are all on in at least one of them with probability 99.87%,
and any 10 with 96.4%.

Each nightly shard writes its run log with `--out` and uploads it as the artifact `sim-log-<lane>-<shard>`. A
stress shard passes when the runner exits with 1 (some runs failed); any other exit code fails it. Then the job
`nightly report` (`tools/sim-nightly/report.py`):

- writes its job summary: for each lane the shard logs, the runs, the fresh runs, the passes, the failures and
  the expected losses, then every failed run with its invariant, tier, replay command and corpus line, under one
  heading per tier. A run keeps its own tier, so a realistic corpus line that fails in the stress lane is a
  realistic-tier failure. A shard without a log is listed too: one of its runs did not finish before the job
  timeout, or the job failed before its runs;
- uploads every failure as the artifact `sim-failures` (the summary and the issue list as many as their size
  limits hold);
- when a run failed, opens the issue `nightly simulation: <date>` (the UTC date, label `sim-nightly`) or, when
  that issue is open already, comments with the failures it does not list yet;
- fails when a realistic-tier run failed, when a shard wrote no log, or when a lane ran fewer fresh runs than its
  budget. Stress-tier failures never fail it.

By hand, any lane runs on any ref: `gh workflow run sim.yml --ref <branch> -f lane=<lane>`, with `pull-request`
(the default), `nightly` (both nightly lanes and the report, as the schedule runs them), `nightly-realistic` or
`nightly-stress`. A nightly lane run by hand files to the same issue, which names the ref and commit of each run.

Measured in the dev container on 3 CPUs with a debug build: the 600 runs of the pull-request budget take about
70 s, so one shard of 150 runs and the corpus stays far inside the 10 minute target, most of which is the build.
Measured on GitHub-hosted runners with a debug build (run 44408176506, both nightly lanes): a realistic shard of
638 or 639 runs took 2.6 to 4.7 minutes (the runs 117 to 226 s, the build 14 to 122 s), inside its 30 minute
timeout, and a stress shard of 1,255 or 1,256 runs took 6.5 to 10.6 minutes (the runs 242 to 548 s), inside its
150 minutes.

## Triaging a nightly failure

The issue, or the summary of the run, gives each failure's invariant, tier, replay command and corpus line, and
the commit the run used. A replay is bit-exact on the same commit with the same lockfile.

1. Replay it in the dev container, on that commit and then on `origin/main`. The replay command prints the
   violations, the dimensions and the statistics of the run. On the same commit it must give the same outcome and
   trace hash; if it does not, the defect is nondeterminism (I14).
2. If it fails on `origin/main`, fix it in a pull request that also appends the corpus line to
   `sim/corpus/seeds.jsonl`, with the note changed to `failed: <what it found>`, so that every later pull request
   runs it. Close the issue when that pull request merges.
3. If it passes on `origin/main`, a later commit fixed it or moved it: a change to the workload, the fault model or
   the product changes what a seed does. Replaying on the commits in between tells which.
4. For a shard without a log, rerun its failed job. The run id fixes the runs of the shard, so another timeout
   means a run that does not finish; a pass means the runner failed.
5. A stress-tier failure takes the same steps. The stress tier is outside the realism envelope, so its failures
   are filed and go into the corpus, but they block nothing.
