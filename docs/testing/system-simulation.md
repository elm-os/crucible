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
| Clients | Model | Seeded OTLP clients that build protobuf and JSON bodies and call the ingest router in process. Spans report a session, a user, an environment, a release, a model, usage, a provided cost and an error status from small domains. With `facts`, the clients also write scores, tag and flag edits and corrections. With `mcp`, requests also carry MCP operations: each is one simulated exchange, written as the keys that an SDK wrapper, the proxy or a server without either would send (contract 9.1). Each MCP span also carries an event with every key that must never be stored (contracts 3.5, 9.3 and 9.4), under values that no stored row may hold |
| Ingest | Real | `crucible_ingest::Ingest::router()`: authentication, admission, the scan, the WAL append, the ack; and `Ingest::append_facts`, the entry point the fact routes call |
| WAL | Real | `crucible-wal` on `SimDisk`, the disk fault model of contract 2.4 |
| Shipper | Real | `crucible-shipper`: stage A and B, the span tables, the rollups, the five MCP tables and the fact tables, seals, inserts, probes, pair and frame quarantine, checkpoints, garbage collection |
| Project salts | Model | One fixed salt per project behind the shipper's `ProjectSalts` hook; with `mcp_identity` a share of its reads fails, and without it the node has no control store. The production hook (`crucible/src/salts.rs`: the sealed `project_salts` rows, created with their project, loaded at startup and every 5 s, and read on a project's first frame until then) has its own tests over `PgModel` |
| Recent-span index | Real | `crucible-recent`, fed by stage B through `crucible_shipper::RecentHook` |
| Query service | Real | `crucible_query::QueryService`: trace by id and the trace list, decorated with their facts; the facts of a target; the metrics query |
| ClickHouse | Model | `ChModel`: inserts, deduplication, probes, the typed-plan interpreter, merges, and seeded faults |
| Keys | Real | `crucible_auth::StaticKeys`, one secret key per project |
| Deletion requests | Model | With `deletions`: the requests of a control store behind the shipper's `DeletionRequests` hook, some of whose reads fail; a node timer that appends each open request's `Tombstone` frame to the real WAL and completes the request once the checkpoint passes it; requests that complete while the node is down. Stage B, which applies them, is the real one (contract 13.3, step 3). The production hook (`crucible/src/lifecycle.rs`: the node registers, then reads every request in every state page by page) has its own tests over `PgModel` |

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

Also outside: the network (requests enter at the router, so HTTP framing and TLS are not simulated), Postgres
(the project salts and the deletion requests are models behind the shipper's hooks, and the control store's
statements run against `PgModel` in their own suites), and several nodes (the simulation runs one node).

The simulation's `WalView` re-reads a span through its locator as the binary does (`Wal::read_at`, then stage A),
and the oracle follows the locator of a span that a read returned from the index: a frame that reads must hold
that span at the locator's ordinal. The fact routes write through `Ingest::append_facts`, which the clients call
directly; at quiescence every target's facts are read back through `QueryService::facts`.

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
| Workload | `mcp` (MCP operations of both sides, in both eras, over stdio and HTTP, from all four capture sources), `mcp_double_capture` (an SDK and the proxy report one server call), `mcp_errors` (every class of the taxonomy, and annotated classes that the exchange does not warrant), `mcp_mrtr` (multi-round exchanges), `mcp_inventory` (tool lists that change), `mcp_deploys` (versions and deploy ids that change), `mcp_content` (opted-in arguments and results), `mcp_identity` (user subjects, a control store for the salts whose reads sometimes fail; without it the node is ingest-only), `deletions` (one more project, whose data deletion requests select; see "Deletions"), `json`, `large_payloads`, `deep_nesting`, `bad_unicode`, `rootless`, `concurrent_reads`, `client_retries` (whole requests again, and single spans again inside a new request), `multi_project`, `bursts`, `multi_day`, `facts`, `nested_usage` (spans form parent chains within a request, across requests with the child first, and under a span that a request sends again, so some have a descendant that carries usage; a span sent again before its parent arrives is weighted toward an exact repeat whose new child carries usage, see "Mutants") |
| Faults | `disk_crash_points`, `barrier_crashes`, `crash_loops`, `crash_in_recovery`, `clean_stops`, `disk_write_errors`, `disk_read_faults`, `disk_sync_errors`, `disk_full`, `slow_sync`, `ch_timeouts`, `ch_ack_lost`, `ch_server_errors`, `ch_rejects`, `ch_hang_overload`, `ch_poison`, `ch_probe_errors`, `ch_read_errors`, `window_expiry`, `outages`, `long_outages`, `wall_clock_steps`, `fsync_lie` (stress tier only), `frame_faults` (a seeded share of the durable frames does not normalize), `dir_sync_errors` (windows in which every directory sync fails) |
| Config | `wal_compression_off`, `small_batches`, `week_partitions`, `tiny_recent`, `small_segments`, `clustered`, `one_connection`, `replay_retention`, `project_rate` (a per-project byte rate), `small_budget` (a `shipper.encoded_bytes` budget of 20 or 200 KiB) |

That is 56 coins. New dimensions take the next bit of the mask and draw their settings after every earlier
draw, so a recorded mask and seed keep their meaning.

One combination is weighted. A quarter of the configurations get the profile `collection_under_repeats`
(`ConfigMask::collection_under_repeats`), which sets the dimensions that let garbage collection run inside an
unchecked batch that holds a repeat, with a restart after it. That is where stage B's rule that a restart decides
as the process before it did is exercised, and fair coins alone set those dimensions so in fewer than one
configuration in a hundred. With 200 configurations, any 6 chosen dimensions are all on in at least one of them with
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
| I2 | The model refuses a malformed block (one without its marker row, for instance) and records a second block of one (batch, table, partition); no (table, LSN, span) row twice, in the span tables, `mcp_ops` and `mcp_content`; no span with more rows than requests that carried it; every row belongs to a request that was sent; no two batch identities over the same frames; a pair the store had accepted is quarantined only after a probe that read absent. A restart keeps and flags the same spans of a frame in a sealed batch as the process before it did | In runs without a quarantine directory, repeats included: every root span has its head row and the `trace_stats` span counts sum to the `span_core` rows. Without a quarantined pair: `mcp_ops` holds one row per `span_core` row of a span with an MCP side, and `mcp_content` one per operation with captured content |
| I4 | Every fact row is a fact that was written and lands once (I2); every acknowledged fact is in its table, in a durable `FactBatch` frame above the watermark, or in a quarantine copy (I1). The rows resolve through `crucible_records::fold_facts` to what the facts as written resolve to, in either order of the rows | The same after the drain, and every target with fact rows reads back through `QueryService::facts` exactly the winning facts the reference fold picks over the stored rows (`system_sim/reads.rs`) |
| I5 | A refusal for load is a 429 or a 503 with `Retry-After`, a 429 only with `project_rate`; a fact write is refused only for load. The rows the shipper holds stay within `shipper.encoded_bytes` plus one frame | |
| I7 | Every `span_core` row carries the tokens, the cost in nano-USD and the error status its span reported. Every `mcp_ops` row carries the fields of the wire contract as the exchange had them (side, capture source, era, transport, method, target, server, deploy, client, times, sizes, parent, multi-round state, session key, group key, the hashed subject or, without a control store, no identity). No text cell of `span_core`, `span_payload`, `mcp_ops` or `mcp_content` holds a value that a span carried in an event under a key that must never be stored; the catalogue has no invariant of its own for that, so it is reported as I7 | Without a quarantined pair: `trace_stats`, `session_rollup`, `user_rollup` and `span_metrics`, merged by key, equal the reference aggregation over `span_core` exactly (counts, trace counts, error counts, token and cost sums, first and last seen, both histograms). The token and cost sums of the reference count every span that is not a usage aggregate, whatever its type. After the last restart the same holds on the read path: every project's session list and user list, read through the real `QueryService` page by page, and the detail of each session and user, equal the reference per session and per user over the window's UTC days, with the users of a session (up to the 8 the rollup keeps), each user's distinct session count and the sessions of each user taken from `trace_heads` in the window (`system_sim/lists.rs`, which records no event and draws no randomness). The `usage_aggregate` flag of every row is what the rule gives (see "Usage aggregates, exactly"). `mcp_tool_rollup`, merged by its key, equals the reference over `mcp_ops` (count, byte sums, both histograms, last seen), and counts per capture source what `mcp_ops` holds of that source, so rows of different sources are never summed; `mcp_deploys` equals the deploys of the server-side rows; `mcp_tool_inventory` equals the tools the listed operations sent, with the schema hash of their canonical JSON. Without a quarantine, the metrics query of every project returns, through `QueryService::metrics`, the span count and error count of its `span_core` rows, the input tokens and cost of those that are not usage aggregates, and the root heads of `trace_heads`, in the window of the run's spans (`system_sim/reads.rs`). One binary runs the whole simulation, so the upgrade exception of contract 5.3 does not arise |
| I9 | With `deletions`: no row of `span_core`, `span_payload`, `trace_heads`, `mcp_ops`, `mcp_content` or the fact tables that a completed request selects lands after the request completed. Every span, MCP operation and fact of the deleted project that no request selects is in its table, in a durable frame above the watermark or in a quarantine copy (I1 for that project) | The same after the drain |
| I8 | Every `mcp_ops` row has the status, error class, subclass, JSON-RPC code, HTTP status and caller-fault flag that `crucible_records::classify_mcp_error` gives for the exchange the client simulated: what happened (a result, `isError`, a code, an HTTP status, a cancellation, a deadline, a broken transport, an exception), the side, and the class an SDK annotated | The same after the drain |
| I3 | A trace-by-id read through the query service after every ack, and from concurrent readers for requests of any incarnation. A response that omits an acknowledged span must carry `partial` or `consistency: lagging`. `not_found` for an acknowledged trace is a violation. A marker must be warranted by the configuration: `store_unavailable` only with `outages`, `long_outages`, `ch_read_errors` or `tiny_recent`; `quarantined` only with `ch_poison` or `frame_faults` | Every acknowledged request is read back, and the whole trace list of every project must hold every acknowledged trace that has a root span. No marker is accepted, except `quarantined` when a quarantine directory exists. Repeated after the last restart |
| I6 | No trace-by-id response holds a span the project did not send, and no list holds a trace of another project (with `multi_project` the projects share trace ids) | The same |
| I12 | The recovered log equals the uninterrupted reference recovery although crashes cut the real one | A restart after the drain sends no insert and leaves the store and the watermark as they were |
| I13 | | No segment wholly at or below the watermark is left, and the index is empty once the grace has passed |
| I14 | | The trace hash covers every event of the run, read results included. The same seed, mask and tier give the same hash and outcome (`system_dst.rs`, on every pull request) |

While the workload runs, a marker excuses a missing span, and `consistency: lagging` is always accepted there:
the oracle cannot tell from outside whether stage B had a reason to trail. So the suite also requires that live
reads mostly need no marker: over the smoke budget more than half of the live trace reads return every span,
whole and strong, and in configurations with the store faults switched off (outages, read faults, insert
faults, poison pairs, the tiny index bound, frame faults, the small row budget) more than 90% do (measured: 572
of 580 over 20 configurations; every trace read also reads the trace's facts, so fewer reads fit a run). The
smoke ratio leaves out the runs with `frame_faults` or `small_budget`: one quarantined frame marks
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
  leaves open what a restart finds (`ship_directed.rs`: a hint that cannot be replaced, and a removal
  whose directory sync fails). While it retries at every tail poll, the failure is logged once per
  `gc_interval` with a count.

Nine corpus lines (invariant I7, seeds 44368508872000 and following) are runs that failed before these rules:
the rollups counted a repeat that `span_core` did not hold, or the reverse.

### Usage aggregates, exactly

The flag `usage_aggregate` is stage B state, and stage B holds what the retained prefix of the WAL holds. The
oracle listens at the shipper's recent-index hook: for every frame that stage B admits it records all the spans
stage A made of it (from the frame itself, so the exact repeats that stage B then drops are there too), whether
each carries usage as the client sent it, and the first LSN the WAL retains at that moment. At quiescence it runs
the rule of contract 5.3 itself: the set starts empty at the first retained LSN; in LSN order every span of a
frame that carries usage or is in the set adds its parent, to a fixed point over all the spans of the frame; a
span is an aggregate when it is in the set then. Every `span_core` row must carry exactly that flag, and two
admissions of one frame above the watermark, before and after a restart, must give the same aggregates. So a
mark taken after the duplicate drop, or a prefix that lost a frame an unchecked frame relied on, is a violation.

### Deletions

With `deletions` one more project gets a share of the traffic, and deletion requests select its data: by one or
two of its traces, by a user (of its spans or its MCP operations, or an MCP subject) with or without a token
issuer, by a session, and now and then the whole project. A request is accepted on a node whose clock is up to
2 s off this one's, so frames that reach this node after the tombstone can still have arrived before
`requested_at`. The node timer appends the `Tombstone` frame of every open request to the real WAL, and a request
completes once the checkpoint passes its tombstone and the inserts in flight then have landed: an attempt sent
before stage B knew the request can land later, and the executor's verification deletes it, which is no part of
stage B. At each start, before the shipper runs, some requests complete as if while the node was down: no
tombstone of them is in the log, and the shipper must read them before it ships.

The oracle knows every span of every admitted frame of the project as stage A made it (trace, `user_id`,
`session_id`) and when the frame arrived, the identity of every MCP operation as the exchange carried it, and when
every fact arrived. It applies the rule of contract 13.3 itself, with a linear reading of each request. Pairs of
one batch may land before and after a restart that knows more requests (a replay drops more rows, never fewer,
contract 5.3), so the project is left out of the exact comparisons (the rollups, the MCP aggregating tables, the
head rows and `trace_stats` counts, and the spans a restart keeps); the usage-aggregate flags are checked for it,
because a deletion does not change them.

Three weightings make the rule about an issuer show. A request made at start names, three times in four and when
it can, the subject of an MCP operation in a frame above the watermark, under the other issuer half of the time.
An operation of the project that carries a subject carries, three times in four, a user that a request names,
under the issuer the request does not name. And a deletion of the whole project, which selects everything that
arrived before it, is rare.

## Mutants

Each defect is behind a `test-mutants` cargo feature that only `crucible-sim` enables. Each must be caught inside
the per-pull-request budget (200 configurations from configuration seed 0, 3 seeds each), and its first catching
run is recorded in `crates/crucible-sim/tests/system_dst.rs`. The same run must pass without the defect.

| Mutant | Crate | Defect | Caught as | Seed | Config |
|---|---|---|---|---|---|
| `skip-wal-sync` | `crucible-wal` | A group's waiters complete before its `sync_data` | I3: a trace-by-id read of an acknowledged request finds no trace (other runs of the budget report liveness, I1 or I9) | 0 | `0xb04e3dca437a47` |
| `drop-token` | `crucible-shipper` | Every insert attempt carries a new deduplication identity | I2: a second block of one batch lands | 1000 | `0x1f2fe574beef1d` |
| `gc-above-checkpoint` | `crucible-wal` | Garbage collection removes segments above the checkpoint watermark | Liveness: the shipper stops because a frame above the watermark was reclaimed (most runs report I12: recovery finds a gap between the watermark and the oldest segment) | 0 | `0xb04e3dca437a47` |
| `silent-eviction` | `crucible-recent` | Entries dropped at the index bound leave no gap | I3: a read omits an acknowledged span and looks whole | 1 | `0xb04e3dca437a47` |
| `skip-probe-after-unknown` | `crucible-shipper` | A retry after an unknown outcome skips the probe | I2: a second block of one batch lands | 7000 | `0xc443c5fa7b02ff` |
| `ack-before-append` | `crucible-ingest` | The client gets 200 before the WAL append resolves | I3: a trace-by-id read of an acknowledged request finds no trace (other runs report I1, and I9 for the deleted project) | 0 | `0xb04e3dca437a47` |
| `double-count-rollup` | `crucible-shipper` | One row of every rollup block is counted twice | I7: a rollup differs from the reference aggregation over `span_core` | 1 | `0xb04e3dca437a47` |
| `drop-fact-on-retry` | `crucible-shipper` | A pair of a fact table counts as landed when its insert is retried | I1: an acknowledged fact is in no table, frame or quarantine copy (I9 for the deleted project) | 3001 | `0x7fce3d29abb2c7` |
| `frame-quarantine-without-copy` | `crucible-shipper` | A frame that does not normalize is quarantined without its copy | I1: an acknowledged request is in no table, frame or quarantine copy (other runs report I3, and I9 for the deleted project) | 0 | `0xb04e3dca437a47` |
| `count-aggregate-usage` | `crucible-shipper` | The rollup sums take the tokens and cost of a usage aggregate | I7: a rollup differs from the reference aggregation over `span_core` | 10000 | `0xba10fc4cdc20e` |
| `collect-what-frames-rely-on` | `crucible-shipper` | Garbage collection removes segments that the admission of an unchecked frame relied on | I2: the WAL no longer holds a frame that the admission of an unchecked frame relied on | 21000 | `0xc38b1d5993bffe` |
| `misclassify-caller-fault` | `crucible-normalize` | An invalid-params error on a server-side row is not the caller's fault | I8: an `mcp_ops` row differs from the shared classifier in `caller_fault` | 2001 | `0x21c6206ca7c81b` |
| `sum-capture-sources` | `crucible-shipper` | Every `mcp_tool_rollup` partial gets one capture source | I7: the rollup differs from the reference over `mcp_ops`, and its counts per capture source are not those of `mcp_ops` | 1 | `0xb04e3dca437a47` |
| `drop-rollup-marker` | `crucible-shipper` | The blocks of `mcp_tool_rollup` are sent without their marker row | I2: the store model refuses the block | 0 | `0xb04e3dca437a47` |
| `mark-after-duplicate-drop` | `crucible-normalize` | Stage B flags the usage aggregates of a frame after its exact repeats were dropped | I7: a span is not a usage aggregate although a dropped repeat of its frame passes the mark on to it | 14000 | `0x9aad7dced818fe` |
| `skip-tombstone-filter` | `crucible-shipper` | Stage B applies no deletion: neither a `Tombstone` frame of the log nor a request read before shipping drops anything | I9: a row that a completed deletion request selects landed after the request completed | 2 | `0xb04e3dca437a47` |
| `ignore-tombstone-issuer` | `crucible-normalize` | A deletion by user that names an issuer matches the subject hash of an MCP operation under the operation's own issuer | I9: an MCP operation that no deletion selects (its subject is the user's under another issuer than the request names) is in no row of `mcp_ops` | 6000 | `0xec5be8fac317f1` |
| `seed-only-open-requests` | `crucible-shipper` | Before shipping, the shipper applies only the deletion requests that are still open, not those that completed while the node was down | I9: a row that a request completed while the node was down selects landed after the request completed | 2 | `0xb04e3dca437a47` |

Of the 600 runs of the budget, the mutants are caught by 594, 140, 57, 44, 101, 500, 466, 74, 145, 154, 21, 48,
219, 292, 41, 127, 11 and 96 runs, in the order of the table. The dimension `deletions` moved the first catching
runs of several mutants: before it, the first fifteen were caught by 593, 137, 62, 46, 105, 499, 480, 76, 149, 162,
20, 48, 229, 292 and 43 runs. Before the planted event keys and the weighting of `nested_usage` below, they were
caught by 587, 147, 59, 43, 102, 498, 479, 71, 157, 161, 22, 49, 233, 292 and 2 runs. Before the
dimensions, the weighted profile and the oracle checks of milestone 3, the first eleven were caught by 592, 163, 41,
25, 106, 474, 480, 84, 169, 175 and 0 runs.

`ignore-tombstone-issuer` needs a deletion by user that names an issuer, applied to a frame that arrived before
the request and holds an MCP operation whose subject is that user under the other issuer, which no other request
selects. Without the weightings of "Deletions", 2 runs of 600 caught it. `skip-tombstone-filter` and
`seed-only-open-requests` need no weighting: a request accepted on a node whose clock runs ahead, and a request that
completed while the node was down, leave frames that only stage B keeps out of the store.

`collect-what-frames-rely-on` was caught by no run of the budget before. Two things changed that. The profile
`collection_under_repeats` makes a quarter of the configurations ones where garbage collection can run inside an
unchecked batch that holds a repeat. And the oracle now checks the rule of contract 5.3 where it is broken, not
only where its effect lands: at every frame that stage B reads, the WAL must still hold the frame that the
admission of each unchecked frame relied on (within one process, and across a restart for the frames of a sealed
batch, whose pairs may have landed), and a restart must keep and flag the same spans of such a frame.

`mark-after-duplicate-drop` needs a repeat that is still in the duplicate window, a new span in its frame that
carries usage under it, and the repeat's parent after it; without a weighting, 2 runs of 600 caught it. Under
`nested_usage` the generator now weights that frame. A span that names a parent that a later request sends carries
usage one time in four. When such a span carries none and its request was acknowledged, the next request of its
project that may repeat a span (with `client_retries`) sends the latest of them again, with a new child that carries
usage and the awaited parent beside it. The directed and property tests of `crucible-normalize`
(`usage_aggregate.rs`) cover the same defect on every run.

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
`skip-probe-after-unknown`, `ack-before-append`, `double-count-rollup`, `drop-fact-on-retry`,
`frame-quarantine-without-copy`, `count-aggregate-usage`, `collect-what-frames-rely-on`,
`misclassify-caller-fault`, `sum-capture-sources`, `drop-rollup-marker`, `mark-after-duplicate-drop`,
`skip-tombstone-filter`, `ignore-tombstone-issuer`, `seed-only-open-requests`.

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
