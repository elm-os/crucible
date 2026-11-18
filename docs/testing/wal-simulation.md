# WAL simulation

How `crucible-wal` is verified in `crucible-sim`, and how to replay a failing seed. The method is in
[`methodology.md`](methodology.md); the invariants are in [`invariants.md`](invariants.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Runtime | `crucible_sim::run`, `Sim`, `Host` | One thread, a paused virtual clock, a root ChaCha20 stream with labelled forks. A host is one process: `Host::crash` freezes its disk, aborts every task at its await point, waits until they are dropped, then applies the crash to the disk |
| Disk | `crucible_sim::SimDisk` | The fault model of contract section 2.4 |
| Workload and oracle | `crucible_sim::wal_sim` | Seeded clients, a stand-in shipper (seal, checkpoint, garbage collection), seeded crash points, and the ledger that checks I1, I12, I13 and I14 |
| Suite | `crates/crucible-sim/tests/wal_dst.rs` | Tiers, mutants, the determinism meta-test, the long run and the replay tool |
| Directed tests | `crates/crucible-sim/tests/wal_directed.rs` | Recovery, the failed-group path, garbage collection, admission signals, durability modes |
| Real filesystem | `crates/crucible-sim/tests/wal_real_fs.rs` | Smoke test and an ignored measurement over the production `PortableDisk` |

Disk-backed tests of `crucible-wal` live in `crucible-sim` because the crate dependency table of the contract
gives `crucible-wal` no edge to a disk implementation.

## What `SimDisk` models

- Each file has a current image and a durable image. Blocks of 4,096 bytes written since the last successful
  sync are dirty.
- A crash keeps, per file, none of the dirty blocks, all of them, or a seeded subset (a torn write). A pending
  change of length is kept or lost the same way.
- A failed sync drops the file's dirty state: those blocks are lost at the next crash, and a later sync does
  not rescue them unless they are written again. An fsync lie does the same and reports success.
- Directory entries created, renamed or removed since the last `sync_dir` are pending. A crash keeps none of
  them, all of them, a seeded prefix or a seeded subset.
- A write can fail after a seeded prefix was written (EIO, short write). A read can fail, or return one flipped
  bit. A sync can fail, lie, or take seeded virtual time. A capacity budget yields ENOSPC.
- Each fault type draws from its own forked stream.

Two limits of the model are deliberate:

- A flipped read is never followed by another flipped read of the same file. The simulation compresses the
  rate of this transient fault, not its correlation, so recovery may confirm a destructive decision with a
  second read.
- Creating a directory is durable at once.

## Tiers

| Tier | Faults | Expectation |
|---|---|---|
| Realistic | Everything above except the fsync lie | Every invariant holds for every seed |
| Stress | Realistic plus the fsync lie | The oracle reports lost acknowledged frames, which shows that the realistic tier's silence means something |

Each run flips a coin per workload feature and per fault (segment size, capacity, retention, compressible
payloads, uniform frame sizes, bursts, crash loops, crashes during recovery, double recovery, wall-clock steps,
the recent-index pin, and each fault type).

## What the oracle checks

| Invariant | Check |
|---|---|
| I1 | Every acknowledged frame above the recovered watermark is in the recovered log, intact, at its LSN. Frames an earlier recovery returned are still there. Every frame above the previous end of the log was submitted in the incarnation that just ended, once. No valid frame is left beyond the end of the recovered log. The recovered watermark is at or above every acknowledged checkpoint. Garbage collection removes nothing above every submitted checkpoint |
| I12 | The recovered state equals the state one uninterrupted recovery reaches on a fault-free copy of the crashed disk, although the real recovery is cut by crashes and faults. A second recovery with no append in between repairs nothing. Bytes scanned are at most the retained segment bytes |
| I13 | At quiescence (faults healed, everything checkpointed, retention elapsed) no segment wholly at or below the watermark is left |
| I14 | The same seed gives the same outcome and trace hash; different seeds give different hashes |

A fatal recovery error on a healed disk is a violation.

## Mutants

`crucible-wal` has a cargo feature `test-mutants` that only `crucible-sim` enables. Each mutant must be caught
inside the per-pull-request seed budget, and its first catching seed is recorded in `wal_dst.rs`.

| Mutant | Defect | Caught by |
|---|---|---|
| `AckBeforeSync` | Waiters complete before `sync_data` | A lost acknowledged frame after a crash |
| `GcAboveCheckpoint` | Garbage collection ignores the watermark | A removal above the watermark, then a `wal_gap` at recovery |
| `KeepTornTail` | Recovery does not zero the tail | A valid stale frame beyond the end of the log, or a frame of an earlier incarnation in the log |

A change to the workload, the fault model or the WAL moves the recorded seeds. The failing assertion prints the
new first catching seed; `mutant_census` lists all of them.

## Commands

```sh
# The per-pull-request budget (runs as part of the workspace test run)
cargo nextest run -p crucible-sim

# Replay one seed and print its outcome
CRUCIBLE_SIM_SEED=714 cargo nextest run -p crucible-sim --test wal_dst replay_seed --run-ignored all --no-capture
# ... in the stress tier, or with a mutant
CRUCIBLE_SIM_SEED=4 CRUCIBLE_SIM_TIER=stress cargo nextest run -p crucible-sim --test wal_dst replay_seed --run-ignored all --no-capture
CRUCIBLE_SIM_SEED=1 CRUCIBLE_SIM_MUTANT=gc-above-checkpoint cargo nextest run -p crucible-sim --test wal_dst replay_seed --run-ignored all --no-capture

# The long run
CRUCIBLE_SIM_SEEDS=100000 cargo nextest run -p crucible-sim --test wal_dst realistic_long_run --run-ignored all --no-capture

# Appends per second and durable-ack latency on PortableDisk
cargo nextest run -p crucible-sim --cargo-profile release --test wal_real_fs perf --run-ignored all --no-capture
```
