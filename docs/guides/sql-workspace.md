# SQL workspace

The **SQL** page runs read-only SQL over the project's telemetry in ClickHouse. Each project's queries run as that
project's own ClickHouse user, which sees only the project's rows within its retention
([how isolation works](../self-hosting/sql-workspace.md#how-isolation-works)). Running ad hoc SQL needs the member
role, or an API key with the `sql` scope; viewers run the project's saved queries.

## Writing a query

The **Schema** browser lists the relations, in the `crucible_sql` database, with their columns:

| Relation | One row per |
|---|---|
| `crucible_sql.spans` | Span: timing, names, model, token usage and cost |
| `crucible_sql.span_payloads` | Span: input, output, metadata and attributes |
| `crucible_sql.traces` | Trace head: the root span's name, user, session and tags |
| `crucible_sql.sessions`, `crucible_sql.users` | Day and session, or day and user: trace, span and error counts, tokens and cost |
| `crucible_sql.span_metrics` | Minute and dimension: span counts, errors, tokens, cost and latency histograms |
| `crucible_sql.scores`, `crucible_sql.tags`, `crucible_sql.flags`, `crucible_sql.corrections` | Fact edit ([facts](facts.md)) |
| `crucible_sql.mcp_operations`, `crucible_sql.mcp_contents` | MCP operation, and its captured arguments and results ([MCP](mcp.md)) |
| `crucible_sql.mcp_tool_metrics` | Minute and MCP target: calls, sizes and latency histograms |
| `crucible_sql.mcp_tools`, `crucible_sql.mcp_deploys` | Tool, prompt or resource a server listed; deploy of a server |

A query is one `SELECT` or `WITH` statement that filters on the time window, written as the parameters
`{from:DateTime64(6)}` and `{to:DateTime64(6)}`, which the page fills from the time range at the top:

```sql
SELECT name, count() AS spans, sum(input_tokens) AS input_tokens
FROM crucible_sql.spans
WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}
GROUP BY name
ORDER BY spans DESC
```

Run it with **Control+Enter**. The relations with a time column need the window: a query without it is refused with
`Filter on the time window: add a condition such as start_time >= {from:DateTime64(6)}`. The statement goes to
ClickHouse as written, so the ClickHouse SQL reference applies; an error says where in the statement it happened.

## Results and limits

A result holds at most 10,000 rows and 16 MiB, with each cell cut at 64 KiB; a cut result says so. A run may take
30 seconds. One person runs one query at a time and 30 a minute, a project four at a time, and each project has an
hourly allowance of ClickHouse statements, 1,800 by default, of which a run takes three. A result downloads as CSV,
JSON lines or Parquet; CSV cells that start with `=`, `+`, `-` or `@` get a leading apostrophe, so a spreadsheet does
not run them.

**History** lists the project's runs of the last 30 days: who ran what, its outcome, rows and time. **Load** puts a
run's query back in the editor.

## Saved queries

A saved query keeps a statement under a name, private to you or shared with the project. Besides the window, it may
declare parameters, `{name:String}` in the text, with a type (`string`, `int`, `float`, `bool`, `timestamp` or
`uuid`) and an optional default; whoever runs it fills them in. Members save and edit queries; viewers run the
shared ones.

## Through the API

`POST projects/{project_id}/sql/run` takes `sql`, `from`, `to` and the values of other parameters, and returns the
columns, the rows (as text, in ClickHouse's text form), whether the result was cut, and what the server read. With a
key that has the `sql` scope:

```sh
curl -s -X POST -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/sql/run" \
  -d '{"sql": "SELECT name, count() AS spans FROM crucible_sql.spans WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)} GROUP BY name ORDER BY name",
       "from": "2026-11-19T00:00:00Z", "to": "2026-11-21T00:00:00Z"}'
```

```json
{"run_id":"01a1f672-205d-7684-a441-0993237ceb88","columns":[{"name":"name","type":"String"},{"name":"spans","type":"UInt64"}],"rows":[["a new trace","1"],["chat","1"],["initialize","2"],["notifications/initialized","2"],["plan trip","1"],["summarize","1"],["tools/call echo","2"],["tools/call lookup_order","1"],["tools/call no_such_tool","1"],["tools/list","1"],["via collector grpc","1"]],"truncated":false,"cut_cells":0,"stats":{"rows_read":14,"bytes_read":567,"elapsed_ms":4,"estimated_rows":14}}
```

`POST .../sql/export` takes a `format` (`csv`, `jsonl` or `parquet`) and returns the file, and the saved-query routes
list, create, change, run and delete saved queries ([API](../api/README.md#sql-workspace)).
