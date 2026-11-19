# Guides

How to send telemetry to Crucible and read it back. The guides describe the server and SDKs as they are on main;
anything not there yet is marked as planned. To run a server first, follow the
[quickstart](../self-hosting/quickstart.md).

| Guide | What it covers |
|---|---|
| [Sending traces](sending-traces.md) | OTLP over HTTP and gRPC from any OpenTelemetry SDK or the Collector: endpoints, keys, what an acknowledgement means, retries, size caps, and the attributes the trace views read |
| [TypeScript SDK](sdk-typescript.md) | `@elm-os/crucible` for Node.js agents, MCP servers and MCP clients |
| [Python SDK](sdk-python.md) | `crucible-sdk` for Python agents, MCP servers and MCP clients |
| [Go SDK](sdk-go.md) | `github.com/elm-os/crucible/sdk/go` for Go agents, MCP servers and MCP clients |
| [MCP proxy](mcp-proxy.md) | `crucible proxy`, which records an MCP server's operations without changing the server |
| [Alerts](alerts.md) | Alert rules, notification channels, verifying a webhook, and the egress and mail settings |
| [REST API](../api/README.md) | Reading traces, spans, payloads and search results back: auth, errors, windows and cursors |

The Rust SDK, crate `crucible-sdk` in `sdk/rust`, is described in [its README](../../sdk/rust/README.md).
