# Crucible

Open-source, self-hosted observability and analytics for AI agents and MCP servers.

> **Status: in development.** Nothing is released yet. This README describes what Crucible is being built to do;
> progress is tracked in [`tasks/todo.md`](tasks/todo.md).

## What Crucible will do

**Agent observability.** Send traces over OTLP from any OpenTelemetry SDK, or use the Crucible SDKs for TypeScript,
Python, Go and Rust. Crucible will show traces, spans, sessions and users with cost, tokens, latency and time to first
token, a trace tree, timeline and agent graph, full-text search across inputs and outputs, and feedback, tags and
comments on any trace.

**MCP server analytics.** Instrument an MCP server with a few lines of SDK code, or run it behind the `crucible proxy`
with no code changes. Crucible will report calls per tool, success and error rates split by error class (protocol,
validation, tool execution, auth, rate limit, timeout, transport), p50, p90 and p99 latency, clients, users and
sessions, tools nobody calls, and before and after comparisons across deploys. Both MCP protocol eras are supported:
the session-based revisions and the stateless 2026-07-28 revision.

**Both together.** When an agent and the MCP servers it calls both report to Crucible, the agent's tool call links to
the server-side span through W3C trace context, so one trace shows both sides.

**Analytics.** Custom dashboards, threshold alerts to Slack, email and webhooks, and a read-only SQL workspace scoped
to one project, with isolation enforced by ClickHouse itself.

**Built for rightmodeler.** rightmodeler will read Crucible exports to validate cheaper models on your own traces,
and coding agents will query Crucible through its MCP server.

## Design

```
 SDKs / OTel exporters / crucible proxy
            |  OTLP (HTTP, gRPC)
            v
 +-------------------------- crucible (one binary) --------------------------+
 |  ingest -> write-ahead log (fsync, then ack) -> normalizer -> shipper      |
 |                     |                                   |                  |
 |              recent-span index                one sealed batch per         |
 |                     |                         table and partition          |
 |  REST API + MCP server + UI  <- typed query plans <------+                 |
 +----------------------------------------------------------------------------+
            |                                   |
         Postgres                           ClickHouse
   (orgs, users, keys, alerts)      (spans, MCP operations, rollups)
```

- **Durable by construction.** A request is acknowledged only after its batch is fsynced to the write-ahead log.
  Inserts into ClickHouse carry deterministic deduplication tokens, so crashes and retries never lose or duplicate
  a span.
- **Fast.** The ingest path is zero-copy Rust; telemetry lands in large Native-format blocks; the read path uses typed
  query plans with keyset pagination and explicit resource limits.
- **Verified.** Deterministic simulation and swarm testing exercise the system under seeded disk, network, database and
  crash faults; a Quint model checks the ingest protocol; Kani proves the WAL codec.
- **Simple to run.** One binary plus ClickHouse (self-hosted 26.3 LTS or newer, or ClickHouse Cloud) plus Postgres,
  with optional S3-compatible storage for media.

## Documentation

- [Architecture overview](docs/architecture/overview.md)
- [Testing methodology](docs/testing/methodology.md)
- [Development](docs/development.md)
- [Self-hosting](docs/self-hosting/README.md) (development preview)

## License

[MIT](LICENSE). Third-party notices are in [`THIRD_PARTY_NOTICES`](THIRD_PARTY_NOTICES).
