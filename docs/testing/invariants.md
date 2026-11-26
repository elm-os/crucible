# Invariants

The invariant catalogue for contract v2 ([`../architecture/contracts.md`](../architecture/contracts.md)). The
method is in [`methodology.md`](methodology.md).

## How invariants are checked

| Kind | Where | What it means |
|---|---|---|
| Continuous | `crucible-sim`, after every simulation step | Cheap checks against the oracle ledger (every request sent, every ack with its virtual time, every delete, every revocation, a reference aggregation) |
| Quiescence | `crucible-sim`, at the end of a run | All faults healed, the system drained, one final restart, then full comparison with the oracle |
| Conformance | `crucible-conformance`, against the models and real ClickHouse (26.3 LTS, 26.8 LTS, latest stable) and Postgres (15, 18) | The assumption an invariant rests on holds on the real system. A mismatch means the model is fixed first |
| Proof | Kani (pure kernels) and the Quint model `specs/ingest.qnt` | Holds for all inputs within the stated bounds, or for all interleavings of the model |

A violation blocks a merge when it is new on the change and reachable inside the realism envelope (realistic
tier). I1 is claimed for `wal.durability = group` on a disk that honours `sync_data`; runs with the fsync-lie fault
are labelled and belong to the stress tier.

## Catalogue

### I1. No acknowledged write is lost

Every acknowledged span, fact and tombstone is, at all times after its ack, in at least one of: the durable WAL
above the checkpoint watermark, ClickHouse, or an open quarantine. A quarantined batch counts as durable.

| Check | Kind |
|---|---|
| An ack is issued only after `sync_data` for the group that holds the frame returned `Ok` | Continuous |
| Every acked span key is in `span_core` (or was dropped as an exact duplicate inside the duplicate window), or in the durable WAL, or in a quarantine | Quiescence |
| `AckOnlyAfterSync`, `NoAckedLoss`, `TruncateOnlyBelowCheckpoint`; liveness: every acked span eventually lands | Proof (Quint) |
| Frame codec: no panic, round-trip, bounded length, single-bit-flip detection, iterator progress | Proof (Kani) |
| Load generator: exact count plus a sampled anti-join after drain | Performance rig |

### I2. Exactly-once rows, per table

For **every table, base and derived**, and every (batch, table, partition) pair: the pair's block lands exactly
once, across retries, lost acks, crashes, restarts and normalizer upgrades. It follows that:

- each acked span that the duplicate window did not drop appears exactly once in `span_core` and once in
  `span_payload`, and each MCP operation exactly once in `mcp_ops`;
- the window decides alike for every table: the pairs of one batch admit the same spans whether they land before
  or after a restart (contract 4.6, step 6), so runs with repeated spans are checked like any other;
- each accepted fact appears exactly once in its fact table;
- every aggregating table (`trace_stats`, `session_rollup`, `user_rollup`, `span_metrics`, `mcp_tool_rollup`,
  `mcp_tool_rollup_hourly`, `mcp_deploys`, `mcp_tool_inventory`) holds exactly one marker per pair, and its merged
  measures equal the oracle's;
- a span that was never acknowledged appears at most once;
- the rows of a quarantined pair appear zero times until requeue and exactly once after it.

| Check | Kind |
|---|---|
| Row counts per span key per plain table; marker counts per pair per aggregating table; sums against the oracle | Quiescence |
| No insert is sent before its `BatchSeal` is durable; no retry after an unknown outcome without a probe | Continuous |
| `NoDuplicateRows`, `BatchIdentityStable`, `CheckpointMonotonic` | Proof (Quint) |
| Token injective over `(wal_epoch, first_lsn, last_lsn, table_id, partition_id)`; partition routing total and monotone | Proof (Kani) |
| C1 token dedup, C2 plain-table window, C3 bounded logs, C4 token overrides content hash, shared-engine dedup settings, a single Native block is not re-split, a two-partition block is rejected | Conformance |
| Startup dedup self-test | Runtime |

Mutants that must be caught: skip the sync, drop the token, truncate early, skip the probe after window expiry.

### I3. Read-your-ack, with the index-gap rule

After every ack, and after every restart once recovery has rebuilt the index, a trace-by-id request on the
ingesting node through the real API handler returns the span. The only permitted alternatives are explicit:

- `partial: true` with `partial_reason: store_unavailable`, when the read could touch an open index gap (entries
  dropped during a store outage before they were visible in ClickHouse);
- `consistency: lagging`, when the read barrier expired or a peer did not answer;
- `partial: true` with `partial_reason: quarantined`, when the read could touch a span whose (table, partition) pair
  is quarantined: such a span may leave the recent index while absent from ClickHouse until it is requeued.

A response that omits an acknowledged span and carries neither marker is a violation. In the realistic tier with
the store reachable and no open gap, the span must be present.

| Check | Kind |
|---|---|
| Trace-by-id after every ack and after every restart | Continuous |
| `ReadYourAck` | Proof (Quint) |
| One writer and many readers on the index; the gap bookkeeping | Shuttle |
| C11 read-after-write on the inserting replica; replica lag against the visibility grace | Conformance |

### I4. Facts resolve deterministically

For any set of fact rows, the read result equals the reference fold: per `(target, key)` the row that is greatest
under `crucible_records::fact_order` (`version`, then `fact_id` in RFC 9562 byte order), with a retraction meaning
"no value". The result does not depend on arrival order, merge
timing, part layout or restarts. Effective tags equal ingest tags plus latest-add minus latest-remove.

| Check | Kind |
|---|---|
| Latest-wins equals the reference fold over every interleaving | Property test, Quiescence |
| No rendered statement contains `FINAL` | Property test |

### I5. Backpressure, never out of memory

Tracked bytes in every `ByteBudget` (`ingest.inflight_bytes`, `wal.unshipped_bytes`, `normalize.cache_bytes`,
`shipper.encoded_bytes`, `recent.bytes`) stay at or below their caps, except that the rows the shipper holds can
exceed `shipper.encoded_bytes` by one frame, because frames are never split (contract 4.8). When a cap is hit the
answer is 429 or 503 with `Retry-After` (or the gRPC equivalents). No accepted request is dropped silently.

| Check | Kind |
|---|---|
| Budgets against caps; response codes under overload | Continuous |
| Reserve and release never underflow or exceed the cap | Proof (Kani), Shuttle |

### I6. Project isolation, including the workspace identity rule

No response to a principal of project A contains data of project B, and no response contains a marker row.

- Behavioural: every plan type runs as project A while project B holds near-identical data (including shared
  trace ids); no B row appears.
- Structural: every rendered statement except `BatchProbe` carries the project predicate and the window predicate
  and contains only bound values; `BatchProbe` returns only a boolean. `ProjectScope` cannot be built outside
  `crucible-auth` and cannot hold the nil UUID.
- **Workspace identity rule**: SQL workspace text executes only under the ClickHouse identity of the requesting
  project. The "project predicate in every statement" property does not apply to workspace text; the `sqliso`
  suite replaces it there.
- Service identities see every row after access migrations, under both values of the server's row-policy switch.

| Check | Kind |
|---|---|
| Cross-project reads through every plan type and endpoint; cursors, payload links and export downloads bound to the project | Continuous |
| Predicate presence and bound-values-only on rendered SQL | Property test |
| The scripted `SqlWorkspaceStore` records the identity of every run; it must equal the request's scope | Continuous |
| `sqliso` layers A (W1 to W15), B (fuzzing with canary oracles), C (through the API), D (mutants) | Conformance |
| "Service identities see all rows after access migrations" | Conformance, Runtime (startup) |
| Runtime self-probe disables the workspace on any unexpected success | Runtime |

### I7. Aggregates match the reference

Counts, error counts, token sums and cost sums (integer nano-USD) are exact against the oracle. Token and cost
sums follow the sums rule of contract 3.1 in every aggregating table and on every read path: the oracle counts
every span whose `usage_aggregate` is false, applying the same descendant rule as stage B (an observed descendant
that carries usage or cost, in the same request or earlier in the LSN stream). Counts include every span. One exception: a
batch whose pairs straddle a crash and a binary upgrade that changed the price table or the normalizer may carry
costs from two versions across its tables (section 5.3 of the contract); a metric counts such batches, and the
oracle compares each pair against the version it was built with. Percentiles equal
the oracle histogram's answer exactly in simulation (one histogram implementation), and are within 1/32 (3.125%)
of the exact quantile on real data. Distinct counts are within the error stated by C14.

| Check | Kind |
|---|---|
| Rollup and core-tier aggregates against the reference aggregation | Quiescence |
| Histogram bucket function: total, monotone, value inside its bucket | Proof (Kani) |
| C14 histogram and distinct-count error; "sumMap in SimpleAggregateFunction" | Conformance |

### I8. MCP error classification

Every simulated MCP exchange is classified exactly as the shared classifier table says, over era, JSON-RPC code,
`isError`, HTTP status, cancel source, timeout, transport failure and SDK annotations; `caller_fault` follows the
caller-fault rule.

| Check | Kind |
|---|---|
| Classification of every simulated exchange against the table | Continuous |
| Classifier totality | Proof (Kani) |
| Golden `McpOpRecord`s for four SDKs and the proxy | SDK conformance suite |

### I9. Retention and deletion are complete, including media and the SQL path

After a deletion request or a retention expiry:

- at once on the accepting node, and within 5 s on every other node, no typed read path returns the data;
- after the request reaches `completed` (or the retention pass and mask cycle have run) and the system is
  quiescent, none of these holds the data: `span_core`, `span_payload`, `trace_heads`, `mcp_ops`, `mcp_content`,
  the four fact tables, `trace_stats`, `session_rollup`, `user_rollup`, Postgres comments, un-shipped WAL frames,
  retained and quarantined WAL segments (after their 24 h cap), the recent-span index, export files created before
  the request, **media references and objects**;
- **SQL path**: rows past retention are invisible to the workspace immediately (row policy); a deletion is
  invisible to the workspace from the moment the masking delete is issued;
- a replay, requeue or drain never re-inserts deleted data;
- data received at or after `requested_at` is kept, except an exact repeat of a deleted span that the duplicate
  window still remembers, which is dropped as a repeat (contract 13.3);
- a **project deletion** selects every row of its project whatever its arrival: once it reaches `completed`, no
  table of the project holds a row of it that was there when the purge ran (the minute rollups, the hourly tier,
  `mcp_deploys` and `mcp_tool_inventory` included), and its control-plane rows are gone. **A row that lands later
  goes eventually**: a frame a node took after the deletion while its key snapshot lagged or under I10's fail-open
  allowance, or took before it and shipped after the purge, is deleted within one sweep interval of landing (an
  hour, for 30 days after the deletion; a day after that) and removed from storage after the next mask cycle.
  Rows that keep landing never fail the deletion, and a deletion that failed for good finishes once no table holds
  a row of the project and each of its deletes is removed from storage. Every other project keeps every row, also a
  row it writes while the deletion runs: a partition is dropped only when it ends before every project, live or made
  later, can write (`ingest.max_event_age` and a clock margin before the database clock, and before the making of
  the oldest live project), every live node's clock is within half that margin of the database clock, and every
  project with rows there is deleted; everywhere else the purge and the sweep delete by the deleted project's id. A
  project deleted before deletions made their project deletion request gets one, once no live node runs a version
  that cannot read it.

Minute rollups keep their counts by contract, unless the whole project is deleted.

| Check | Kind |
|---|---|
| Hidden-on-read after every accepted request | Continuous |
| Completeness across the reach list, including the media model, after replay and restart | Quiescence |
| The write path: no row that a completed request selects lands after the request completed, whether the request's tombstone is in the node's log or it completed while the node was down; nothing that no request selects goes (system simulation, dimension `deletions`) | Continuous and quiescence |
| What stage B drops for a deletion, against a linear model; dropping leaves stage B's state alone | Property |
| Project deletion: every table of the project empty whatever the rows' arrival, its control-plane rows gone, the other projects' rows kept in the partitions it dropped or deleted from, late rows of a live project included; a project deleted without a request gets one (lifecycle simulation and its directed scenarios over the real WAL and shipper) | Quiescence |
| Project deletion, eventual: a row of the project that lands after its request completed is deleted within one sweep interval and gone at the end (lifecycle simulation and its directed scenarios) | Continuous and quiescence |
| No project writes before the drop horizon, however late its spans ship (property, `crucible-lifecycle`) | Property |
| Project deletion under failures: a project whose probes fail holds back no other and no daily pass; a refusing object store holds the request at `verified` with its error; rows that keep landing never fail it nor pile up purges; a failed deletion keeps its reason and finishes once every table is empty and each of its deletes is removed from storage; a node registered as 0.1.3 holds the request; a node whose clock is more than half the margin off turns drops into deletes (directed scenarios) | Quiescence |
| C9 delete visibility; "mask application" (`APPLY DELETED MASK`, `DELETE ... IN PARTITION`); no masked parts within the cycle | Conformance |
| W11 lifecycle races (retention change, policy rewrite) | Conformance (`sqliso`) |
| Retention boundary arithmetic | Proof (Kani) |

### I10. Revocation, restated for fail-open

- A node rejects a revoked key from the first key-feed poll it completes at or after the revocation commit.
- A node that can reach Postgres rejects the key within 5 s of the commit.
- A node partitioned from Postgres in `fail_open` mode may accept the key until connectivity returns plus 5 s
  (or until `auth.fail_open_max`, when set). In `fail_closed` mode it rejects every secret key after 5 s without a
  successful poll.
- A node that starts without reaching Postgres accepts nothing.
- A revocation made through a node applies on that node before the response returns.
- Out-of-order feed commits never cause a missed revocation.

| Check | Kind |
|---|---|
| Acceptance times against revocation commits under partition, restart, failover and out-of-order feed commits | Continuous |
| Mutant "ignore the overlap window" is caught | Simulation |
| Feed ordering and commit-unknown behaviour on real Postgres | Conformance |

### I11. Proxy fail-open with byte-identical passthrough

With the Crucible sink down, slow or erroring, the bytes exchanged between the MCP client and the server behind
the proxy are identical to a run with no telemetry, for every chunking, and added latency stays under the bound.
The proxy never writes non-protocol bytes to stdout and never injects content.

| Check | Kind |
|---|---|
| Byte identity over seeded chunk splitting with `ProxyCore` hosted in a deterministic simulation (`crucible-proxy` tests, 10,000 seeds) | Continuous |
| Real processes: collector killed or stalled, huge lines, signals and exit codes, overhead at or under 1 ms p99 | Process tests |

### I12. Recovery is idempotent and bounded

A crash at any point during recovery followed by another recovery yields the state a single uninterrupted
recovery yields. Recovery work is bounded by the retained WAL size.

| Check | Kind |
|---|---|
| Crash at every barrier, including inside recovery; state digests compared | Continuous, Quiescence |
| Fuzzing of segment recovery | Fuzz |

### I13. WAL space is reclaimed

At quiescence every segment that lies wholly at or below the checkpoint watermark, is outside replay retention and
is not referenced by the recent-span index has been removed. Nothing above the watermark is ever removed.

| Check | Kind |
|---|---|
| Segment set at quiescence | Quiescence |
| Liveness: every covered segment is eventually truncated; `TruncateOnlyBelowCheckpoint` | Proof (Quint) |

### I14. Determinism

One seed on one commit, lockfile and container image gives one trace hash.

| Check | Kind |
|---|---|
| The same seed run twice gives identical hashes; two seeds differ | Meta-test on every pull request |

## Conformance rows that the contract names

| Row | Establishes |
|---|---|
| C1 | One token retried within the window yields one copy, on every engine variant |
| C2 | A plain table deduplicates only with `non_replicated_deduplication_window` set; the DDL sets it |
| C3 | Deduplication logs are bounded; beyond them the probe is what prevents duplicates |
| C4 | The token overrides the content hash; a retry need not be byte-identical |
| C9 | A lightweight delete hides rows from every later SELECT at once |
| C11 | Read-after-write on the inserting replica; cross-replica lag |
| C12 | The error codes used for classification are stable across the version matrix |
| C13 | Cancel-on-close for read-only queries, including POSTed SELECTs |
| C14 | Histogram error at most 1/32; `uniqCombined64` error |
| C5 to C8, C10 | Guard checks only: Crucible's explicit settings (no materialized views, no asynchronous inserts, one partition per block, no TTL) avoid these paths |
| Shared-engine dedup settings | The Cloud engines honour the replicated window settings |
| Mask application | `APPLY DELETED MASK [IN PARTITION]` and `DELETE ... IN PARTITION` on 26.3 and Cloud |
| Single Native block not re-split | A block of 262,144 rows yields one part and one dedup entry |
| sumMap in SimpleAggregateFunction | The histogram column type is accepted and merges by integer addition |
| Service identities see all rows after access migrations | Section 7.3 of the contract, under both values of the row-policy switch |
| `sqliso` W1 to W15 | SQL workspace isolation |
