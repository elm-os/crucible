# Changelog

Notable changes to Crucible, newest first. Versions follow [semantic versioning](https://semver.org/).

## [0.1.0] - 2026-11-20

The first release of Crucible: open-source, self-hosted observability and analytics for AI agents and MCP
servers, built in Rust and engineered for speed and throughput. One binary ingests OpenTelemetry traces and serves
the web UI, the REST API and an MCP server. ClickHouse stores the telemetry and Postgres runs the control plane.

### Ingest and storage

- OTLP trace ingest over HTTP (protobuf or JSON, optionally gzip) on ports 4318 and 8080, and over gRPC on 4317.
- Every request is appended to a write-ahead log and acknowledged only after its commit group is synced to disk.
  Each batch reaches ClickHouse with a deduplication token derived from its log position, so a retry never stores
  a span twice. A batch ClickHouse rejects is quarantined on disk, not dropped.
- Fixed byte budgets for memory, with 429 or 503 and Retry-After under load instead of dropped data.
- Ingest's blocking work (decompression, scanning and frame compression) runs under one CPU budget of first-come
  permits, so a burst of large requests queues instead of oversubscribing the machine.
- Recent spans are served from memory, so a trace can be read as soon as it is acknowledged.
- Mappings for the OpenTelemetry GenAI and MCP semantic conventions and for popular instrumentation libraries,
  with compatible ingest paths for other platforms' OTLP exporters ([integrations](docs/integrations/)).
- Cost calculated from usage with a built-in price table, in exact integer arithmetic, when the client sends none.
- Single-node ClickHouse 26.3 or newer for telemetry, and Postgres 15 or newer for the control plane.

### Explore

- A web UI embedded in the binary: a home overview, traces with a timeline and an agent graph, spans, sessions,
  users, full-text search, and a payload viewer that loads large payloads by byte range.
- Tags, flags, feedback scores and corrections, written through the same durable path as spans.
- A REST API under `/api/v1`, described by an OpenAPI 3.1 document (`crucible openapi`).
- A read-only SQL workspace, isolated per project by ClickHouse users, row policies and quotas, with saved queries
  and CSV, JSON lines and Parquet downloads.
- A metrics query API over spans and traces.
- Dashboards over the metrics query: built-in starters for an agent overview, cost and tokens, latency and errors,
  and your own, edited on a 12-column grid by pointer or keyboard, with stat, time series, bar, table and Markdown
  widgets.

### MCP

- Every MCP operation is stored with its method, target, error class, latency and capture source, and an agent's
  tool call links to the server-side span through trace context. Both MCP protocol eras are supported.
- `crucible proxy stdio` and `crucible proxy http` record an MCP server's calls with no code changes, forward its
  traffic byte for byte, and keep the session working when Crucible is unreachable.
- Crucible's own MCP server at `/mcp`, with tools to list and search traces, read spans and payloads, and create
  exports.
- SDKs for TypeScript, Python, Go and Rust in [`sdk/`](sdk/), with agent helpers and MCP server wrappers. They
  install from the repository.

### Platform

- Sign-in with email and password, and with GitHub, Google or any OpenID Connect provider added through the API.
  The first owner is created through a one-time setup link or `crucible admin init`.
- Organizations, projects, organization and project roles, invitations, and project API keys with scopes.
- Alert rules on span and trace measures, delivered to Slack, email and signed webhooks under an egress policy.
- Exports in `crucible-jsonl` and `otlp-json` through the API, the UI, the MCP server and `crucible export`.
- Per-project retention settings and deletion requests by trace, user or session.

### Distribution

- The container image `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and provenance
  attestations.
- Binaries for Linux and macOS, plus Windows for development, with shell and PowerShell installers. The
  write-ahead log's durability holds on Linux and macOS.
- Docker Compose files that run Crucible with a bundled ClickHouse and Postgres, or against your own
  ([self-hosting](docs/self-hosting/)).

### Known limits

- Crucible ingests OTLP traces; OTLP logs and metrics are answered with 404.
- ClickHouse Cloud and clustered ClickHouse are refused at startup.
- Sign-in providers, organization settings and project renames are managed through the REST API, not the UI.
