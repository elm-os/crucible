# MCP

Crucible covers MCP from both ends. An MCP server reports every operation it handles, through an SDK wrapper or
`crucible proxy`; an agent's MCP client calls carry their trace context to the server, so the agent's trace holds
the server's side too; and Crucible's own MCP server lets a coding agent read its traces.

| Want to | Use |
|---|---|
| Record what an MCP server does, with error subclasses, user identity and opt-in arguments and results | The SDK wrapper of the server's language, below |
| Record what an MCP server does without changing it, in any language | [`crucible proxy`](mcp-proxy.md) |
| See an agent's MCP calls and the servers' handling of them in one trace | The SDK's client wrapper in the agent, and a wrapper or the proxy on the server |
| Let a coding agent list, read and search traces | [Crucible's MCP server](mcp-server.md) at `/mcp` |

## SDK wrappers

Each SDK has a server wrapper, which records one span per MCP operation the server handles, in both protocol eras
(the session-based revisions and the stateless 2026-07-28 revision), and a client wrapper, which records each call
an agent makes and puts its trace context in the request's `params._meta`.

| Language | Server | Client | Guide |
|---|---|---|---|
| TypeScript (2.x line of the official MCP SDK) | `instrument(server)` and, for HTTP handlers, `instrumentHandler`, from `@elm-os/crucible/mcp` | `instrumentClient(client)` from `@elm-os/crucible/mcp-client` | [TypeScript SDK](sdk-typescript.md#instrument-an-mcp-server) |
| Python (`mcp` 2.x) | `crucible_sdk.mcp.instrument(server)` | `crucible_sdk.mcp.instrument_client(client)` | [Python SDK](sdk-python.md#instrument-an-mcp-server) |
| Go (the official MCP Go SDK) | `cruciblemcp.Instrument(server, options)` | `cruciblemcp.InstrumentClient(client, options)` | [Go SDK](sdk-go.md#instrument-an-mcp-server) |
| Rust (rmcp) | `CrucibleService`, feature `mcp` | `TracedClient`, feature `mcp-client` | [Rust SDK](../../sdk/rust/README.md#instrument-an-mcp-server) |

In a tool handler, every wrapper lets the server say more than the wire shows: an error's subclass (auth, rate
limit, upstream, business), whether a failure was the caller's (validation, not found), and who the caller is.
Arguments and results are captured only when you ask for them, per server or per tool, cut at 16 KiB by default and
scrubbed in the SDK with the shared rules of `conformance/scrub-rules.json`.

## What Crucible records

Every span that carries `mcp.method.name`, from an SDK, the proxy or any OpenTelemetry instrumentation that follows
the MCP semantic conventions, is stored as a span and recorded as an MCP operation on the client or the server side:
its method, tool, prompt or resource, JSON-RPC id, outcome and error class, duration, protocol era and version,
transport, client name and version, session, the hashed user subject, and the sizes of the request and response.
Captured arguments and results are kept beside it, under their own retention (`mcp_content_retention_days`).

**MCP** in the navigation reads them per side: servers, tools (with the ones a server listed but nobody called),
one tool over time with its deploys marked, errors, clients, users, sessions, operations with their captured content,
deploys, and a compare of two deploys or two windows. Each page reads every server from one capture source, the
automatic choice by default, and says which. The same reads are the routes under `projects/{project_id}/mcp/` (see
the [API reference](../api/README.md#mcp-analytics)) and the analytics tools of [Crucible's MCP
server](mcp-server.md); [alert rules](alerts.md) and the MCP servers [starter dashboard](dashboards.md) read the
`mcp_ops` metrics.

The SQL workspace reads them in `crucible_sql.mcp_operations`, `crucible_sql.mcp_contents`,
`crucible_sql.mcp_tool_metrics` (per minute and tool), `crucible_sql.mcp_tools` (the tools, prompts and resources
each server listed) and `crucible_sql.mcp_deploys`:

```sql
SELECT server_name, target_name, status, error_class, count() AS calls, quantile(0.99)(duration_us) AS p99_us
FROM crucible_sql.mcp_operations
WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}
  AND side = 'server' AND method = 'tools/call'
GROUP BY server_name, target_name, status, error_class
ORDER BY calls DESC
```

A call recorded by both an SDK wrapper and the proxy in front of the same server has one row per capture source;
`capture_source` (`sdk`, `proxy_stdio`, `proxy_http` or `otlp`) keeps them apart.
