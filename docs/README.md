# Crucible documentation

## Running Crucible

| Page | What it covers |
|---|---|
| [Self-hosting](self-hosting/README.md) | Deployments, the image, Docker Compose, the binary and ports |
| [Quickstart](self-hosting/quickstart.md) | Docker Compose from an empty machine to a first trace |
| [Configuration](self-hosting/configuration.md) | Every setting, with its key, environment variable, flag and default |
| [Operations](self-hosting/operations.md) | Upgrades, backups and restores, health checks, metrics and logs |
| [Security](self-hosting/security.md) | What is protected by default, and the switches that harden a deployment |
| [Retention and deletion](self-hosting/retention-and-deletion.md) | How long data is kept, and deleting a trace, a user or a session |
| [Alerts and egress](self-hosting/alerts-and-egress.md) | Outbound calls of channels, the egress policy and email |
| [SQL workspace](self-hosting/sql-workspace.md) | The per-project ClickHouse users, the self-probe, limits and the quota |
| [Troubleshooting](self-hosting/troubleshooting.md) | Common refusals and their fixes |

## Sending and using data

| Page | What it covers |
|---|---|
| [Guides](guides/README.md) | The index of the guides below |
| [Sending traces](guides/sending-traces.md) | OTLP over HTTP and gRPC from any OpenTelemetry SDK or the Collector |
| [TypeScript SDK](guides/sdk-typescript.md), [Python SDK](guides/sdk-python.md), [Go SDK](guides/sdk-go.md), [Rust SDK](../sdk/rust/README.md) | The SDKs for agents, MCP servers and MCP clients |
| [MCP](guides/mcp.md) | SDK wrappers, the proxy and Crucible's MCP server |
| [MCP proxy](guides/mcp-proxy.md) | Recording an MCP server without changing it |
| [Crucible's MCP server](guides/mcp-server.md) | Connecting a coding agent to `/mcp` |
| [MCP analytics](guides/mcp-analytics.md) | Servers, tools, errors, clients, sessions, operations and deploys, per side |
| [Exploring traces](guides/exploring-traces.md) | Home, traces, the trace page, spans, sessions and users |
| [Search](guides/search.md) | Full-text and metadata search, and payloads |
| [Tags, flags, scores and corrections](guides/facts.md) | Facts in the UI, the SDKs and the API |
| [Dashboards](guides/dashboards.md) | Starter dashboards, your own, the editor and the widget kinds |
| [Alerts](guides/alerts.md) | Alert rules, channels and webhook verification |
| [Exports](guides/exports.md) | Exports and the rightmodeler workflow |
| [SQL workspace](guides/sql-workspace.md) | Read-only SQL over a project's telemetry |
| [REST API](api/README.md) | Every route, authentication, errors, windows, cursors and idempotency |
| [Integrations](integrations/README.md) | Pointing other instrumentation libraries and tracing SDKs at Crucible, and what each maps |
| [Authorization matrix](architecture/authz-matrix.md) | The permission of every route, and the roles and scopes that grant it |

## Comparisons

| Page | What it covers |
|---|---|
| [How Crucible differs](comparison/README.md) | Crucible's architecture next to other platforms, from their own documentation |

## Architecture

| Page | What it covers |
|---|---|
| [Overview](architecture/overview.md) | The map: roles, write and read paths, storage, MCP, the control plane, jobs and crates |
| [Contracts](architecture/contracts.md) | The binding contract: records, the WAL, the insert protocol, schemas, the API and the control plane |
| [Decisions](architecture/decisions.md) | The reasons behind the contract's load-bearing choices |
| [Price table](architecture/price-table.md) | How usage without a provided cost is priced |
| [MCP TypeScript seam](architecture/mcp-typescript-seam.md) | The decision record for the TypeScript SDK's MCP seam |
| [Adding an endpoint](architecture/adding-an-endpoint.md) | The files and registries a read endpoint touches |
| [Adding a job](architecture/adding-a-job.md) | Registering a background job and the checks that prove it |

## Testing and development

| Page | What it covers |
|---|---|
| [Testing](testing/README.md) | The index of the verification pages: methodology, invariants, simulations, conformance and performance |
| [Methodology](testing/methodology.md) | Deterministic simulation, swarm testing, formal methods and conformance |
| [Development](development.md) | The dev container, running the server from source, the UI against a server and the end-to-end test |
| [Contributing](../CONTRIBUTING.md) | The checks a change passes and the pull request process |
