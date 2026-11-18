# System simulation

How the whole node of milestone 1 is verified in `crucible-sim` with deterministic simulation and swarm testing,
and how to replay a failing run. The method is in [`methodology.md`](methodology.md); the invariants are in
[`invariants.md`](invariants.md); the pieces this suite builds on are in [`wal-simulation.md`](wal-simulation.md)
and [`shipper-simulation.md`](shipper-simulation.md).

## The model

One simulated node, on one thread with a paused virtual clock and seeded random streams
(`crucible_sim::system_sim`):

| Piece | Real or model | What runs |
|---|---|---|
| Clients | Model | Seeded OTLP clients that build protobuf and JSON bodies and call the ingest router in process |
| Ingest | Real | `crucible_ingest::Ingest::router()`: authentication, admission, the scan, the WAL append, the ack |
| WAL | Real | `crucible-wal` on `SimDisk`, the disk fault model of contract 2.4 |
| Shipper | Real | `crucible-shipper`: stage A and B, seals, inserts, probes, quarantine, checkpoints, garbage collection |
| Recent-span index | Real | `crucible-recent`, fed by stage B through `crucible_shipper::RecentHook` |
| Query service | Real | `crucible_query::QueryService`: trace by id and the trace list |
| ClickHouse | Model | `ChModel`: inserts, deduplication, probes, the typed-plan interpreter, merges, and seeded faults |
| Keys | Real | `crucible_auth::StaticKeys`, one secret key per project |

### The boundary

The ClickHouse adapter (`crucible-store-clickhouse`) is outside the simulation: the `clickhouse` crate spawns
tasks, reads time and draws randomness on its own. `ChModel` stands in for it behind `TelemetryStore`. What keeps
the model honest is the conformance suite: `crucible-conformance` runs the same typed-plan reads against `ChModel`
and against real ClickHouse (26.3, 26.8 and the latest release) and compares the rows. The model may be stricter
than a server and is never more permissive.

Also outside: the network (requests enter at the router, so HTTP framing and TLS are not simulated), Postgres and
the control plane (later milestones), and several nodes (milestone 1 is one node).

The index never re-reads a payload from the WAL in the query services of milestone 1, so the simulation's
`WalView` answers the durable LSN and refuses payload reads.

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
| Workload | `json`, `large_payloads`, `deep_nesting`, `bad_unicode`, `rootless`, `concurrent_reads`, `client_retries`, `multi_project`, `bursts`, `multi_day` |
| Faults | `disk_crash_points`, `barrier_crashes`, `crash_loops`, `crash_in_recovery`, `clean_stops`, `disk_write_errors`, `disk_read_faults`, `disk_sync_errors`, `disk_full`, `slow_sync`, `ch_timeouts`, `ch_ack_lost`, `ch_server_errors`, `ch_rejects`, `ch_hang_overload`, `ch_poison`, `ch_probe_errors`, `ch_read_errors`, `window_expiry`, `outages`, `long_outages`, `wall_clock_steps`, `fsync_lie` (stress tier only) |
| Config | `wal_compression_off`, `small_batches`, `week_partitions`, `tiny_recent`, `small_segments`, `clustered`, `one_connection`, `replay_retention` |

That is 41 coins. With 200 configurations, any 6 chosen dimensions are all on in at least one of them with
probability 95.7% (`1 - (1 - 2^-6)^200`).

## Tiers

| Tier | What it adds | What a violation means |
|---|---|---|
| `realistic` | Faults at rates and lengths a deployment can meet. `fsync_lie` has no effect | A failure. It blocks the merge |
| `stress` | Fault rates times three, twice the incarnations, store outages up to 10 minutes, and with `fsync_lie` a disk that reports syncs that did not happen | Without the lie, a failure. With the lie, acknowledged data can be lost by construction: the run is recorded as `loss-expected`, and only a run that does not finish fails |

## Invariants

| Invariant | Continuous (after every ack and at every restart) | Quiescence |
|---|---|---|
| I1 | At every restart: every span of every acknowledged request is in `span_core` and `span_payload`, or in a durable frame above the recovered watermark, or in a quarantine copy | The same after the drain, and the watermark has passed the last frame that warrants a seal |
| I2 | The model refuses a malformed block and records a second block of one (batch, table, partition); no (table, LSN, span) row twice; no span with more rows than requests that carried it; every row belongs to a request that was sent; no two batch identities over the same frames; a pair the store had accepted is quarantined only after a probe that read absent | In runs without repeats and quarantines: every root span has its head row and the `trace_stats` span counts sum to the `span_core` rows |
| I3 | A trace-by-id read through the query service after every ack, and from concurrent readers for requests of any incarnation. A response that omits an acknowledged span must carry `partial` or `consistency: lagging`. `not_found` for an acknowledged trace is a violation. A marker must be warranted by the configuration: `store_unavailable` only with `outages`, `long_outages`, `ch_read_errors` or `tiny_recent`; `quarantined` only with `ch_poison` | Every acknowledged request is read back, and the whole trace list of every project must hold every acknowledged trace that has a root span. No marker is accepted, except `quarantined` when a quarantine directory exists. Repeated after the last restart |
| I6 | No trace-by-id response holds a span the project did not send, and no list holds a trace of another project (with `multi_project` the projects share trace ids) | The same |
| I12 | The recovered log equals the uninterrupted reference recovery although crashes cut the real one | A restart after the drain sends no insert and leaves the store and the watermark as they were |
| I13 | | No segment wholly at or below the watermark is left, and the index is empty once the grace has passed |
| I14 | | The trace hash covers every event of the run, read results included. The same seed, mask and tier give the same hash and outcome (`system_dst.rs`, on every pull request) |

Because a marker excuses a missing span, the suite also requires that most reads need none: over the smoke
budget more than half of the trace reads return every span, whole and strong.

## Mutants

Each defect is behind a `test-mutants` cargo feature that only `crucible-sim` enables. Each must be caught inside
the per-pull-request budget (200 configurations from configuration seed 0, 3 seeds each), and its first catching
run is recorded in `crates/crucible-sim/tests/system_dst.rs`. The same run must pass without the defect.

MUTANT-TABLE

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
cargo xtask sim replay --seed 17002 --config 0x1e574beef1d
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

`.github/workflows/sim.yml`:

- every pull request runs the corpus and 200 fresh configurations with 3 seeds each in the realistic tier, split
  over 4 shards. The fresh configurations start at the workflow run id;
- a nightly job runs 5,000 fresh configurations in the stress tier, split the same way.

TIMING
