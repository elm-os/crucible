# MCP analytics

The **MCP** pages answer questions about MCP servers from the operations Crucible records: which servers and tools
are called, how often they fail and why, how long they take, who calls them, and what a deploy changed. They read the
MCP operations that the SDK wrappers, `crucible proxy` and spans with the MCP semantic conventions produce
([MCP](mcp.md)). The same answers come from the REST API and from Crucible's MCP server.

## The scope of every page

The bar at the top of every MCP page sets what its sections read, and keeps it in the address:

- **Side.** Server side reads the servers' record of the calls they answered; client side reads the agents' record
  of the calls they made. The two sides are never added together.
- **Capture source.** Automatic reads each server from the best source that has calls in the time range: the SDK,
  then the proxy, then OTLP spans. SDK, Proxy and OTLP read only that source. A server's calls always come from one
  source, never a sum of two, and every section names the source it read for each server.
- **Include caller faults.** Calls the caller caused, such as invalid parameters, are left out of calls, errors and
  latency unless this is on. Every answer counts them apart as caller faults.

The time range is the shell's, at most the project's maximum window (30 days by default).

## Pages

| Page | What it shows |
|---|---|
| Servers | For each server: calls, errors, error rate, caller faults, p50, p90 and p99 latency, the last call, how many clients called it, its version and its deploys |
| Tools | For each tool: the same measures, request and response bytes, and the hash of its latest listed schema; tools a server listed that nobody called are marked unused |
| Tool | One tool: calls and errors over time as bars, p50, p90 and p99 latency as lines, with deploy markers and a table view; its error classes and its top clients |
| Errors | Calls by error class, and the failing methods and targets; the breakdown groups the errors by message and JSON-RPC code |
| Clients | Calls by client name, or by name and version; names and versions are what the clients reported about themselves |
| Users | The users behind the calls |
| Sessions | The MCP sessions |
| Operations | Every operation, filtered by target, method, client, error class, session or user, a page at a time |
| Operation | One operation: its captured arguments and result (the first 16 KiB of each), the other side's record of it, and the rounds of a multi-round call |
| Deploys | Each server's deploys: version, who observed it, first and last seen |
| Compare | Two deploys of one server, or a time range against an earlier one |

A server is named by `crucible.mcp.server.name`, else its resource's `service.name`. A deploy is the resource's
`crucible.deploy.id`, else the server's version (`crucible.mcp.server.version`, else `service.version`), as seen on
the server side.

## Long time ranges

The pages read a per-minute rollup, so a longer time range reads more rows. Over 7 or 30 days with many servers, a
page can take seconds to answer. A read made with an API key runs under its class's 5-second limit, and one that
passes it answers `query_timeout`: narrow the time range, or name a server.

## Alerts and dashboards

Alert rules can watch MCP calls: their count, error rate, errors, and p50, p90 or p99 latency, by server or target,
on one side and from one capture source ([alerts](alerts.md)). The MCP servers starter dashboard charts the server
side ([dashboards](dashboards.md)).

## Crucible's MCP server

Five read-only tools give a coding agent the same answers: `list_mcp_servers`, `list_mcp_tools`, `get_mcp_tool`,
`list_mcp_errors` and `compare_mcp_deploys` ([Crucible's MCP server](mcp-server.md)).

## Through the API

The routes under `projects/{project_id}/mcp/` take `side` (`client` or `server`), `from` and `to`, and optionally
`source` (`auto`, `sdk`, `proxy` or `otlp`), `include_caller_faults` and `server`
([API reference](../api/README.md#mcp-analytics)):

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/mcp/servers?side=server&from=2026-11-20T00:00:00Z&to=2026-11-21T00:00:00Z"
```

```json
{
  "data": [
    {
      "server_name": "weather-server",
      "stats": {
        "calls": 33, "errors": 3, "error_rate": 0.0909, "caller_faults": 2,
        "latency": {"p50_us": 39936, "p90_us": 88064, "p99_us": 88064},
        "last_seen": "2026-11-20T03:53:31.100900Z"
      },
      "clients": 2, "version": "1.3.0", "deploys": 2
    }
  ],
  "capture_sources": [{"server_name": "weather-server", "available": ["sdk"], "chosen": "sdk"}],
  "saturated": false,
  "meta": {"as_of": "2026-11-20T03:58:36.434682Z", "partial": false, "consistency": "strong"}
}
```

The answer above is cut to one server. Its two caller faults (calls refused with JSON-RPC `-32602`, invalid
parameters) are counted apart; with `include_caller_faults=true` the same server answers 35 calls and 5 errors. A
request without `side` answers 422 with `/side` `required`.
