# Architecture overview

This page is the map. The binding details (records, WAL frames, insert protocol, API, schemas) live in
[`contracts.md`](contracts.md).

## Shape

Crucible is one Rust binary, `crucible`, with these roles:

| Role | Where | Notes |
|---|---|---|
| OTLP ingest | `:4318` (HTTP), `:4317` (gRPC) | Protobuf or JSON, gzip; compatibility paths for other SDKs' OTLP traffic |
| REST API | `:8080/api/v1` | Code-first OpenAPI 3.1 |
| MCP server | `:8080/mcp` | Read-only analytics tools for coding agents |
| UI | `:8080/` | Next.js static export embedded in the binary |
| MCP proxy | `crucible proxy stdio -- <cmd>`, `crucible proxy http` | Zero-code MCP instrumentation |
| Metrics | `127.0.0.1:9464/metrics` | Prometheus |

A deployment is the binary plus **Postgres** (control plane) plus **ClickHouse** (telemetry): self-hosted 26.3 LTS
or newer, or ClickHouse Cloud. S3-compatible object storage is optional, for media.

## Write path

1. **Decode and admit.** A zero-copy scanner validates the OTLP payload (caps: 64 MiB per request
   including a 64 KiB envelope reserve, 50,000 spans per request). JSON is transcoded to protobuf. Byte budgets and per-project token buckets answer 429 or 503 with
   `Retry-After` before memory runs out.
2. **WAL append.** The raw request plus an envelope (project, key, compatibility dialect) is appended to a local
   write-ahead log. A group committer issues one append and one `fsync` per group.
3. **Ack.** The client is acknowledged only after its frame is durable.
4. **Normalize.** A versioned pure function maps each span to a `SpanRecord` and, for MCP operations, an
   `McpOpRecord`. It computes cost from a versioned price table and never estimates tokens.
5. **Index.** The in-memory recent-span index makes a span readable within about 100 ms of its ack.
6. **Seal and ship.** The shipper seals a batch (`BatchSeal` in the WAL), encodes one Native block per table and
   partition, and inserts it with a deterministic deduplication token. Unknown outcomes are probed before any retry.
7. **Checkpoint.** A low-watermark `Checkpoint` record lets fully shipped WAL segments be deleted.

The optional `wal-uring` backend uses io_uring for WAL writes on Linux when the kernel and seccomp policy allow it,
and falls back to the portable backend otherwise.

## Storage

- **Immutable rows.** Spans are written once. Facts written later (feedback, tags, flags, corrections) go to separate
  append-only tables and are resolved latest-wins in the Rust tier. No query uses `FINAL`.
- **Two tiers.** `span_core` holds narrow columns and previews for lists and aggregates; `span_payload` holds full
  inputs, outputs and attributes for point reads and full-text search.
- **Derived tables written by the shipper.** `trace_heads`, `trace_stats`, session and user rollups, span metrics and
  the MCP tool rollup. Latency percentiles use a deterministic integer histogram.
- **One schema, three variants.** Single node (MergeTree), clustered (Replicated, one shard), and ClickHouse Cloud,
  rendered from one schema description.

## Read path

Every telemetry read is a typed query plan: a scope (project), a time window, a query class with resource limits,
and a tree of nodes. The renderer turns plans into parameterized SQL. The same plans run against an in-memory model
of ClickHouse during simulation. The SQL workspace is the one exception: it runs user SQL under a per-project
ClickHouse identity whose row policies enforce isolation.

## Control plane

Postgres holds the organization, users, sessions, sign-in providers, projects and roles, API keys, alerts and
channels, dashboards, saved SQL queries, retention settings, deletion requests and comments. API keys are verified
from an in-memory snapshot; revocations propagate within 5 seconds.

## MCP module

- SDK wrappers for TypeScript, Python, Go and Rust capture every MCP operation with timing, outcome, error class,
  client and session, and opt-in arguments and results scrubbed in the SDK.
- The proxy forwards bytes unchanged and analyses a copy, so a Crucible outage never breaks an MCP session.
- Agents propagate trace context in `params._meta`, which links their tool calls to server-side spans.

## Crate map

| Area | Crates |
|---|---|
| Environment | `crucible-env`, `crucible-env-prod` |
| Shared types | `crucible-records` |
| Ingest and WAL | `crucible-otlp`, `crucible-admission`, `crucible-ingest`, `crucible-wal-codec`, `crucible-wal` |
| Normalize and ship | `crucible-normalize`, `crucible-compat`, `crucible-schema`, `crucible-chfmt`, `crucible-shipper`, `crucible-store-clickhouse` |
| Read | `crucible-plan`, `crucible-recent`, `crucible-query`, `crucible-lifecycle` |
| Platform | `crucible-config`, `crucible-auth`, `crucible-control`, `crucible-control-pg`, `crucible-api`, `crucible-api-types`, `crucible-cursor`, `crucible-jobs`, `crucible-alerts`, `crucible-notify`, `crucible-egress`, `crucible-sqlws`, `crucible-media`, `crucible-ui-assets` |
| MCP | `crucible-proxy`, `crucible-mcp-server` |
| Verification | `crucible-sim`, `crucible-conformance`, `crucible-loadgen`, `crucible-fake-collector` |
| Binary | `crucible` |
