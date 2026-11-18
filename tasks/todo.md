# Crucible plan and progress

Each item is `[step] -> verify: [check]`. Milestones end when every exit check is green.

## Step 0: environment and bootstrap

- [x] Preserve research and design notes outside the repository -> verify: all files present and non-empty.
- [x] Reference source vendored outside the repository, without the separately licensed directories, and graphed
      -> verify: no such directory in the copy; graph queries locate the ingestion processor and price matcher.
- [x] Dev container image `crucible-dev:1` (Rust 1.100.0, rustfmt, clippy, nightly, nextest, cargo-deny)
      -> verify: `cargo --version`, `cargo nextest --version`, `cargo deny --version` inside it.
- [x] Repository created with license, notices, rules, policy checker, hooks, CI, docs skeleton
      -> verify: CI green on the first commit; a planted naming violation and a planted AI trailer are rejected.
- [x] Skills installed (`ux-guidelines`, `ui-sdk`); graphify set up for this repo -> verify: skills listed; graph builds.

## M1: walking skeleton

- [x] Crate skeletons for every M1 crate and one pinned `[workspace.dependencies]` table -> verify: workspace builds; clippy clean.
- [x] Formal tools image `crucible-dev-formal:1` (Kani 0.68.0, Quint 0.33.1, cargo-fuzz 0.13.2, Java 21) -> verify: versions print.

- [x] M1.0 Freeze contracts v2 in `docs/architecture/contracts.md` (reconcile the MCP rollup key with
      `capture_source`, a backing set for `mcp/deploys`, one ClickHouse identity scheme with `USING 1` policies for
      every service identity before any workspace policy, owners for the UI's extra API fields, the compat interface)
      -> verify: doc merged (#1); conformance row "service identities see all rows after access migrations" planned in
      `docs/testing/invariants.md`.
      Owners of the UI's extra API fields (contracts 8.8; every route in `crucible-api`, every DTO in
      `crucible-api-types`):
      - `span_count` and paged span children above 5,000 spans: `crucible-plan`, `crucible-query`, `crucible-recent`
      - live-tail `since`: `crucible-plan`, `crucible-recent`, `crucible-query`, `crucible-cursor`
      - `partial: true` and `consistency: lagging`: `crucible-recent` (gap, barrier), `crucible-query` (meta)
      - `data-bounds`: `crucible-query`, `crucible-plan`, `crucible-recent`
      - filter-values lookup: `crucible-plan` (registry), `crucible-query`
      - metrics schema and caps: `crucible-plan` (registry and compiler), `crucible-query`
- [x] A0 foundation: shared primitives in `crucible-records`, `ProjectScope` minting confined to `crucible-auth`,
      every M1 crate's dependencies wired once, internal dependency edges in contracts 1.5
      -> verify: clippy clean with the bans; 26 tests pass; `cargo deny` passes; a planted `ProjectScope::mint` call in
      `crucible-plan` is rejected; Kani, cargo-fuzz and Quint smoke runs pass on the workspace.
- [x] `crucible-env` and `crucible-env-prod`; `clippy.toml` bans with the allow-attribute grep
      -> verify: a banned call fails CI; the hello-world app serves `/healthz` on the real runtime. Done in #5: the
      `bans` workflow proves all 52 bans fire; `crucible serve` answered `/healthz`, `/readyz` and `/metrics` and
      shut down cleanly on SIGTERM.
- [x] `crucible-wal-codec` with proptest and Kani harnesses -> verify: five proofs pass; 1M random frames round-trip.
      Done in #6: 10 Kani harnesses (obligation 4 by bounded lemmas, contracts 4.10), 1M frames in 5.6 s, fuzzing
      found nothing in 24M executions.
- [x] Portable WAL with group commit, recovery and GC -> verify: torn-tail and fsync-lie DST seeds keep I1. Done in #16:
      61,000 long-run seeds with zero acked loss; the stress tier's fsync lie is detected; three mutants caught.
- [x] Minimal OTLP HTTP ingest, admission, normalizer stub -> verify: proto and JSON inputs map identically. Done in
      #11 (scanner and admission), #17 (normalizer, proto equals JSON over 1,000 cases) and #23 (ingest: the ack waits
      for durability, and an ack-before-sync mutant is caught).
- [x] Shipper (seal, token, probe, checkpoint) and the ClickHouse adapter with the own Native encoder
      -> verify: the same token three times yields one copy on single node and replicated; a two-partition block is
         rejected. Done in #24 (shipper: 5,000 seeds, six mutants caught) and #20 (adapter: C1 on single node and on a
         one-shard, two-replica cluster; the two-partition block is rejected as poison).
- [x] `crucible-schema` DDL-as-data for `span_core`, `span_payload`, `trace_heads`, `trace_stats`
      -> verify: golden SQL for three variants; `SHOW CREATE TABLE` matches on 26.3, 26.8, latest. Done in #8 for all
      18 tables, plus access step A1 and the service-identity row under both server switch values and on a cluster.
- [x] `crucible-records` record types and kernels (histogram, MCP error classifier, batch token) with Kani proofs (#10).
- [x] `crucible-chfmt` Native encoder and framing, byte-identical to ClickHouse 26.3, 26.8 and 26.10 output (#9).
- [x] Typed plan IR, trace-by-id and trace list; recent-span index with read barrier -> verify: I3 in DST. Plan IR #8,
      recent index and read services #18, I3 checked after every ack and restart in the system simulation (#30).
- [x] Simulation harness (turmoil, SimDisk, ChModel, oracle, I1-I3, I12-I14) and the swarm config generator
      -> verify: seeded mutants (skip sync, drop token, truncate early) caught within 200 configs; same seed, same
         trace hash. Done in #30: 41 swarm dimensions, 612 runs per PR across four shards, six mutants caught, logs
         byte-identical across thread counts, ChModel equal to ClickHouse on 184 plans per version. Single node, so no
         turmoil yet.
- [x] Quint spec of the ingest protocol -> verify: safety properties hold; an ack-before-sync variant is caught. Done
      in #13: five mutants caught; the model's three findings are in the contract (#15).
- [x] Conformance C1-C4, startup dedup self-test, single-block rows -> verify: green on 26.3, 26.8, latest. Done in
      #20: 36 of 36 rows single node and 21 of 21 on a one-shard, two-replica cluster; `conformance` workflow in CI.
- [x] fsync-honesty probe and load generator baseline -> verify: probe classification recorded. Done in #21: the
      Docker volume classifies honest and tmpfs suspicious; the generator holds 50k requests/s within 0.0002%.
- [x] UI scaffold in `ui/`: pinned Next 16.4.6 static export, tokens and fonts, shell with UI SDK navigation, project
      placeholder and the one global time range -> verify: typecheck, Oxlint, Biome and `next build` green; `ui/out`
      has one `index.html` per route.
- [x] UI data layer: typed `/api/v1` client, problem+json mapper, TanStack Query provider, four-state `Section`
      -> verify: Vitest green (mapper rows, range hook, no-backend shapes, loading ladder, agent test); headless
      Chrome shows the designed error state on `/traces/` with no backend.
- [x] One UI table in the embedded static export -> verify: the binary serves it with the try-files contract. Done in
      #19 (serving), #26 (API and table) and #29 (end to end: the embedded UI shows real traces through the temporary
      Connect dialog).
- [x] End to end on real infrastructure (#29): freshness 13 to 28 ms from ack to the API; 100,000 acked spans across a
      20,000-span burst, two `kill -9` rounds and a SIGTERM under load, with zero lost and zero duplicated.
- [x] Close M1 on main: conformance, e2e, sim and quint dispatched on main's head and green; the release binary
      carries no test-only features (CI guard).
- [x] TypeScript MCP seam spike (3 days) -> verify: the five exit criteria or a recorded fallback. All five pass on SDK
      2.0.0 to 2.4.0 (#3); decision in `docs/architecture/mcp-typescript-seam.md`.
- [x] CI for the UI (`ci-ui`: install, Biome, Oxlint, typecheck, Vitest, static export) -> verify: green on its PR.

## M2: core observability

- [ ] Full span model, GenAI semantic-convention mapping, compatibility mappers, price table, gRPC ingest.
- [ ] Facts tables and decoration; trace detail (tree, timeline, agent graph); spans, sessions, users; full-text search.
- [ ] Rollups, cursors, guardrail classes; OpenAPI and ts-rs pipelines.
- [ ] UI screens with all four states -> verify: Playwright surface table; I4-I7 green; perf baseline recorded.

## M3: MCP module

- [ ] MCP operation records and tool inventory; analytics endpoints including deploys and compare.
- [ ] SDKs: TypeScript, Python, Go, Rust (agent helpers + MCP wrappers, both protocol eras).
- [ ] Proxy (stdio, HTTP), agent to server linking, SDK conformance suite and MCP wrapper matrix
      -> verify: I8, I11 green; all four SDKs pass; proxy overhead within 1 ms p99 on the laptop rig.

## M4: platform

- [ ] Postgres control plane, auth (password, GitHub, Google, OIDC), roles, API keys with fail-open cache.
- [ ] Alerts (Slack, email, webhook), dashboards and the metrics catalogue, SQL workspace with `sqliso` suite,
      optional S3 media, retention and deletion executor, exports -> verify: I9, I10 green; Playwright journeys.

## M5: integrations and docs

- [ ] Crucible MCP server; rightmodeler `crucible` adapter; rightmodeler site and positioning update.
- [ ] Documentation: self-hosting (OSS and Cloud), SDK and MCP guides, API reference, integrations, migration,
      comparison -> verify: rightmodeler reads an export with zero dropped records; naming check green.

## Performance work, scheduled by trigger

Each item's first commit is a check that fails on main, bounded by work counts rather than wall-clock time; then the
change; then the validation named here. Ids are stable.

Now (touches none of the single-writer files):

- [ ] SPD-03: a capture-overhead scenario in the shared SDK suite, with worst-case shapes and work bounds
      -> verify: the scenario fails on main for the scrubbing and serialization shapes, in every SDK.
- [ ] SPD-06: scrub captured text in time linear in its length (TypeScript, Python, Go, Rust, and the fake
      collector's scrubber) -> verify: SPD-03's token-run shapes fall to a few substring searches.
- [ ] SPD-07: serialize no more of a captured value than the 16 KiB cap needs -> verify: SPD-03's large-result
      shape no longer grows with the result.
- [ ] SPD-13: bound the SDK export queues by bytes as well as by spans (contract 9.10 wording from the lead)
      -> verify: a stalled-collector test holds the queue under its byte ceiling.
- [ ] SPD-08: keep idle store connections as long as the server keeps them, detecting the server's keep-alive at
      start-up and staying under it with a margin -> verify: a connect counter shows reuse after an idle gap; no
      reused dead connection turns an insert into an unknown outcome.
- [ ] SPD-09: size memory-derived defaults from the container's memory limit, with a fallback on Windows
      -> verify: a limited container gets a bound from its limit, not the host's memory.

When the perf gate is on main:

- [ ] SPD-01 (lanes): an end-to-end sustained lane (overload, drain, the zero-loss check), freshness p95 at a fixed
      rate, a gzip lane, varied request shapes, and compression off the generator's runtime workers
      -> verify: an A/A run over 5 pairs passes and a seeded slowdown is flagged.
- [ ] SPD-02: a read-latency bench for trace by id and the 30-day list (the MCP dashboard lane once its endpoints
      exist), with per-request store-read, connection and barrier-wait counters -> verify: p50/p95/p99 per endpoint.
- [ ] SPD-15 (pre-ack items): trim the inflate and block-frame copies, bench first.
- [ ] SPD-04 (first half): one CPU budget for blocking work: permits sized from the core count (with a Kani proof of
      the counter), a blocking-thread ceiling, wait and hold timings, the ingest part -> verify: an A/B result with no
      ack p99 regression.

When the facts slice and SPD-08 are on main:

- [ ] SPD-10: issue a request's independent store reads together, capturing the watermark and `as_of` once before
      the wave -> verify: trace detail in two waves instead of six sequential reads; the read bench.

Exclusive perf window (after MCP storage merges, before the next heavy launch):

- [ ] The fsync probe and a full-rig A/A; the baseline with post-MCP rows per span; SPD-02's large datasets; SPD-14
      (the fat-LTO profile) with the inflate-backend and target-CPU checks as extra arms.

Shipper-throughput window (single writer, after MCP storage):

- [ ] Phase timings on main; SPD-05 (constant-time provisional-head lookup); SPD-11 (wake the shipper when the
      durable LSN moves).
- [ ] Encoding off the shipper task and parallel stage A, under SPD-04's permits (its second half).
- [ ] SPD-15 (WAL and handoff items), bench first.
- [ ] Decide whether the columnar pair builders go in this window or in M6, from the re-measured phase split.

With wave B:

- [ ] SPD-17: a stable per-id offset for scheduled evaluations, in the alert evaluator.
- [ ] Proxy opt-in content capture, after SPD-06, so it scrubs in linear time from the start.

Conditional:

- [ ] SPD-12: bound the work of re-reading one recent span's payload (after MCP storage merges).
- [ ] SPD-16: cache settled time slices of dashboard aggregates, only if SPD-02 shows the dashboard lane over its
      gate.

## M6: v0 hardening and release

- [ ] Perf tuning to the gates; io_uring WAL backend benchmarked; nightly simulation of at least 10,000 seeds green.
- [ ] Packaging (Compose, multi-arch image, binaries for seven targets) and the v0 tag
      -> verify: every v0 exit criterion in `docs/testing/methodology.md` holds.

## Review

### M1 (closed 2026-11-17)

Every M1 exit check is green on main: one span flows end to end under simulation and on real ClickHouse; the seeded
mutants are caught (WAL, shipper, ingest, recent index); the Quint safety properties hold and the model's five mutants
are caught; the TypeScript seam spike met its five exit criteria; conformance passes on ClickHouse 26.3, 26.8 and 26.10,
including a one-shard, two-replica cluster.

What worked: freezing the contract first; the A0 foundation commit, which made the parallel waves merge cleanly;
formal and empirical checks finding real defects before integration (the Quint model's probe rule, the WAL's two
long-run bugs, ClickHouse's grants and UUID order, the data-bounds parameter format found end to end).

Carried into M2 (see `docs/architecture/contracts.md` 8.9 and the decisions log): nullable outputs and `set_merge`;
the recent-index locator ordinal and the environment default; ChModel query-class guardrails (the model is more
permissive than ClickHouse there); frame quarantine; throughput, pending the first baseline.
