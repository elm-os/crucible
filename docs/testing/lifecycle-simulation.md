# Lifecycle simulation

How retention and deletion (contract 13, invariant I9) are verified: the retention executor, the mask scheduler,
the deletion executor and the per-node timer of `crucible-lifecycle`, and the `Service` behind the API. The method
is in [`methodology.md`](methodology.md); the job runners below the executors are in
[`jobs-simulation.md`](jobs-simulation.md) and the store models in [`control-store.md`](control-store.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Executors | `crates/crucible-lifecycle` | The four leader jobs under the `lifecycle` lease (retention, mask, deletion and the purge sweep), the node timer, the service |
| Unit and property tests | `crates/crucible-lifecycle/src`, `crates/crucible-plan/src/hidden.rs` | The drop-or-delete decision of retention, the instant before which no project can write against the ingest rule (no project, live or made later, writes before it, however late its spans ship), the partitions of a span of time, the reach list as data (a project deletion's every table included), the predicates of a delete (a project deletion's, without an arrival bound), the pending-deletion set, the predicate a pending deletion adds to a plan, the tombstone of a project deletion |
| Simulation | `crucible_sim::lifecycle_sim`, `crates/crucible-sim/tests/lifecycle_dst.rs` | The real jobs and the real service on several nodes over `ChModel` and `PgModel` under faults, with the oracle and the mutants |
| Directed scenarios | `crates/crucible-sim/tests/lifecycle_directed.rs` | One node with the real WAL and the real shipper: the tombstone is a durable frame that reads back as the request, the checkpoint passes it on a node that receives nothing else, a deletion by trace runs to `completed`, and a project deletion made as `DELETE projects/{id}` makes it logs a project tombstone, empties every table of the project (the rollups the shipper wrote and a frame taken after the deletion included), removes its objects and salt, and runs to `completed`; with `max_event_age` at 5 days, a project deletion drops only its partition that no project can write to any more beside a project made now, drops nothing such a project can still write to beside a project made forty days ago (whose row lands there as the purge runs and stays), and deletes today's partition when a project is made while it runs (whose row stays); a project too large for one read is purged, and a read that keeps failing is recorded on the request with its attempts; a frame taken after the tombstone and held until the request completed is deleted within one sweep interval of landing; a project that an older version deleted without a request gets one request, under two servers' leader jobs, and is purged |
| Project deletion in the control plane | `crates/crucible-auth/src/service/tests.rs`, `crates/crucible-api/tests/api/lifecycle.rs` | The project deletion request is made in the transaction that marks the project deleted; org owners and admins read it after the deletion and nobody else does; `POST deletion-requests` refuses the kind `project` and says how to delete a project |
| Jobs of a deleted project | `crates/crucible-sim/tests/deleted_project_jobs.rs`, `crates/crucible-export/tests/job.rs`, `crates/crucible-sqlws/src/tests.rs` | From the moment of deletion the alert scheduler writes no tick of the project's rules, the evaluator runs none of them, the outbox sends nothing to its channels, an export waiting to run ends with `project_deleted`, and the SQL reconciler drops the project's identity; each beside a live project for which the job acts |
| Stage B | `crates/crucible-normalize/src/tombstone.rs`, `crates/crucible-normalize/src/stage_b.rs`, `crates/crucible-sim/tests/stage_b_deletions.rs` | What a deletion selects of spans, MCP operations and facts, against a linear model; that dropping leaves stage B's state alone; the frames after a tombstone, a request that completed while the node was down, and a shipper that ships nothing until it has read the requests, each also with the defect it guards against; a read that leaves out a request made before the frames arrived and reads again for a frame that arrived before it after the wall clock stepped back; the drop counts by kind |
| Write path | The dimension `deletions` of the system simulation ([`system-simulation.md`](system-simulation.md)) | The real WAL, stage B and shipper with deletion requests, tombstones and requests that complete while the node is down, under the system simulation's faults, with invariant I9 and three mutants |
| Read services | `crates/crucible-query/tests/services/hidden.rs` | Trace detail, the trace list, the payload route, the session, user and span lists and the pages of an export in progress under a pending-deletion set, from the recent index and from the store |
| Project salts | `crates/crucible/src/salts.rs` | A deletion by user hashes the user with the stored salt as stage B did, never creates a salt, and says how to fix one that does not open |
| Differential, ClickHouse | The row "ChModel differential: retention and deletion" of `crucible-conformance` | The reads and maintenance calls the executors issue, on the model and on ClickHouse 26.3, 26.8 and latest, among them the issuers of the MCP operations, the MCP traces of a user, the delete of a user's operations with a full chunk of subject hashes, and a project deletion's partitions of a table and probes of the project's rows in a partition and in a table |
| Differential, Postgres | The `lifecycle` script of `crucible-conformance` | Every statement of `LifecycleRepo` on `PgModel` and on Postgres 15 and 18 |
| Routes | `crates/crucible-api/tests/api/lifecycle.rs` | Roles and scopes, `If-Match`, idempotency and every refusal, over `PgModel` |

## What runs

Three nodes (five in one test) each start the real `Registry` with the node timer and the four leader jobs, and
hold the real `Service`. What the simulation models is a node's log and its stage B:

- a node's log is a list of frames that survives the node's crashes. A frame holds one to three spans of one
  trace, or a tombstone. `NodeLog::append_tombstone` appends a tombstone frame;
- a shipper task moves the frames to the store in log order: for a frame of spans it inserts one block into each
  of `span_core`, `span_payload`, `trace_heads`, `mcp_ops`, `mcp_content`, the four fact tables, `trace_stats`,
  `session_rollup`, `user_rollup`, `span_metrics`, `mcp_tool_rollup`, `mcp_tool_rollup_hourly`, `mcp_deploys` and
  `mcp_tool_inventory`, then raises the checkpoint watermark to the frame;
- stage B keeps two rules. A tombstone applies to the frames after it in the log: their spans that the request
  selects and that were received before it are dropped, and for a deletion by user also the MCP rows that carry
  the user by `user_id`, or by the hash of the user's subject under the issuer the request names (under any
  issuer when it names none), while the rest of the frame ships. A node that starts registers first
  (its heartbeat is in Postgres), then reads the deletion requests, and only then ships.

A workload appends a frame about once a second to a node that is up. Three projects get traces of five users and
seven sessions, on fresh days (today and yesterday) and on old days (six to eleven days back). The first project's
old days reach further than the second's, and the third's further still, so some old partitions hold one project
and some hold two or three. Half the
traces show their user everywhere. The others show it only on their MCP rows, as `user_id` or only as the hash
of the user's subject under one of two issuers, or on their spans with the MCP rows hashed: a deletion by user
has to reach those MCP rows by their own columns, in partitions where no span names the user. Every 15 to 50 s a
caller asks a node for a deletion by trace, by user or by session, with an idempotency key, and sends the
request again when the call fails. A deletion by user names one of the two issuers two times in three: it reaches
the MCP rows hashed under that issuer and keeps those hashed under the other. Once in a run the first project's
retention is shortened to three days.

`ingest.max_event_age` is 12 days. The first two projects are made as the run starts and the third forty days
before. The third also holds history it sent when that was younger than 12 days: six frames whose spans start 15 to
25 days back, in partitions that no project can write to any more, which its deletion drops.

Once in a run, between 3 and 13 minutes in, the third project is deleted the way `DELETE projects/{id}` deletes
it: one transaction marks its row deleted and makes its project deletion request, sent again while it fails. Its
frames are still taken for up to 5 s after that, as a node takes them until its key snapshot has the revocation,
and no deletion request names it any more. The second and the third project each start with one row of every
control-plane table a project deletion removes: a membership, an API key, a channel with its sealed secret, an
alert rule with the state of a series, a saved query, a SQL run, an export, a retention floor and a salt.

The second project, which keeps its data forever, also writes late. At the moment the deletion's drop or delete of
a `span_core` partition reaches the store, a frame of the second project whose spans start in that partition lands
there, in every table, as a node ships a span late that ingest took when it started at most 12 days before its
arrival, after the project was made. A partition that ends before that gets no such frame, since no node takes one.
The deletion has to keep those rows too, so it may drop a partition only where no project can still write.

Rows of the third project also land after its request has completed, once the faulty phase is over and up to a
minute apart: a frame a node took two seconds after the deletion while its key snapshot lagged, one a node without a
heartbeat took a minute before it, one a node cut off from Postgres took twenty minutes after it under `fail_open`,
and one from the WAL of a node back after weeks, into a partition of the history that the deletion dropped. A
request that completes too late for a sweep and a mask cycle to follow before the end gets none.

The mask cycle is 30 min, the retention executor plans every 10 min and the purge sweep looks at each deleted project
every 10 min, so that a run covers several cycles; the other settings are the defaults.

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

The faulty phase lasts 15 min of virtual time. A fault-free tail of 100 min follows, more than three mask cycles,
and at its end every node is stopped.

## The oracle

| Invariant | Check |
|---|---|
| L1: every action on ClickHouse is recorded under the fencing token before it is issued | The leader reaches the store through a wrapper. At each delete, partition drop and mask application it looks for an action row of that kind, table and partition in the state `issued` with a fencing token |
| L2: a deletion is hidden at once on the accepting node | When `request_deletion` returns, the node's pending-deletion set holds the request, and the plan that reads exactly the spans the request selects returns nothing once it carries the set |
| L3: every request completes | At the end every deletion request is `completed` and every action is `verified` |
| I9: nothing a completed request selects is left | Every row the store still holds of the seventeen tables, masked or not, is compared with every completed request: a span table by the request's own column, the MCP, fact and stats tables by the traces the request resolves to, the MCP tables for a deletion by user also by their own `user_id` and by the hash of the user under the issuer it names, or under either when it names none, the rollups by key. A row received at or after the request is kept; a table without an arrival time is exempt for a key that got data after the request. A completed project deletion selects every row of its project in every table, whatever its arrival or when it landed |
| I9, eventual: a row of a deleted project that lands after its purge goes | For each frame of the third project that landed after its request completed, a delete of the project's rows or a drop of its `span_core` partition reached the store within one sweep interval and three runs of the executor of the landing; at the end the row is gone, masked or not, like every other row of the project |
| I9: nothing retention expired is left | No table with a retention binding holds a row of the shortened project that is more than five days old, masked or not |
| I9: nothing else went | Every shipped span that no request selects and that retention keeps is still visible in `span_core`, and so is every `mcp_ops` row that ever landed and that no request selects: a deletion by user that names an issuer keeps the rows hashed under the other, and a project deletion keeps the rows of the other projects in the partitions it dropped or deleted from, the late frames of the second project included. The model's stage B dropped only spans that a request selects at its point in time |
| I9: the control plane | Once the project deletion completed, the deleted project holds no row of the eleven control-plane tables seeded for it (alert state and channel secrets among them), and the second project still holds each of its rows |

The suite fails unless frames, all three kinds of request, the retention change, deletes, partition drops, mask
applications, hidden reads, shipper stalls, frames whose MCP rows alone carry their user, deletions by user that
name an issuer and MCP rows that such a deletion kept because they are hashed under the other occurred, a project
was deleted, frames of it were taken after its deletion, late frames of the second project landed in its partitions,
its control-plane rows were checked, partitions of its history were dropped and frames of it landed after its request
completed, each removed in time, and unless every accepted request, the project deletions included, completed. The profile with every fault also requires crashes,
long downtimes, lease steals, failovers, store outages and stage B drops.

## Mutants

Each seeded defect is a value of `crucible_lifecycle::Mutant`, which exists only with the feature `test-mutants`.

| Mutant | Where the defect is | What the oracle reports | Catching seeds |
|---|---|---|---|
| Skip the checkpoint wait | The deletion executor plans a logged request without waiting for every live node's checkpoint | I9: a row received before the request landed after the delete and is still there | 8 of seeds 3000..3008, without other faults |
| Drop the mask cycle | The mask scheduler plans nothing | I9: masked rows are still on disk; L3: no request is verified | 3 of seeds 4000..4003, both reports in each |
| Delete without fencing | An action is sent without its fenced record | L1: an action reached the store with no row recorded as issued | 5 of seeds 5000..5005, with every fault |
| MCP rows by trace only | A deletion by user reaches the MCP tables only through the traces its spans resolve to, never by the operations' own `user_id` and `user_subject_hash` | I9: an MCP row that carries the user, by `user_id` or subject hash, in a trace no span of which names the user is still there | 6 of seeds 6000..6006, without other faults |
| Drop shared partitions | A project deletion drops each partition it reaches, whatever other projects hold rows there | I9: a span of a project that is not deleted, which no request selects and retention keeps, is gone | 6 of seeds 7000..7006, without other faults |
| Drop within max_event_age | A project deletion drops a partition in which every project with rows is deleted without the event-age bound, although a live project can still write there | I9: a late span of the second project, which no request selects and retention keeps, is gone | 6 of seeds 8000..8006, without other faults |
| Sweep disabled | The purge sweep never runs | I9, eventual: the rows of the third project that landed after its request completed were not removed in time, and are still there | 6 of seeds 9000..9006, without other faults |
| Sweep without the project predicate | The purge sweep removes the rows it finds by dropping their partition | I9: a span of a live project, which no request selects and retention keeps, is gone | 6 of seeds 9100..9106, without other faults |

The checkpoint mutant needs frames that a live node has not shipped when the request is logged: that is what the
shipper stalls are for. It runs in the profile without other faults, and a seed that shows the defect is run again
without it and must hold.

## What the simulation does not cover

- The two rules of stage B in the real normalizer and shipper: here stage B is a model of them. The system
  simulation runs the real ones under its faults (dimension `deletions`), and the stage B directed scenarios run
  each rule on its own; a project tombstone selects every span, MCP operation and fact of its project there.
- Stage B keeps a frame of a deleted project that a node took after the deletion, as it keeps whatever a deletion
  received after its point in time; the purge sweep removes its rows once they land. The run lands such frames
  after the request completed, as shipped by a node that lagged, was cut off or was down, without modelling those
  nodes' logs.
- Replicas of a clustered store. The model is one server; the every-replica reads that a clustered store makes for
  a partition's projects, the partitions of a table, the probes of a project's rows, mutations and masked parts are
  run on a real cluster by the cluster workflow's conformance.
- Replay of retained segments, requeue from quarantine and `crucible wal drain`.
- The size of a statement. A ClickHouse server takes at most 1,000 parameters in a request, and every trace id of
  a list is one. The executor resolves and deletes a list of traces 500 ids at a time, which the conformance row
  runs on a real server. A read carries at most 128 trace ids and 32 user or session deletions of the pending
  set, oldest first; what does not fit is filtered from the recent index and stays readable from the store until
  its deletes have run.
- Reads hide the MCP rows of a pending deletion by user by `user_id` only. A row that carries only the hash of
  the user's subject stays readable until the executor's delete runs; the oracle checks that the delete reaches it.
- Comments, which this deployment does not hold; a completed request reports them `not_present`. Media are
  reached through their uses: a deletion by trace deletes the uses of those traces in its own fenced transaction,
  and the media collector removes an object once nothing uses it; a directed scenario checks the uses and the
  media place's `done`, and [media-simulation.md](media-simulation.md) covers the collector. Export files made
  before a request are expired and removed when the request completes; a directed scenario checks the expiry.

## Running it

```sh
cargo nextest run -p crucible-lifecycle
cargo nextest run -p crucible-sim --test lifecycle_dst
```

Replay one seed in the profile with every fault:

```sh
CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test lifecycle_dst replay_seed --run-ignored all --no-capture
```
