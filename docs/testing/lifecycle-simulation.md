# Lifecycle simulation

How retention and deletion (contract 13, invariant I9) are verified: the retention executor, the mask scheduler,
the deletion executor and the per-node timer of `crucible-lifecycle`, and the `Service` behind the API. The method
is in [`methodology.md`](methodology.md); the job runners below the executors are in
[`jobs-simulation.md`](jobs-simulation.md) and the store models in [`control-store.md`](control-store.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Executors | `crates/crucible-lifecycle` | The three leader jobs under the `lifecycle` lease, the node timer, the service |
| Unit and property tests | `crates/crucible-lifecycle/src`, `crates/crucible-plan/src/hidden.rs` | The drop-or-delete decision, the partitions of a span of time, the reach list as data, the predicates of a delete, the pending-deletion set, the predicate a pending deletion adds to a plan |
| Simulation | `crucible_sim::lifecycle_sim`, `crates/crucible-sim/tests/lifecycle_dst.rs` | The real jobs and the real service on several nodes over `ChModel` and `PgModel` under faults, with the oracle and the mutants |
| Directed scenarios | `crates/crucible-sim/tests/lifecycle_directed.rs` | One node with the real WAL and the real shipper: the tombstone is a durable frame that reads back as the request, the checkpoint passes it on a node that receives nothing else, and a deletion by trace runs to `completed` |
| Read services | `crates/crucible-query/tests/services/hidden.rs` | Trace detail, the trace list, the payload route, the session, user and span lists and the pages of an export in progress under a pending-deletion set, from the recent index and from the store |
| Project salts | `crates/crucible/src/salts.rs` | A deletion by user hashes the user with the stored salt as stage B did, never creates a salt, and says how to fix one that does not open |
| Differential, ClickHouse | The row "ChModel differential: retention and deletion" of `crucible-conformance` | The reads and maintenance calls the executors issue, on the model and on ClickHouse 26.3, 26.8 and latest, among them the issuers of the MCP operations, the MCP traces of a user and the delete of a user's operations with a full chunk of subject hashes |
| Differential, Postgres | The `lifecycle` script of `crucible-conformance` | Every statement of `LifecycleRepo` on `PgModel` and on Postgres 15 and 18 |
| Routes | `crates/crucible-api/tests/api/lifecycle.rs` | Roles and scopes, `If-Match`, idempotency and every refusal, over `PgModel` |

## What runs

Three nodes (five in one test) each start the real `Registry` with the node timer and the three leader jobs, and
hold the real `Service`. What the simulation models is a node's log and its stage B:

- a node's log is a list of frames that survives the node's crashes. A frame holds one to three spans of one
  trace, or a tombstone. `NodeLog::append_tombstone` appends a tombstone frame;
- a shipper task moves the frames to the store in log order: for a frame of spans it inserts one block into each
  of `span_core`, `span_payload`, `trace_heads`, `mcp_ops`, `mcp_content`, the four fact tables, `trace_stats`,
  `session_rollup` and `user_rollup`, then raises the checkpoint watermark to the frame;
- stage B keeps two rules. A tombstone applies to the frames after it in the log: their spans that the request
  selects and that were received before it are dropped, and for a deletion by user also the MCP rows that carry
  the user by `user_id` or subject hash, while the rest of the frame ships. A node that starts registers first
  (its heartbeat is in Postgres), then reads the deletion requests, and only then ships.

A workload appends a frame about once a second to a node that is up. Two projects get traces of five users and
seven sessions, on fresh days (today and yesterday) and on old days (six to nine days back). The first project's
old days reach further than the second's, so some old partitions hold one project and some hold both. Half the
traces show their user everywhere. The others show it only on their MCP rows, as `user_id` or only as the hash
of the user's subject under one of two issuers, or on their spans with the MCP rows hashed: a deletion by user
has to reach those MCP rows by their own columns, in partitions where no span names the user. Every 15 to 50 s a
caller asks a node for a deletion by trace, by user or by session, with an idempotency key, and sends the
request again when the call fails. Once in a run the first project's retention is shortened to three days.

The mask cycle is 30 min and the retention executor plans every 10 min, so that a run covers several cycles; the
other settings are the defaults.

## Faults

| Fault | How it is injected |
|---|---|
| Crash | Every task of the node is aborted. The node starts again over the same log up to 10 s later, and in one case of four 70 to 150 s later, which is longer than the 60 s its heartbeat counts as live |
| Shipper stall | A node's shipper stops for 30 to 90 s, so frames received before a request are still on their way when every node has logged it |
| Clock skew | Each node's wall clock is up to 2 s off the database's |
| Control store | The store fails over for up to 8 s, loses commits with an unknown outcome, aborts transactions and delays commits |
| Telemetry store | The store is unreachable for 2 to 20 s; 3 in 100 reads and maintenance calls fail |
| Project salts | One read of a project's salt in four fails, so a deletion by user waits and says why |
| Lease steal | The `lifecycle` lease row is rewritten for another holder under its leader, which then releases it or lets it expire |

The faulty phase lasts 15 min of virtual time. A fault-free tail of 90 min follows, which is three mask cycles, and
at its end every node is stopped.

## The oracle

| Invariant | Check |
|---|---|
| L1: every action on ClickHouse is recorded under the fencing token before it is issued | The leader reaches the store through a wrapper. At each delete, partition drop and mask application it looks for an action row of that kind, table and partition in the state `issued` with a fencing token |
| L2: a deletion is hidden at once on the accepting node | When `request_deletion` returns, the node's pending-deletion set holds the request, and the plan that reads exactly the spans the request selects returns nothing once it carries the set |
| L3: every request completes | At the end every deletion request is `completed` and every action is `verified` |
| I9: nothing a completed request selects is left | Every row the store still holds of the twelve tables, masked or not, is compared with every completed request: a span table by the request's own column, the MCP, fact and stats tables by the traces the request resolves to, the MCP tables for a deletion by user also by their own `user_id` and by the hash of the user under either issuer, the rollups by key. A row received at or after the request is kept; a table without an arrival time is exempt for a key that got data after the request |
| I9: nothing retention expired is left | No table with a retention binding holds a row of the shortened project that is more than five days old, masked or not |
| I9: nothing else went | Every shipped span that no request selects and that retention keeps is still visible in `span_core`, and the model's stage B dropped only spans that a request selects |

The suite fails unless frames, all three kinds of request, the retention change, deletes, partition drops, mask
applications, hidden reads, shipper stalls and frames whose MCP rows alone carry their user occurred, and unless
every accepted request completed. The profile with every fault also requires crashes, long downtimes, lease
steals, failovers, store outages and stage B drops.

## Mutants

Each seeded defect is a value of `crucible_lifecycle::Mutant`, which exists only with the feature `test-mutants`.

| Mutant | Where the defect is | What the oracle reports | Catching seeds |
|---|---|---|---|
| Skip the checkpoint wait | The deletion executor plans a logged request without waiting for every live node's checkpoint | I9: a row received before the request landed after the delete and is still there | 8 of seeds 3000..3008, without other faults |
| Drop the mask cycle | The mask scheduler plans nothing | I9: masked rows are still on disk; L3: no request is verified | 3 of seeds 4000..4003, both reports in each |
| Delete without fencing | An action is sent without its fenced record | L1: an action reached the store with no row recorded as issued | 5 of seeds 5000..5005, with every fault |
| MCP rows by trace only | A deletion by user reaches the MCP tables only through the traces its spans resolve to, never by the operations' own `user_id` and `user_subject_hash` | I9: an MCP row that carries the user, by `user_id` or subject hash, in a trace no span of which names the user is still there | 6 of seeds 6000..6006, without other faults |

The checkpoint mutant needs frames that a live node has not shipped when the request is logged: that is what the
shipper stalls are for. It runs in the profile without other faults, and a seed that shows the defect is run again
without it and must hold.

## What the simulation does not cover

- The two rules of stage B in the real normalizer and shipper. The real shipper seals a `Tombstone` frame, so
  its checkpoint passes it (the directed scenarios run that path), and it applies no tombstone to the frames after
  it. The simulation's stage B is a model of the two rules above, so invariant I9 holds end to end in the binary
  only once the normalizer and the shipper implement them.
- Replay of retained segments, requeue from quarantine and `crucible wal drain`.
- The size of a statement. A ClickHouse server takes at most 1,000 parameters in a request, and every trace id of
  a list is one. The executor resolves and deletes a list of traces 500 ids at a time, which the conformance row
  runs on a real server. A read carries at most 128 trace ids and 32 user or session deletions of the pending
  set, oldest first; what does not fit is filtered from the recent index and stays readable from the store until
  its deletes have run.
- Reads hide the MCP rows of a pending deletion by user by `user_id` only. A row that carries only the hash of
  the user's subject stays readable until the executor's delete runs; the oracle checks that the delete reaches it.
- Comments and media, which this deployment does not hold; a completed request reports each as `not_present`.
  Export files made before a request are expired and removed when the request completes; a directed scenario
  checks the expiry.

## Running it

```sh
cargo nextest run -p crucible-lifecycle
cargo nextest run -p crucible-sim --test lifecycle_dst
```

Replay one seed in the profile with every fault:

```sh
CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test lifecycle_dst replay_seed --run-ignored all --no-capture
```
