# Crucible's MCP server

Crucible serves an MCP server at `/mcp` on its main listener, so a coding agent can read a project's traces while it
works: list traces, spans, sessions and users, open a trace, read a span's input or output, search, read the MCP
analytics of the project's MCP servers, and start an export. It speaks Streamable HTTP in both MCP protocol eras:
the session-based revisions and the stateless 2026-07-28 revision.

## Connect an agent

Every request carries a project's secret key, with the `read` scope, as `Authorization: Bearer <secret key>`. The
key decides the project; no tool takes a project argument.

A key made in **Settings**, **API keys** has the scopes `ingest`, `read` and `write`, so it can also send traces and
write tags, flags, scores and corrections. A key that can read and nothing else comes from the command line: from
the folder that holds the Compose file, this prints the new key's `project_id`, `public_key` and `secret_key`:

```sh
docker compose exec crucible crucible admin create-key --project default --scopes read --note "coding agent"
```

Or, signed in as a project admin, send `POST /api/v1/projects/{project_id}/api-keys` with the body
`{"note": "coding agent", "scopes": ["read"]}`, the session cookie, `X-Crucible-CSRF` and `Origin`
([how to sign in](../api/README.md#authentication)).

With Claude Code, from your project's directory:

```sh
export CRUCIBLE_SECRET_KEY=csk_...
claude mcp add --transport http crucible http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer $CRUCIBLE_SECRET_KEY"
claude mcp get crucible     # Status: Connected
```

In PowerShell, `$CRUCIBLE_SECRET_KEY` is a PowerShell variable, so set and read the environment variable with
`$env:`:

```powershell
$env:CRUCIBLE_SECRET_KEY = 'csk_...'
claude mcp add --transport http crucible http://127.0.0.1:8080/mcp `
  --header "Authorization: Bearer $env:CRUCIBLE_SECRET_KEY"
```

The default scope keeps the server and its key in your own Claude Code configuration for that directory.
`--scope project` writes them to `.mcp.json` in the directory instead, with the key as your shell expanded it. To
share the server through source control without the key, write `.mcp.json` yourself with a reference to the
variable, which Claude Code expands from each person's environment when it loads the file:

```json
{
  "mcpServers": {
    "crucible": {
      "type": "http",
      "url": "http://127.0.0.1:8080/mcp",
      "headers": { "Authorization": "Bearer ${CRUCIBLE_SECRET_KEY}" }
    }
  }
}
```

Claude Code asks you to approve a project server the first time it starts there.

Any MCP client that speaks Streamable HTTP and can send a header connects the same way, to `<server address>/mcp`.
With the TypeScript MCP SDK, `@modelcontextprotocol/client`, in an ES module:

```ts
// crucible-mcp.ts
import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";

const transport = new StreamableHTTPClientTransport(new URL("http://127.0.0.1:8080/mcp"), {
  requestInit: { headers: { Authorization: `Bearer ${process.env.CRUCIBLE_SECRET_KEY}` } },
});
const client = new Client({ name: "my-agent", version: "1.0.0" });
await client.connect(transport);

const bounds = await client.callTool({ name: "get_data_bounds", arguments: {} });
console.log(bounds.structuredContent);

await client.close();
```

With the Python `mcp` package:

```python
# crucible_mcp.py
import asyncio
import os

import httpx2
from mcp import Client
from mcp.client.streamable_http import streamable_http_client


async def main() -> None:
    headers = {"Authorization": f"Bearer {os.environ['CRUCIBLE_SECRET_KEY']}"}
    async with httpx2.AsyncClient(headers=headers, timeout=30) as http:
        async with Client(streamable_http_client("http://127.0.0.1:8080/mcp", http_client=http)) as client:
            bounds = await client.call_tool("get_data_bounds", {})
            print(bounds.structured_content)


asyncio.run(main())
```

Each prints the project's data bounds; they were run with `@modelcontextprotocol/client` 2.4.0 on Node.js 24 and
with `mcp` 2.3.0 on Python 3.12. The tools' output schemas give integer fields the formats `uint64` and `uint32`,
which JSON Schema does not define, so the TypeScript MCP SDK logs `unknown format "uint64" ignored in schema` (or
`"uint32"`) for each such field when it lists the tools; the tools work as described.

A request without a usable key answers 401 with a problem document that says how to send one. The transport's own
refusals are plain text, with the status codes of the MCP specification: 400 `Bad Request: Session ID is required`
for a request of the session-based revisions without its session id, and 404 `Not Found: Session not found` for a
session the server does not hold, such as one from before it restarted; the client then starts a new session.

## Tools

| Tool | What it returns |
|---|---|
| `get_data_bounds` | Where the project's data begins and ends: the first and last minute with spans and with MCP operations, the retention horizon, the widest window the list tools accept, and the server's time. Agents call it first to choose a window |
| `list_traces` | Traces whose head span started in `[from, to)`, newest first, with name, status, duration, span and error counts, cost, tokens, user, session, environment, release and tags. `filter` narrows them as the REST trace list's filter parameters do. Up to 50 per call |
| `list_spans` | Spans that started in `[from, to)` across traces, newest first, with trace and span ids, name, observation type, status, level, duration, model, environment, user, session, cost and tokens. `filter` takes the REST span list's filter parameters. Up to 50 per call |
| `list_sessions` | Sessions whose spans fall on the window's UTC days, last seen first, with trace, span and error counts, cost, tokens and up to 8 user ids. Up to 50 per call |
| `list_users` | Users whose spans fall on the window's UTC days, last seen first, with trace, session, span and error counts, cost and tokens. Up to 50 per call |
| `list_user_sessions` | The sessions one user's traces in the window name, latest trace first, with the first and last trace start and the user's trace count. Up to 50 per call |
| `get_trace` | One trace's spans in start order, with parent links, timing, status, model, tokens, cost and the first 1,024 bytes of each input and output. Up to 200 spans per call; traces above 5,000 spans come back paged: the root spans and their direct children |
| `get_span_children` | The direct children of one span, for walking a paged trace. Up to 200 per call |
| `get_span_payload` | One payload field of one span (input, output, system instructions, attributes, resource attributes, events, links, tool definitions or tool calls), up to 32 KiB of text per call, continued with `offset` |
| `search_spans` | Spans whose input or output contain words, or whose metadata holds `key=value` entries, newest first, with highlighted fragments. Up to 20 hits per call |
| `list_mcp_servers` | The MCP servers seen on one side in the window, most calls first: calls, errors, error rate, calls the caller caused, latency p50, p90 and p99, distinct clients, last seen and the latest deploy's version, with the capture source read for each |
| `list_mcp_tools` | The MCP tools called on one side, most calls first, then the tools a server listed but nobody called: calls, errors, latency, bytes and the latest schema hash |
| `get_mcp_tool` | One MCP tool over the window and per time bucket, at most 200 buckets with their size in `granularity`: calls, errors and latency, calls per error class, the top clients, and its servers' deploys |
| `list_mcp_errors` | The errors of MCP calls on one side: calls per error class and failed calls per method and tool, or with `breakdown` the failed calls grouped down to the JSON-RPC code and message |
| `compare_mcp_deploys` | One MCP server's tools in two deploys: per tool the calls, errors, error rate and latency of each, and the tools added, removed or with a changed schema |
| `create_export` | Starts an export of the window's spans as `crucible-jsonl` or `otlp-json` |
| `get_export` | One export's state; while it is queued or running, its attempts, the seconds it has waited and its last error; and, once it is `ready`, the REST path to download the file from with the same key |

- Every tool reads except `create_export`, which starts an export job and is marked as not read-only.
- `filter` on `list_traces` and `list_spans` holds the conditions of the REST list filters: `environment`, `release`,
  `version`, `user_id`, `session_id`, `level`, `observation_type`, `tags` and `metadata` (`key=value` entries) as
  lists, any value of a list matching, and `name`, text the name contains. Every condition must hold.
- On `list_traces`, `level` tests the trace's head span, as on the REST trace list. To find the traces with an error
  in any span, call `list_spans` with `"filter": {"level": ["error"]}`: each row carries its span's `trace_id`. Each
  `list_traces` row also counts the trace's error spans in `error_count`.
- `list_spans` returns at most 50 spans per call, newest first, and takes no sort or duration condition. For MCP tool
  calls, `list_mcp_tools` gives each tool's calls, errors and latency percentiles in one call; ranking other spans,
  such as the slowest tool spans of a day, takes the [SQL workspace](sql-workspace.md), which the MCP server does not
  serve.
- `list_sessions` and `list_users` read whole UTC days, because sessions and users are summed per day: a window of one
  hour returns every session or user with spans on that day, with the day's totals.
- Latency percentiles (`p50_us`, `p90_us`, `p99_us`) are estimates within 3.125% and can exceed the slowest call;
  exact values are in the SQL workspace ([MCP analytics](mcp-analytics.md#percentiles)).
- `compare_mcp_deploys` takes any deploy id: one that the server never reported reads as a deploy with no calls.
  `get_mcp_tool` lists the deploys of a tool's servers.
- A result stays under 64 KiB. A tool that cut its result returns a `next_cursor` to pass back as `cursor`, with the
  same other arguments, for the rest; cursors expire after 24 hours.
- Results carry previews, never whole payloads: `get_span_payload` reads a payload in pieces.
- An error the agent can act on, such as a window wider than the project's maximum or an expired cursor, comes back
  as a tool result with `isError` and a problem document whose `code` and text say what to change.

## Limits

Tool calls are limited per key: 50 per second sustained, with bursts of 200, by default. The operator changes them
with `mcp.calls_per_second` and `mcp.burst` (`CRUCIBLE_MCP_CALLS_PER_SECOND`, `CRUCIBLE_MCP_BURST`);
`mcp.calls_per_second = 0` turns the limit off. A project may create 5 exports per hour.
