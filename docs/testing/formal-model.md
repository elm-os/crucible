# Formal model of the ingest protocol

`specs/ingest.qnt` is a [Quint](https://quint-lang.org) model of the path from an OTLP request to a row in
ClickHouse: WAL group commit and the ack rule, sealed batches, tokens, the existence probe, quarantine, the
checkpoint low watermark, segment GC, crash and recovery, and the recent-span index. It is the "Proof (Quint)"
row of invariants I1, I2, I3 and I13 in [`invariants.md`](invariants.md), and it sits beside the methods in
[`methodology.md`](methodology.md). The contract it models is
[`../architecture/contracts.md`](../architecture/contracts.md), sections 4, 5 and 8.6.

The model checks the protocol, not the code. Simulation (`crucible-sim`) and conformance check that the crates
follow the protocol and that ClickHouse behaves as the model assumes.

## What is modelled

One node with one `wal_epoch`, one ClickHouse store, and a client.

| Part | In the model |
|---|---|
| WAL | Frames by LSN (`IngestRequest`, `BatchSeal`, `Checkpoint`, `Quarantine`), the last appended LSN, the durable LSN, fixed-size segments. Every frame consumes one LSN |
| Ack rule | A waiter is acknowledged only when its LSN is at or below the durable LSN |
| Batches | Adjacent LSN ranges with at least one `IngestRequest` frame. The boundary is chosen freely, appended as a `BatchSeal`, and must be durable before any insert |
| Pairs and tokens | Each span yields one row per table in partition `span % 2`. A (batch, table, partition) pair is sent as one insert whose token is the pair |
| ClickHouse | A set of landed blocks, each with its rows; a bounded deduplication log per table; inserts in flight that outlive a crash of the node; applied-but-ack-lost outcomes; rejections; the probe, which looks up `(batch_seq, table, partition)` and never the token |
| Shipper state | Per pair: new, sent, sent after doubt, unknown, probed absent, rejected, quarantined, confirmed. Lost by a crash |
| Quarantine | After `POISON_LIMIT` data errors the pair is copied to the quarantine store and a `Quarantine` frame is appended; the pair is resolved when that frame is durable |
| Checkpoint | Appended for the oldest open batch when its seal is durable and every pair is resolved. The watermark is the highest durable `Checkpoint` frame |
| Segment GC | The oldest segment is removed when it is full, durable, wholly at or below the watermark and not referenced by the recent index |
| Crash | Keeps every synced frame and any prefix of the unsynced tail; clears process memory; leaves inserts in flight |
| Recovery | Reopens sealed batches above the watermark with their recorded boundaries and every pair unknown (probe first), keeps pairs named by a durable `Quarantine` frame resolved, rebuilds the recent index from the frames above the watermark |
| Recent index | Entries by LSN, stage-B progress, the size bound with the gap rule, eviction at or below the watermark |

## What is abstracted away

| Left out | Why it is safe to leave out, or where it is checked instead |
|---|---|
| Frame bytes, CRCs, torn-frame detection, compression | Kani proofs and fuzzing of `crucible-wal-codec`; in the model a crash leaves a clean prefix |
| `FactBatch`, `Tombstone`, `Noop` frames | They take the same append, sync, ack and batch path as `IngestRequest` |
| Failed `sync_data`, rotation after a failure, LSN rewind inside one process | A failed group is never acknowledged; the model covers the outcome through a crash that drops the unsynced tail |
| Time: linger, backoff, replay retention, visibility grace, `max_execution_time`, the read barrier's 250 ms | Replaced by nondeterministic ordering, which covers every timing. Replay retention and the grace only delay GC and eviction |
| The duplicate window for client retries | Rows are keyed by frame LSN, so a retried span is another row. Span-level duplicate suppression is checked by simulation (I2) |
| Normalizer and price-table versions, block bytes, marker rows, row order | The token, not the content, decides deduplication (conformance C4) |
| Oldest-first eviction of the deduplication log | The model lets any entry expire, which allows more behaviours than the store has |
| The fatal-configuration error class, `crucible wal requeue`, quarantine retention | The shipper stops or an operator acts; neither sends an insert without the probe |
| Recovery as several steps, the `CHECKPOINT` hint file | Recovery in the model is one step that reads only durable state, so a crash inside it changes nothing. Idempotence of the real recovery (I12) is checked by simulation and fuzzing |
| Admission, backpressure, byte budgets | I5, checked by Kani and simulation |
| More than one node, replicas, `insert_quorum`, peer fan-out for reads | Out of scope; conformance C11 covers replica lag |
| The unsafe durability modes `interval` and `none` | They are the `ack-before-sync` mutant |

Two assumptions about ClickHouse are stated in the model as the `evictable` predicate, and both rest on
conformance rows C1 to C3: a token stays in the deduplication log while a copy of its insert is in flight, and
while the shipper holds a probe-absent decision for its pair. The `assumption_short_window` instance lifts them
and the model then finds a duplicate, so they are load-bearing.

## Actions and the code that implements them

| Action | What it models | Crates | Contract |
|---|---|---|---|
| `Receive` | A request is admitted | `crucible-ingest`, `crucible-otlp`, `crucible-admission` | 4.8 |
| `ClientRetry` | A client resends a span whose ack it did not see | SDKs; absorbed by `crucible-normalize` | 4.8, 5.3 |
| `Append` | The committer assigns the LSN and appends | `crucible-wal`, `crucible-wal-codec` | 4.3, 4.5 |
| `Sync` | `Disk::sync_data` returns `Ok` for the group | `crucible-wal`, `crucible-env` | 4.5 |
| `Ack` | The waiter completes and the response is sent | `crucible-wal`, `crucible-ingest` | 4.5 |
| `IndexApply`, `EvictRecent` | Stage B applies a frame; eviction; the gap rule | `crucible-recent` | 8.6 |
| `FormBatch` | The shipper picks the batch boundary | `crucible-shipper` | 5.1 |
| `PersistBatchBoundary` | The `BatchSeal` frame is appended | `crucible-shipper`, `crucible-wal` | 4.4, 5.1, 5.3 |
| `SendInsert` | One insert with one block and its token | `crucible-shipper`, `crucible-chfmt`, `crucible-records` (`BatchToken`), `crucible-store-clickhouse` | 5.2, 5.4 |
| `ChApply` | ClickHouse writes the block or deduplicates it | ClickHouse; `crucible-store-clickhouse`; the store model in `crucible-sim` | 5.2, 6.2 |
| `ChAckDeliver` | The success response arrives | `crucible-store-clickhouse`, `crucible-shipper` | 5.6 |
| `ChAckLost` | Timeout or reset: outcome unknown | `crucible-store-clickhouse`, `crucible-shipper` | 5.6 |
| `ChReject` | A retryable or a data error | `crucible-store-clickhouse` (code table), `crucible-shipper` | 5.6 |
| `QuarantinePair` | Copy, then the `Quarantine` frame | `crucible-shipper`, `crucible-wal` | 5.6 |
| `WindowExpire` | A deduplication log entry expires | ClickHouse table settings in `crucible-schema` | 6.2 |
| `ExistenceProbe` | The `BatchProbe` plan | `crucible-shipper`, `crucible-plan`, `crucible-store-clickhouse` | 5.5 |
| `AdvanceCheckpoint` | The `Checkpoint` frame | `crucible-shipper`, `crucible-wal` | 5.7 |
| `TruncateWal` | Segment GC | `crucible-wal`, `crucible-recent` (references) | 4.7 |
| `Crash` | Any crash barrier | `crucible-sim` (barriers), every crate above | 5.9 |
| `Recover` | WAL scan, reopen sealed batches, rebuild the index | `crucible-wal`, `crucible-shipper`, `crucible-recent` | 4.6 |

## Properties

Safety, as state invariants (`Safety` is their conjunction):

| Property | Statement | Invariant |
|---|---|---|
| `AckOnlyAfterSync` | Every ack was issued for an LSN at or below the durable LSN | I1 |
| `NoAckedLoss` | Every acked span is in the durable WAL, or for every table in ClickHouse or in quarantine. It holds in every reachable state, so it holds across crash and recovery | I1 |
| `NoDuplicateRows` | No two landed blocks share a (batch, table, partition), and no row is in two blocks of one table. The second half catches a row that lands under two batch identities | I2 |
| `CheckpointMonotonic` | No step lowers the watermark | I2 |
| `TruncateOnlyBelowCheckpoint` | Nothing above the watermark was removed from the WAL | I1, I13 |
| `BatchIdentityStable` | Every batch identity that was ever sent and every durable seal are pairwise equal or disjoint, across crash and recovery. `invariants.md` calls it `BatchIdentityStableAcrossCrash` | I2 |
| `ReadYourAck` | Once stage B has applied through an ack's LSN (what the read barrier waits for), the span is in the recent index or in ClickHouse, or it lies in an open index gap, or one of its pairs is quarantined | I3 |

Liveness, as temporal properties:

| Property | Statement | Invariant |
|---|---|---|
| `AckedEventuallyLands` | Every acked span is eventually, for every table, in ClickHouse or in quarantine | I1 |
| `CoveredEventuallyTruncated` | Every full segment at or below the watermark is eventually removed | I13 |

Both are stated under the `Fairness` assumption, which has two parts. Faults are finite: crashes, lost insert
acks and rejections share the budget `MAX_FAULTS`, and client sends are bounded by `MAX_FRAMES`. Every step that
makes progress is weakly fair (if it stays enabled it eventually happens): `Sync`, `IndexApply`, `EvictRecent`,
`FormBatch`, `PersistBatchBoundary`, `SendInsert`, `ChApply`, `ChAckDeliver`, `QuarantinePair`,
`ExistenceProbe`, `AdvanceCheckpoint`, `TruncateWal`, `Recover` and `WindowExpire`. Client steps and faults
carry no fairness. Without the assumption `AckedEventuallyLands` fails, and each property fails under a
reduced fairness set (for example, truncation without fair index eviction); this was checked once by hand.

`NoStuckState` is a state invariant that supports liveness on instances too large for the temporal check: while
the node is up and an acked span is not yet stored or a covered segment is not yet removed, some progress step
is enabled.

## Instances

| Module | Spans | Tables | Frames | Faults | Window | Segment | Index cap | Poison limit | Used by |
|---|---|---|---|---|---|---|---|---|---|
| `ingest_ok`, `mutant_*`, `assumption_short_window` | 3 | 2 | 4 | 3 | 2 | 2 | 3 | 1 | Random simulation |
| `ingest_small` | 2 | 1 | 2 | 2 | 1 | 2 | 2 | 1 | Apalache, bounded; simulation of `ReadYourAckStrict` |
| `ingest_tiny`, `reading_retry_without_reprobe` | 1 | 1 | 1 | 3 | 1 | 1 | 1 | 1 | TLC, every reachable state |

"Frames" bounds the `IngestRequest` frames appended (first sends and client retries); control frames are not
bounded. "Faults" bounds crashes, lost insert acks and rejections together.

## Running it

Everything runs through `specs/check.sh` in the formal dev container (`deploy/dev/formal.Dockerfile`, Quint
0.33.1). The volume keeps Quint's evaluator and the Apalache distribution between runs. On Windows with Git
Bash, prefix the command with `MSYS_NO_PATHCONV=1`.

```sh
docker run --rm --cpus=2 --memory=3g -v "$PWD":/work -w /work -v crucible-quint:/root/.quint \
  crucible-dev-formal:1 sh specs/check.sh all
```

| Mode | What runs | Passes when |
|---|---|---|
| `typecheck` | `quint typecheck` | No error |
| `simulate` | `quint run` (Rust backend) on `ingest_ok`: 10,000 samples of 60 steps, the seven safety invariants and `NoStuckState` | No violation, and every witness (checkpoint, truncate, quarantine, probe present, probe absent, deduplicated insert, window expiry, re-ship after recovery, index gap) is reached in at least one trace |
| `mutants` | `quint run` on every row of the table below, 10,000 samples, checking only the named property | A violation is found |
| `seeds` | `quint run --seed` with the recorded seed of every row | The violation reproduces |
| `verify` | `quint verify` (Apalache) on `ingest_small`, `Safety`, 6 steps; and on `mutant_ack_before_sync` | No violation; a violation for the mutant |
| `exhaustive` | `quint verify --backend tlc` on `ingest_tiny`: `Safety` and `NoStuckState`, then both liveness properties; and on `reading_retry_without_reprobe` | No violation in the complete state space; a violation for the narrow reading |

`SAMPLES`, `STEPS`, `THREADS` and `VERIFY_STEPS` override the defaults. To see a counterexample, run one case
directly with more output:

```sh
quint run specs/ingest.qnt --main mutant_drop_token --invariant NoDuplicateRows --max-steps 60 \
  --seed 0xfc3ff7b7f569e714 --verbosity 3
```

The `lastAction` variable in each state names the step that produced it.

CI runs the same modes in `.github/workflows/quint.yml` on every change under `specs/` and once a week.

## Mutants

Each mutant breaks one rule. The run checks only the named property, so the violation is of that property.
Seeds are for Quint 0.33.1 with the Rust backend and 60 steps; they must be recorded again when the model
changes (`specs/check.sh seeds` fails when one goes stale).

| Module | Broken rule | Violated property | Seed |
|---|---|---|---|
| `mutant_ack_before_sync` | The ack is sent after the append, before `sync_data` | `AckOnlyAfterSync` | `0x6e0e16290c7a6342` |
| `mutant_send_before_seal` | An insert is sent before the `BatchSeal` is durable | `BatchIdentityStable` | `0x5e222faea438aabf` |
| `mutant_drop_token` | Every attempt carries a new deduplication identity | `NoDuplicateRows` | `0xfc3ff7b7f569e714` |
| `mutant_truncate_early` | Segment GC ignores the checkpoint watermark | `TruncateOnlyBelowCheckpoint` | `0x2aaa4c6f109fee59` |
| `mutant_no_probe` | A retry after an unknown outcome skips the probe (and the window has expired) | `NoDuplicateRows` | `0x9c79520373fc318d` |

How each one fails:

- **ack-before-sync**: receive, append, ack. The acked LSN is above the durable LSN; a crash then loses the
  frame, which also violates `NoAckedLoss`.
- **send-before-seal**: a batch is sent, the node crashes before the seal is durable, and recovery seals the
  same frames with another boundary. Two overlapping identities have been on the wire. The mutant also violates
  `NoDuplicateRows` (the shared rows land twice) and `NoAckedLoss`.
- **drop-token**: the first attempt times out while still in flight, the probe reads absent, the retry goes
  out under a new identity, and both land. This is the race the token exists for.
- **truncate-early**: a full, durable segment is removed while its frames are above the watermark. The index
  reference does not protect it: stage B may not have indexed the frames yet, or the gap rule dropped them. The
  acked spans then exist nowhere, which also violates `NoAckedLoss`.
- **no-probe-after-window-expiry**: the insert lands, the ack is lost or the node crashes, the log entry
  expires, and the blind retry lands again.

Three more rows are not protocol mutants. They record what the model found about the contract:

| Module | What is changed | Violated property | Seed or tool |
|---|---|---|---|
| `assumption_short_window` | A log entry may expire while an attempt of its pair is under way | `NoDuplicateRows` | `0x9626299bcef9648e` |
| `ingest_small` | Nothing; `ReadYourAckStrict` is I3 without the quarantine exception | `ReadYourAckStrict` | `0x3c65ec97ca7ea887` |
| `reading_retry_without_reprobe` | After a rejection the shipper resends without a probe even when an earlier attempt of the pair had an unknown outcome | `NoDuplicateRows` | TLC; the shortest trace has 16 states |

## Findings

1. **Probe again after a rejection that follows an unknown outcome.** Section 5.5 requires the probe "before
   every retry whose previous outcome is unknown". Read narrowly, this permits: attempt 1 times out while in
   flight; the probe reads absent; attempt 2 is sent; attempt 1 lands; attempt 2 is rejected with a retryable
   error (a known outcome); the log entry expires during the backoff; attempt 3 is sent without a probe and
   lands again. The model therefore takes the wider reading: once an attempt of a pair has had an unknown
   outcome, a later rejection puts the pair back in the unknown state, and the probe runs before the next send.
   Random simulation did not find this trace in 10,000 samples; TLC did.
2. **The window must outlast a probe-absent decision.** Between a probe that reads absent and the resend, the
   first attempt can land and its log entry can expire. Section 6.2 says the window "only has to cover the
   in-flight race"; the race includes the time from the probe to the resend.
3. **Read-your-ack and quarantine.** A span whose pair is quarantined is resolved for the watermark, so its
   index entry can be evicted, but it is not in ClickHouse until an operator requeues it. I3 lists only the
   gap and lagging exceptions. `ReadYourAck` in the model adds the quarantine exception; `ReadYourAckStrict`
   is the literal statement and fails.

## Limits

- The bounds are small. Random simulation samples the larger instance; Apalache covers `ingest_small` to 6
  steps, which reaches an ack but not a checkpoint; TLC covers every reachable state of `ingest_tiny` (one
  span, one frame, three faults; 71,595 distinct states).
- `ingest_small` is too large to finish under TLC in the dev container. One run with a 25 minute limit covered
  31.8 million distinct states and reached search depth 25 without a violation of `Safety`. It is not part of
  `check.sh`.
- Liveness is model-checked only on `ingest_tiny`. On larger instances it is supported by `NoStuckState`,
  which is weaker than a temporal proof.
- The model says nothing about whether the Rust crates implement these actions. That link is the job of
  simulation, which checks the same properties as continuous and quiescence invariants.
