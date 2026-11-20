# Crucible's MCP server

Crucible serves an MCP server at `/mcp` on its main listener, so a coding agent can read a project's traces while
it works: list them, open one, read a span's input or output, search, and start an export. It speaks Streamable
HTTP in both MCP protocol eras: the session-based revisions and the stateless 2026-07-28 revision.

## Connect an agent

Every request carries a project's secret key, with the `read` scope, as `Authorization: Bearer <secret key>`. The
key decides the project; no tool takes a project argument. Make a key in **Settings**, **API keys**, or with
`crucible admin create-key --project <slug> --scopes read`, which gives a key that can read and nothing else.

With Claude Code, from your project's directory:

```sh
claude mcp add --transport http crucible http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer $CRUCIBLE_SECRET_KEY"
claude mcp get crucible     # Status: Connected
```

The default scope keeps the server and its key in your own Claude Code configuration for that directory.
`--scope project` writes them to `.mcp.json` in the directory instead, key included, so keep that file out of source
control; Claude Code asks you to approve a project server the first time it starts there. Any MCP client that speaks
Streamable HTTP and can send a header connects the same way, to `<server address>/mcp`.

A request without a usable key answers 401 with a problem document that says how to send one.

## Tools

| Tool | What it returns |
|---|---|
| `get_data_bounds` | Where the project's data begins and ends: the first and last minute with spans and with MCP operations, the retention horizon, the widest window the list tools accept, and the server's time. Agents call it first to choose a window |
| `list_traces` | Traces whose head span started in `[from, to)`, newest first, with name, status, duration, span and error counts, cost, tokens, user, session, environment, release and tags. Up to 50 per call |
| `get_trace` | One trace's spans in start order, with parent links, timing, status, model, tokens, cost and the first 1,024 bytes of each input and output. Up to 200 spans per call; traces above 5,000 spans come back paged: the root spans and their direct children |
| `get_span_children` | The direct children of one span, for walking a paged trace. Up to 200 per call |
| `get_span_payload` | One payload field of one span (input, output, system instructions, attributes, resource attributes, events, links, tool definitions or tool calls), up to 32 KiB of text per call, continued with `offset` |
| `search_spans` | Spans whose input or output contain words, or whose metadata holds `key=value` entries, newest first, with highlighted fragments. Up to 20 hits per call |
| `create_export` | Starts an export of the window's spans as `crucible-jsonl` or `otlp-json` |
| `get_export` | One export's state and, once it is `ready`, the REST path to download the file from with the same key |

- Every tool reads except `create_export`, which starts an export job and is marked as not read-only.
- A result stays under 64 KiB. A tool that cut its result returns a `next_cursor` to pass back as `cursor`, with the
  same other arguments, for the rest; cursors expire after 24 hours.
- Results carry previews, never whole payloads: `get_span_payload` reads a payload in pieces.
- An error the agent can act on, such as a window wider than the project's maximum or an expired cursor, comes back
  as a tool result with `isError` and a problem document whose `code` and text say what to change.

## Limits

Tool calls are limited per key: 50 per second sustained, with bursts of 200, by default. The operator changes them
with `mcp.calls_per_second` and `mcp.burst` (`CRUCIBLE_MCP_CALLS_PER_SECOND`, `CRUCIBLE_MCP_BURST`);
`mcp.calls_per_second = 0` turns the limit off. A project may create 5 exports per hour.
