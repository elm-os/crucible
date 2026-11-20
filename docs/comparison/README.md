# How Crucible differs

This page compares the architecture of Crucible with other observability platforms for AI applications that can be
self-hosted or run in your own cloud. Every statement about another product comes from that product's own public
documentation, linked where it is made, as read on 2026-11-19; when a product's documentation does not say something,
this page does not guess. It holds no performance figures for any product.

## Crucible's design in brief

- **One binary.** The `crucible` server ingests OTLP over HTTP and gRPC and serves the REST API, the MCP server and the
  web UI; `crucible proxy` instruments MCP servers. A deployment adds ClickHouse for telemetry and Postgres for the
  control plane, and nothing else: no queue, no cache and no object store
  ([self-hosting](../self-hosting/README.md)).
- **Durable before acknowledged.** An ingest request is acknowledged only after it is written to the server's own
  write-ahead log and synced to disk. A server that stops after the acknowledgement ships the request when it starts
  again ([sending traces](../guides/sending-traces.md#what-an-acknowledgement-means)).
- **Exactly-once inserts.** Every insert into ClickHouse carries a deterministic deduplication token derived from the
  sealed batch, so a retry after a crash or an unknown outcome never duplicates a row
  ([contract 5](../architecture/contracts.md#5-insert-protocol)).
- **Deterministic simulation.** The whole server runs in a seeded simulation with injected disk, network, database and
  crash faults, and a Quint model checks the ingest protocol ([testing methodology](../testing/methodology.md)).
- **MCP on both ends.** SDK wrappers and the proxy record every operation an MCP server handles as an MCP operation
  with its outcome and error class, and Crucible's own MCP server answers coding agents ([MCP](../guides/mcp.md)).
- **MIT licensed.**

## Deployment shape

| Product | What a self-hosted deployment runs | Datastores | License |
|---|---|---|---|
| Crucible | One `crucible` server | ClickHouse, Postgres | MIT |
| [Langfuse](https://langfuse.com/self-hosting) | A web container and a worker container ([containers](https://langfuse.com/self-hosting/deployment/infrastructure/containers)) | Postgres, ClickHouse, Redis or Valkey, and S3-compatible object storage ([overview](https://langfuse.com/self-hosting)) | MIT core, with directories under a commercial enterprise license ([LICENSE](https://github.com/langfuse/langfuse/blob/main/LICENSE)) |
| [Arize Phoenix](https://arize.com/docs/phoenix/self-hosting) | One container with the UI and the trace collector ([architecture](https://arize.com/docs/phoenix/self-hosting/architecture)) | SQLite by default, or Postgres ([architecture](https://arize.com/docs/phoenix/self-hosting/architecture)) | Elastic License 2.0 ([license](https://arize.com/docs/phoenix/self-hosting/license)) |
| [LangSmith](https://docs.langchain.com/langsmith/self-hosted) | A frontend, backends, a queue and an ingest queue, a playground and agent services, on Kubernetes, as an Enterprise add-on | ClickHouse, Postgres and Redis or Valkey, with blob storage recommended ([self-hosted](https://docs.langchain.com/langsmith/self-hosted)) | Not stated in its documentation; self-hosting needs a license key |
| [Helicone](https://github.com/Helicone/helicone) | A web frontend, a worker and the Jawn server ([README](https://github.com/Helicone/helicone)); an all-in-one Docker image runs the dashboard and Jawn ([Docker](https://docs.helicone.ai/getting-started/self-host/docker)) | Postgres, ClickHouse and MinIO ([Docker](https://docs.helicone.ai/getting-started/self-host/docker)) | Apache 2.0 ([LICENSE](https://github.com/Helicone/helicone/blob/main/LICENSE)) |
| [Braintrust](https://docs.braintrust.dev/admin/deployment) | A Braintrust-managed control plane; the data plane runs as SaaS or in a cloud environment you own that Braintrust operates (BYOC) ([deployment](https://docs.braintrust.dev/admin/deployment)) | Postgres, Redis and object storage in the data plane ([architecture](https://docs.braintrust.dev/admin/deployment/architecture)) | Not stated in its documentation |

## How ingest is acknowledged

| Product | What its documentation says |
|---|---|
| Crucible | Acknowledges an OTLP request after it is synced to the server's write-ahead log, then ships it to ClickHouse in the background |
| Langfuse | "All traces are received in batches by the Langfuse Web container and immediately written to S3. Only a reference is persisted in Redis for queueing", and a worker loads them into ClickHouse ([overview](https://langfuse.com/self-hosting)). Its OTLP endpoint takes HTTP with JSON or protobuf and not gRPC ([OpenTelemetry](https://langfuse.com/integrations/native/opentelemetry)) |
| LangSmith | Its OTel endpoint "accepts a batch, returns `200`, and materializes runs in the background" ([OpenTelemetry](https://docs.langchain.com/langsmith/trace-with-opentelemetry)) |
| Braintrust | "When the Braintrust ingestion API returns a successful response, the accepted trace data has been written to durable object storage" ([durability](https://docs.braintrust.dev/instrument)) |
| Arize Phoenix | Takes OTLP over HTTP and gRPC ([configuration](https://arize.com/docs/phoenix/self-hosting/configuration)); its documentation does not describe when a request is acknowledged |
| Helicone | Logs through its gateway or asynchronously ([proxy and async](https://docs.helicone.ai/references/proxy-vs-async)); its documentation does not describe when an event is durable |

## MCP

| Product | MCP server for querying data | Recording MCP servers |
|---|---|---|
| Crucible | `/mcp` on the server, read-only tools plus exports, with a project key | SDK wrappers for TypeScript, Python, Go and Rust and `crucible proxy` record each operation with its outcome, error class, client, session and duration, as MCP operations |
| Langfuse | `/api/public/mcp`, self-hosted too ([MCP server](https://langfuse.com/docs/api-and-data-platform/features/mcp-server)) | Links client and server traces through trace context in `_meta` ([MCP tracing](https://langfuse.com/docs/observability/features/mcp-tracing)) |
| Arize Phoenix | `/mcp`, built in (beta) ([MCP](https://arize.com/docs/phoenix/integrations/mcp)) | Context propagation between MCP clients and servers through OpenInference ([MCP tracing](https://arize.com/docs/phoenix/integrations/python/mcp-tracing)) |
| LangSmith | A remote MCP server at `/api/mcp` from v0.16 ([remote MCP](https://docs.langchain.com/langsmith/langsmith-remote-mcp)) | A Tool Gateway (beta) that traces tool calls, documented for LangSmith Cloud ([Tool Gateway](https://docs.langchain.com/langsmith/tool-gateway)) |
| Braintrust | A hosted MCP server ([MCP](https://docs.braintrust.dev/integrations/developer-tools/mcp)) | Traces MCP servers through its SDKs ([MCP servers](https://docs.braintrust.dev/observe/mcp-servers)) |
| Helicone | An MCP server run with `npx` ([MCP](https://docs.helicone.ai/integrations/tools/mcp)) | Not described in its documentation |
