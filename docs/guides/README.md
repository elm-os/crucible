# Guides

How to send telemetry to Crucible and use it. To run a server first, follow the
[quickstart](../self-hosting/quickstart.md).

## Sending data

| Guide | What it covers |
|---|---|
| [Sending traces](sending-traces.md) | OTLP over HTTP and gRPC from any OpenTelemetry SDK or the Collector: endpoints, keys, what an acknowledgement means, retries, size caps, and the attributes the trace views read |
| [TypeScript SDK](sdk-typescript.md) | `@elm-os/crucible` for Node.js agents, MCP servers and MCP clients |
| [Python SDK](sdk-python.md) | `crucible-sdk` for Python agents, MCP servers and MCP clients |
| [Go SDK](sdk-go.md) | `github.com/elm-os/crucible/sdk/go` for Go agents, MCP servers and MCP clients |
| [Rust SDK](../../sdk/rust/README.md) | `crucible-sdk` for Rust agents, rmcp MCP servers and MCP clients |

## MCP

| Guide | What it covers |
|---|---|
| [MCP](mcp.md) | The ways Crucible covers MCP, the SDK wrappers per language, and what it records about MCP operations |
| [MCP proxy](mcp-proxy.md) | `crucible proxy`, which records an MCP server's operations without changing the server |
| [Crucible's MCP server](mcp-server.md) | Connecting a coding agent to `/mcp` with a project key, and its tools |

## Using the data

| Guide | What it covers |
|---|---|
| [Exploring traces](exploring-traces.md) | Home, traces and the trace page, spans, sessions, users, filters and the time range |
| [Search](search.md) | Finding spans by the words of their input and output or their metadata, and reading payloads |
| [Tags, flags, scores and corrections](facts.md) | Adding facts in the UI and the API, and how they resolve |
| [Dashboards](dashboards.md) | The starter dashboards, your own dashboards, the editor, the widget kinds and who can edit |
| [Alerts](alerts.md) | Alert rules, notification channels and verifying a webhook |
| [Exports](exports.md) | Exporting spans from the UI, the API, the MCP server and `crucible export`, and validating models with rightmodeler |
| [SQL workspace](sql-workspace.md) | Read-only SQL over the project's telemetry, saved queries and limits |
| [Retention and deletion](../self-hosting/retention-and-deletion.md) | How long a project keeps data, and deleting a trace, a user or a session |
| [REST API](../api/README.md) | Every route, authentication, errors, windows and cursors |
