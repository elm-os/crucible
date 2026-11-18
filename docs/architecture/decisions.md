# Decision log

Short records of the load-bearing choices behind [`contracts.md`](contracts.md) (contract v2). Each entry is the
decision and its main reason. Entries marked "reconciled" settle a point on which the track designs disagreed.

## Write path and durability

1. **Ack only after a group-commit fsync to a local WAL.** One append and one `sync_data` per group; the only
   durability mode under which no-acked-loss is claimed.
2. **The WAL stores the canonical raw request.** Normalization runs after the ack as a versioned pure function, so
   pre-ack work is decode and validate only, and a normalizer fix can be replayed.
3. **Portable WAL backend by default; io_uring is optional.** The ring backend sits behind the `Disk` trait and a
   cargo feature, and becomes the default only if benchmarks show a gain.
4. **LZ4 WAL frames, stored uncompressed when compression does not help.** With a 64 KiB envelope reserve this
   keeps every request inside the 64 MiB frame cap. Reconciled: the request cap is stated as 64 MiB minus the
   reserve.
5. **Every frame consumes one LSN and an LSN gap is fatal.** A torn tail truncates; a discontinuity never does.
6. **Batch identity comes from WAL positions: `batch_seq = first_lsn`.** Reconciled: this replaces a separate
   batch counter.
7. **Token `{wal_epoch}:{first_lsn}-{last_lsn}:{table_id}:{partition_id}`.** Reconciled: the earlier
   per-(batch, table) token gained the LSN range and the partition.
8. **Single-partition inserts, one Native block of at most 262,144 rows per (batch, table, partition).** Removes
   cross-partition atomicity and server-side block splitting from the protocol.
9. **Own Native encoder with LZ4 framing.** The client library's native insert path cannot compress and lacks
   several column types; own bytes also make retries byte-identical.
10. **Explicit settings on every insert and every query.** Server and service defaults differ across versions and
    on Cloud; nothing is left to them.
11. **Probe before any retry with an unknown outcome; the dedup window covers only the in-flight race.** Plain
    tables probe by `batch_seq`; aggregating tables by a nil-project marker row.
12. **Marker rows are keyed by two trailing sort-key columns (`marker_epoch`, `marker_batch_seq`).** A uniform
    rule for every aggregating table, so a marker never merges away.
13. **The checkpoint is a low watermark.** Reconciled: the per-table bitmap stays in the frame for format
    stability and is all ones.
14. **Poison batches are quarantined, never dropped.** A quarantine counts as durable; `crucible wal requeue`
    re-ships with the original identity.
15. **Replay retention is byte-budgeted (15 min default, 24 h cap).** A time-only window is not viable at the
    target ingest rate, and the cap bounds raw copies for the deletion SLA.

## Storage

16. **No materialized views, projections or table TTL; the shipper writes every derived table.** One insert
    protocol and one model cover base and derived data. Reconciled: derived sets were first sketched as views.
17. **Immutable rows; later-written facts are separate append-only tables resolved latest-wins in Rust.** No
    `FINAL`, no updates.
18. **Two tiers: `span_core` and `span_payload`.** Lists and aggregates never touch large values.
19. **Deterministic integer log-linear histogram for latency.** Mergeable by integer addition, encodable from
    Rust, identical in the model and production. The exact error bound is 1/32 (3.125%).
20. **Cost is `Int64` nano-USD.** Exact sums; the wire carries a USD number.
21. **Root-anchored trace list (`trace_heads`) with provisional heads, plus mergeable `trace_stats`.** An immutable
    sort key gives keyset pagination without a window-wide GROUP BY.
22. **`trace_stats` is partitioned by its `row_day` key column.** Reconciled: a partition expression over an
    aggregated column was replaced by the key column that carries the same day.
23. **Trace-level attributes are never backfilled; the trace value is a rank-prefixed byte-wise `max`.**
    Keeps rows immutable and the pick mergeable.
24. **Fact tables are named `fact_scores`, `fact_tags`, `fact_flags`, `fact_corrections`.** Reconciled: one set of
    names for records and tables.
25. **`mcp_tool_rollup` includes `capture_source` in its key.** Reconciled: required by the double-capture rule.
26. **`mcp_deploys` is a small aggregating table.** Reconciled: gives `mcp/deploys` and `mcp/compare` a backing
    set without adding `server_version` to the rollup.
27. **Daily partitions by default, `week` as a provisioning option; clustered self-host is one shard with N
    replicas.**
28. **`SourceFormat` and `IoShape` carry no product names.** Reconciled: variants that named instrumentation
    libraries were replaced by `compat` plus the dialect id, and by `indexed_messages`.

## ClickHouse access

29. **One identity scheme: `crucible_admin`, `crucible_ingest`, `crucible_read_ui`, `crucible_read_agent`,
    `crucible_maint`, and `crucible_sql_<project hex>`.** Reconciled: the single read identity was split by
    workload; DDL and access management stay with the operator-supplied admin; all runtime maintenance and
    `KILL QUERY` go to `crucible_maint`; the ingest identity gains column-level SELECT for the probe.
30. **Service `USING 1` policies are created before any workspace policy, one per table.** Under the strict
    server setting a missing policy would turn a probe into a re-insert or a UI read into an empty page.
31. **Workspace isolation is carried by ClickHouse alone: per-project users, two policy layers, each PERMISSIVE
    and RESTRICTIVE.** Correct under both values of the server's row-policy switch.
32. **The static policy also excludes the nil project.** A malformed user name must not match marker rows.
33. **Typed query plans are the only read path, with the SQL workspace as the one exception.** Plans make
    isolation and windows structural and let the model interpret reads; the workspace has its own trait, suite and
    runtime probe.
34. **`single` identity mode as a fallback.** When the operator's admin lacks access management, everything but
    the workspace still works.

## Read side and API

35. **The recent-span index gives read-after-write; gaps are explicit.** `partial: true` and
    `consistency: lagging` replace silent misses.
36. **`BatchProbe` is the only statement without a project predicate.** It returns a boolean and is constructible
    only in the shipper.
37. **Entity routes live under `projects/{id}/`.** Reconciled: project-less entity paths were ambiguous for users
    with several projects.
38. **One `auth/oauth/{provider_id}` route family for all provider kinds.** Reconciled: separate OIDC routes were
    dropped.
39. **`filter-values` is the single distinct-values endpoint.** Reconciled: it replaces a separate dimension-values
    route; both read one registry.
40. **Starter dashboards are served by the API from code.** Reconciled: not shipped in the UI bundle.
41. **Problem field errors use JSON pointers.** Reconciled: `errors[].pointer`, not a field name.
42. **Health: `/healthz` and `/readyz` at the root, admin detail at `/api/v1/health`.** Reconciled.
43. **Debug fault header is `X-Crucible-Test-Fault`.** Reconciled: one name.
44. **Data bounds are cached for 30 s.** Reconciled between 30 s and 60 s.
45. **Large traces page by parent.** Above 5,000 spans the detail returns roots and their children, and a
    children route pages the rest.
46. **Code-first OpenAPI 3.1 and ts-rs from one struct; no `bigint` on the wire.**
47. **Static UI export embedded in the binary.** Auth, routing fallbacks and CSP live in Rust; entity ids travel
    in search params.

## MCP

48. **A span is an MCP operation if and only if it has `mcp.method.name`.** Plain semantic-convention spans work
    without a wrapper.
49. **Double capture is resolved at read time with a `source` parameter, automatic preference SDK, then proxy,
    then OTLP.** Sources are never summed.
50. **Multi-round correlation uses keyed hashes of the request state (state-in, state-out).** Reconciled: a
    minted correlation id is impossible for a wrapper that does not own the state.
51. **`deploy_id` defaults to `server_version`; `handle_id` comes only from `identify` or a named argument.**
52. **User subjects are hashed server-side with the per-project salt; the issuer is kept.** Delete-by-user works
    with a raw id.
53. **`progress_count` is optional and `identity_source` is recorded.** Reconciled: "unobserved" and "unverified"
    need a representation.
54. **MCP endpoints that need user, session, client-version, subclass or code dimensions read `mcp_ops`.**
    Reconciled: only rollup-backed endpoints carry the 300 ms target.
55. **The mapping engine is one pure `map` over dialect tables, wrapped by `SpanMapper` and `McpProjector`.**
    Reconciled: the storage-side traits and the MCP-side function are one mechanism.
56. **The proxy ships OTLP; the `NativeBatch` frame type is reserved.**

## Control plane and operations

57. **Leases with fencing tokens for leader work, not session advisory locks.** Works behind transaction poolers
    and gives a token for compare-and-set writes.
58. **API keys are verified from an in-memory snapshot fed by a polled, versioned key feed.**
59. **Ingest fails open during a Postgres outage; UI sessions fail closed.** Reconciled: the earlier proposal to
    fail closed after a grace period is the `fail_closed` option, not the default. A cold start accepts nothing.
60. **Repository-shaped `ControlStore` on a client whose sockets go through `Net`.** The control plane is
    simulation-testable.
61. **`Clock::wall_now` returns microseconds.** Reconciled with the one internal time unit.
62. **Deletion is a Postgres request plus a WAL tombstone; the executor waits for every live node's checkpoint.**
    Reconciled: SQL visibility of a deletion within 60 s is a target, not a bound.
63. **Retention by partition drop where possible, else lightweight delete plus a 6 h mask cycle.**
64. **Sign-up is invite-only with optional allowed email domains; `project_access` defaults to `all`.**
65. **Media is optional and goes through presigned URLs with hash-keyed deduplication.**

## Verification

66. **Determinism is an architecture rule: every source of nondeterminism is an `Env` trait.** The shipped code
    is the simulated code.
67. **In-process models may be stricter than the real systems, never more permissive; conformance decides.**
68. **Contracts change only by a pull request that updates `contracts.md` first.**

## Crate boundaries

69. **`ProjectScope` lives in `crucible-records`; minting is restricted by lint, not by visibility.** A leaf crate
    breaks the cycle that `crucible-env` (store traits) to `crucible-plan` (plans hold a scope) to `crucible-auth`
    (minting) to `crucible-env` would form. `clippy.toml` bans `ProjectScope::mint` outside
    `crates/crucible-auth/src/scope.rs`, and the CI guard confines the allow to that file.
70. **The internal dependency graph is part of the contract (section 1.5).** A new edge needs a contract change.
71. **Shared primitives live in `crucible-records`:** ids, `Timestamp`, `Lsn`, `TableId`, `PartitionId`, `BatchToken`,
    the frozen enums and the deterministic collections. The batch-token injectivity proof moves there with the type.
72. **Hash collections use a fixed hasher (`DetMap`, `DetSet`).** Randomly seeded constructors are banned in
    `clippy.toml`, because iteration order would differ between runs.
73. **The simulated `Env` lives in `crucible-sim` and shares `crucible-env-prod`'s exemption from the determinism
    lints.** Both crates implement the traits over real primitives; product crates never get the exemption.
74. **I1 is guaranteed on Linux and macOS; Windows is a development platform.** Windows has no directory sync
    barrier through the standard library, and `unsafe` is denied.
75. **Prices are pico-USD per unit.** Whole nano-USD per unit cannot express common fractional prices; cost stays
    exact nano-USD with half-even rounding.
76. **Native block headers keep `SimpleAggregateFunction` wrappers.** ClickHouse accepts both forms; sending the
    column's own type means the server converts nothing.
77. **Source keys follow the semantic conventions where they exist, `crucible.*` otherwise (contracts 10.6).** They
    are wire contract from milestone 3, when the SDKs start emitting them.
78. **An un-normalizable frame is quarantined, not fatal.** Shipping continues; the frame's copy keeps I1, and a
    newer binary can ship it.
79. **One fact comparator, in Rust.** ClickHouse orders UUIDs differently, so no SQL ordering of UUIDs decides a
    fact winner.
80. **Replays after an upgrade use the current price table and normalizer**, with a stated I7 exception for batches
    that straddle the upgrade.
81. **Security never at the cost of a smooth self-hosted default (contracts 1.7).** Protections that cost nothing are
    always on; friction-adding hardening (Secure-only cookies, private-network egress bans, fail-closed sessions
    during a Postgres outage, a mandatory public URL) is an operator switch. Set after the owner asked that security
    not degrade the experience.
82. **Configuration loads TOML, then `CRUCIBLE_*` variables, then flags, in `crucible-config` alone.** Unknown file
    keys are errors naming the nearest key, every problem is reported in one run, and secrets never print. Unknown
    `CRUCIBLE_*` variables only warn.
83. **The control store is optional.** A node without Postgres still ingests, stores and serves telemetry; platform
    features (sessions, keys in Postgres, alerts, dashboards, the SQL workspace) require it.
85. **Dialect tables are layers in the contracts 10.3 order, and every span is mapped with all of them.** The
    chosen dialect decides only `compat_dialect`, which bare-key (`own`) sources are live, and the value
    encoding. This is what the field priority already described; it needs no duplicated rows and stops the data
    loss on spans that one dialect's exporter forwards for another. One key-to-sources index is built at start-up
    so the hot path stays flat.
86. **A request hint is a detection at confidence 60, not an override.** A span that another dialect's own scope
    name or exclusive key identifies keeps that dialect on a hinted path, so its bare keys still map and it is
    labelled with the dialect that produced it. Namespaced keys map either way, because of the layers.
87. **Token and cost sums leave out only spans whose usage repeats their descendants' (contracts 3.1).** Several
    instrumentations repeat their children's usage on a parent span. A span is marked `usage_aggregate` when a
    descendant that carries usage or cost has been observed, in the same request or earlier in the LSN stream:
    stage B keeps, per trace, the span ids that have such a descendant (children are exported no later than their
    parents), bounded and rebuilt from the WAL like the duplicate window. There is no rule by observation type, so a
    hand-instrumented span of any type that carries a provided cost is never dropped from totals. The span still
    stores and shows its own usage. Known limit: a descendant that arrives after its ancestor, or after the window
    forgot the trace, is counted twice; the I7 oracle models the same rule.
88. **Compat fixtures hold invented values in the exact wire shape of their source.** Same keys, encodings,
    nesting and wire order; no third-party content is carried. Each fixture's `provenance` cites the repository,
    tag, file and lines its shape comes from, the goldens follow the invented values, and `THIRD_PARTY_NOTICES`
    does not change.

