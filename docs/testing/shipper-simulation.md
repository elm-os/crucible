# Shipper simulation

How `crucible-shipper` is verified in `crucible-sim`, and how to replay a failing seed. The method is in
[`methodology.md`](methodology.md); the invariants are in [`invariants.md`](invariants.md); the protocol model the
shipper follows is in [`formal-model.md`](formal-model.md); the disk and the WAL-only suite are in
[`wal-simulation.md`](wal-simulation.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Store model | `crucible_sim::ch_model::ChModel` | The simulated ClickHouse behind `TelemetryStore`. This suite uses its insert, deduplication and probe side |
| Workload and oracle | `crucible_sim::ship_sim` | The WAL suite's plan (segment sizes, disk faults, crash loops, process stops, crashes during recovery), clients that append OTLP requests as `IngestRequest` frames, the real shipper, seeded crash points, and the ledger that checks I1, I2, I12, I13 and I14 |
| Recovery driver | `crucible_sim::boot` (crate-private) | Recovers the WAL through crashes and faults and compares it with an uninterrupted reference recovery. Both suites use it |
| Suite | `crates/crucible-sim/tests/ship_dst.rs` | The per-pull-request seeds, the mutants, the determinism test, the long run and the replay tool |
| Directed tests | `crates/crucible-sim/tests/ship_directed.rs` | One scripted situation each, with the calls the store received checked in order; and a crash at every disk operation of the two quarantine copies, after which every whole manifest must stand beside the copy it declares |
| Pipeline tests | `crates/crucible-sim/tests/ship_pipeline.rs` | The shipper's blocking jobs (stage A of the frames it reads ahead, the encoding of each block) out of order: a spawner keeps them and runs them later, in a seeded order and after a draw of virtual time each. A property over random logs of requests, deletions and facts: the shipper inserts the same pairs with the same bytes as when the jobs run in place, and every attempt of a pair sends one body. The property also draws deletion requests that fail their first reads, so the shipper drops the frames it read ahead and reads them again, and a restart: a first node ships part of the log and crashes, mid-batch or after its checkpoints, and the second rebuilds stage B from the retained prefix with stage A ahead. Directed tests check the read-ahead depth, the rebuild's repeats, a batch that waits to seal while the one before it is encoded, a shipper that keeps up with the log running stage A on its task, and the fallback to the task while ingest is busy |

The simulation's spawner runs blocking jobs in place, so in this suite and in the system simulation the shipper
reads one frame at a time and encodes at the seal, as its task did before the jobs moved off it, and a batch is
encoded when it seals. The recorded mutant seeds, the system simulation's recorded runs and the corpus are those
of the shipper that encoded on its task. The pipeline tests are where the jobs end out of order.

The WAL suite keeps its stand-in shipper (seal, checkpoint, collect). It checks `crucible-wal` alone, with opaque
frames and incarnations of a few milliseconds, and its recorded mutant seeds do not depend on the shipper. The
shipper suite runs the same plan and the same disk through the real shipper.

## What `ChModel` models

- Tables are the production `TableDef`s. A block is decoded from its `EncodedBlock` with the Native decoder of
  `crucible-chfmt` and checked: column names and header types, at least one row and at most 262,144, every row in
  the partition the token names (through `crucible_schema::route`), the ORDER BY sort, the batch identity of every
  row of a plain table (`batch_seq` equals the token's first LSN, `lsn` inside the token's range), and on an
  aggregating table exactly one marker row and zero marker columns on data rows. A block that fails is refused and
  recorded as a violation.
- Parts are kept per table with their partition. A second part with the batch identity of a landed part in the
  same partition is a violation of I2.
- A probe is answered by scanning the rows of the landed parts (plain tables) or their marker rows (aggregating
  tables). It never reads the token.
- Each table has a deduplication log bounded by the count window of the DDL (20,000) and, for the replicated
  flavours, by the age window (7,200 s).
- Window expiry is also a fault: an entry may expire early, per store call, unless a copy of its insert is in
  flight, or a probe of its pair read absent less than `probe_valid_for` ago. These are the two assumptions the
  ingest model states as its `evictable` predicate.
- An insert in flight belongs to the store, not to the node: it lands at its time whether the caller timed out,
  dropped the call or crashed.

| Outcome fault | What the caller sees | What the store does |
|---|---|---|
| Timeout before apply, server error | Unknown outcome | Nothing lands |
| Timeout in flight | Unknown outcome | The block lands after a seeded delay (up to 120 s) |
| Applied, ack lost | Unknown outcome | The block lands at once |
| Hang | No answer; the shipper's timeout fires | As the unknown outcome it replaces |
| Too many parts, memory, too many queries | Retryable rejection | Nothing lands |
| Overload | After a timeout in flight, every insert is shed with code 252 for up to 90 s; probes still answer | Nothing lands |
| Outage | Inserts are not sent (or time out); probes fail | Inserts in flight still land |
| Poison pair | A data error on every attempt, except that the first attempt may be accepted and stay in flight | As stated |
| Probe error | The probe fails | Nothing |

The read side of the model (the typed-plan interpreter, merges, deletes and partition drops) is used by the
system simulation, [`system-simulation.md`](system-simulation.md), and checked against real servers by the
conformance suite. The raw landed parts that this suite inspects are unaffected by merges.

## Crash points

An incarnation ends at the first of: a seeded number of disk operations, a seeded number of crash barriers of the
shipper (any barrier, or one chosen barrier so that the rare ones get their share), a seeded virtual time (up to
150 s), or a process stop. When the chosen barrier is the rarest one, the quarantine copy, the virtual time is drawn
up to 150 s in every incarnation, short ones included, because the copy needs a pair that three attempts and their
backoffs failed first. The barriers are those of contract 5.9. The
shipper awaits a hook at each one (`crucible_shipper::Barriers`); the simulation's hook freezes the disk and never
completes at the barrier it picked, and the driver then aborts the host. Short incarnations (a few disk operations)
put the crash inside the shipper's own recovery: the rebuild of stage B and the re-ship of sealed batches.

`Stats::crashes_at` counts the crashes per barrier, and the suite fails unless the per-pull-request seeds crash at
every one of them: before and after the seal append, before and after an insert, after the quarantine copy, and
before and after the checkpoint append. It also fails unless the seeds reach a restart that ships sealed batches
again, a probe that reads present, a probe whose answer expires before its send, a deduplicated insert, an insert
that lands late, a window expiry and a quarantine.

## What the oracle checks

| Invariant | Check |
|---|---|
| I1 | At every restart: every accepted span of every acknowledged request at or below the recovered watermark has its row in `span_core` and in `span_payload`, or its pair is quarantined. At quiescence: the same for every acknowledged request, and the watermark has passed the last frame that warrants a seal |
| I2 | No (table, LSN, span) row lands twice. No two blocks of one (batch, table, partition) land, and every aggregating block has exactly one marker (the model checks both on every insert). No two different batch identities over the same frames were ever sent. Every row belongs to a submitted request. A pair of which the store had accepted a block is quarantined only after a probe that read absent. At quiescence, in runs without repeats and quarantines: every root span has its head row and the `trace_stats` span counts sum to the `span_core` rows |
| I12 | The recovered log equals the uninterrupted reference recovery although crashes cut the real one. After quiescence a restart sends no insert and leaves the store and the watermark as they were |
| I13 | At quiescence no segment wholly at or below the watermark is left |
| I14 | The same seed gives the same outcome and trace hash |

A request that repeats an earlier one may be dropped by the duplicate window, which a restart rebuilds from the
retained prefix only. For such a span the oracle asks for its key at least once, or for a quarantine copy that
holds it, instead of a row at its own LSN.

A quarantine directory counts as a quarantine when its batch is at or below the watermark and the store's last
answer about the pair was an absent probe, or a data error for a pair it never accepted. A directory whose pair
landed after all is what a crash between the copy and the `Quarantine` frame leaves behind. The copy of a counted
quarantine must hold exactly the frames of its batch. Here every restart ships each pair of a sealed batch again,
so the last answer tells the two apart. In the system simulation a restart that knows more deletion requests can
leave a pair without rows, so it counts only the directories that a `Quarantine` frame names (see "Deletions" in
[`system-simulation.md`](system-simulation.md)).

## Mutants

`crucible-shipper` has a cargo feature `test-mutants` that only `crucible-sim` enables; without it the `Mutant`
type has no value. Each mutant must be caught inside the per-pull-request budget of 500 seeds through the
invariant it breaks, and the first seed that does so is recorded in `ship_dst.rs`.

| Mutant | Defect | Caught as | Seed |
|---|---|---|---|
| `SkipProbeAfterUnknown` | A retry after an unknown outcome skips the probe | A block lands twice after its window entry expired | 4 |
| `SendBeforeSeal` | Inserts are sent before the `BatchSeal` frame is durable | Two batch identities over the same frames | 31 |
| `DropToken` | Every attempt carries a new deduplication identity | A block lands twice | 1 |
| `CheckpointBeforeResolution` | The checkpoint is appended before the pairs are resolved | An acknowledged span below the watermark is nowhere | 0 |
| `QuarantineWithoutProbe` | A pair in doubt is quarantined without a probe | A quarantined pair whose block the store had accepted | 4 |
| `RejectionClearsDoubt` | A rejection after an unknown outcome clears the doubt (the model's finding, reverted) | A block lands twice: the first attempt lands late, the store sheds the retries until the probe's answer has expired, and the blind resend lands again | 44 |

A change to the workload, the fault model or the product moves the recorded seeds. The failing assertion prints
the new first catching seed; `mutant_census` lists, per mutant, how many seeds show each kind of evidence.

## Commands

```sh
# The per-pull-request budget (runs as part of the workspace test run)
cargo nextest run -p crucible-sim

# Replay one seed and print its outcome
CRUCIBLE_SIM_SEED=204 cargo nextest run -p crucible-sim --test ship_dst replay_seed --run-ignored all --no-capture
# ... with a mutant
CRUCIBLE_SIM_SEED=44 CRUCIBLE_SIM_MUTANT=rejection-clears-doubt cargo nextest run -p crucible-sim --test ship_dst replay_seed --run-ignored all --no-capture

# The long run
CRUCIBLE_SIM_SEEDS=20000 cargo nextest run -p crucible-sim --test ship_dst long_run --run-ignored all --no-capture

# The census of one mutant, or of all without the variable
CRUCIBLE_SIM_MUTANT=drop-token cargo nextest run -p crucible-sim --test ship_dst mutant_census --run-ignored all --no-capture
```

Mutant names: `skip-probe-after-unknown`, `send-before-seal`, `drop-token`, `checkpoint-before-resolution`,
`quarantine-without-probe`, `rejection-clears-doubt`.
