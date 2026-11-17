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
- [ ] `crucible-env` and `crucible-env-prod`; `clippy.toml` bans with the allow-attribute grep
      -> verify: a banned call fails CI; the hello-world app serves `/healthz` on the real runtime.
- [ ] `crucible-wal-codec` with proptest and Kani harnesses -> verify: five proofs pass; 1M random frames round-trip.
- [ ] Portable WAL with group commit, recovery and GC -> verify: torn-tail and fsync-lie DST seeds keep I1.
- [ ] Minimal OTLP HTTP ingest, admission, normalizer stub -> verify: proto and JSON inputs map identically.
- [ ] Shipper (seal, token, probe, checkpoint) and the ClickHouse adapter with the own Native encoder
      -> verify: the same token three times yields one copy on single node and replicated; a two-partition block is
         rejected.
- [ ] `crucible-schema` DDL-as-data for `span_core`, `span_payload`, `trace_heads`, `trace_stats`
      -> verify: golden SQL for three variants; `SHOW CREATE TABLE` matches on 26.3, 26.8, latest.
- [ ] Typed plan IR, trace-by-id and trace list; recent-span index with read barrier -> verify: I3 in DST.
- [ ] Simulation harness (turmoil, SimDisk, ChModel, oracle, I1-I3, I12-I14) and the swarm config generator
      -> verify: seeded mutants (skip sync, drop token, truncate early) caught within 200 configs; same seed, same
         trace hash.
- [ ] Quint spec of the ingest protocol -> verify: safety properties hold; an ack-before-sync variant is caught.
- [ ] Conformance C1-C4, startup dedup self-test, single-block rows -> verify: green on 26.3, 26.8, latest.
- [ ] fsync-honesty probe and load generator baseline -> verify: probe classification recorded.
- [ ] One UI table in the embedded static export -> verify: the binary serves it with the try-files contract.
- [ ] TypeScript MCP seam spike (3 days) -> verify: the five exit criteria or a recorded fallback.

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

## M6: v0 hardening and release

- [ ] Perf tuning to the gates; io_uring WAL backend benchmarked; nightly simulation of at least 10,000 seeds green.
- [ ] Packaging (Compose, multi-arch image, binaries for seven targets) and the v0 tag
      -> verify: every v0 exit criterion in `docs/testing/methodology.md` holds.

## Review

(Filled in at each milestone boundary.)
