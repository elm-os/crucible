# Exploring traces

The UI's pages for reading telemetry: the home page, traces and the trace page, spans, sessions and users. Every
page reads the project chosen in the header and the time range beside it; links between pages keep both. The same
data is in the [REST API](../api/README.md#reading-traces).

## Home

**Home** shows the project's activity over the time range: traces over time, spans and errors per bucket, span
latency percentiles, and the totals of traces, spans, errors and the error rate, tokens and cost. The charts use
minute buckets for the 1-hour range, hourly buckets for 24 hours and 7 days, and daily buckets for 30 days. Hover
over or focus a chart to read a bucket: the arrow keys, Home and End step through the buckets, and **View as
table** lists every value.

The **Set up** card lists four steps, each marked **To do**, **Done** or **Couldn't check**:

1. **Create an API key** is done once the project has a key that is neither revoked nor expired. Its link opens
   **Settings**, **API keys**.
2. **Send your first trace** is done once the project has spans inside its retention. Its link opens
   [sending traces](sending-traces.md).
3. **Instrument an MCP server** is done once the project has MCP operations from either side: an MCP server's SDK
   wrapper or `crucible proxy`, or an agent's MCP client wrapper. Its link opens the [MCP overview](mcp.md).
4. **Invite your team** is done once the organization has more than one member; an invitation counts once it is
   accepted. Its link opens **Settings**, **Members**.

On a project that has received no spans the card comes first, above the tiles; otherwise it comes after the
charts, and it is gone once no step is left to do. A step the viewer cannot do says who does it instead: project
admins create API keys, and organization owners and admins invite people. On a server without a control plane, the
key step says the keys come from the keys file, and the invite step says how to turn on sign-in and members. When a
step's read fails, or the data comes back incomplete, the step says **Couldn't check**, with **Check again**.

## Traces

**Traces** lists the traces whose head span (the root, or the earliest span while the root has not arrived)
started in the time range, newest first: name, status, start, duration, span count and cost. While you watch, a
banner says when new traces arrived, with a button such as **Show 3 new traces** that loads them, and
**Load more traces** reads the next page.

**Filter by** adds filters: environment, level, type, user, session, tag, metadata (`key=value`, for example
`team=search`), release and version, and **Name contains** matches part of a name. Each value of one filter
widens the match, and different filters must all hold. On the traces list, type and metadata keep traces that have a
matching span; the other filters test the trace's head span. Type and metadata cannot be combined on the traces list;
the spans list filters by both.

## The trace page

Choosing a trace opens its page. Its header is titled with the trace name when one is set, else the head span's name, as
in the trace list; when the two differ, a line names the head span: **Root span**, or **First span** while the root span
has not arrived. The header shows the start, duration, cost, tokens and span count, the same numbers as the trace list,
then the user and the session, each a link to its page, the environment and the release, each when set. The cost and
tokens leave out a span whose child spans report usage, so an agent span that repeats its run's totals counts once. For
a trace with more than 5,000 spans they still cover the whole trace. Below the header are three views of its spans.

- **The tree and timeline.** Each span on one row under its parent, with its type and a bar for its duration on
  the trace's timeline. A trace with more than 5,000 spans opens with its root spans and their direct children, and
  expanding a span loads its children.
- **Agent graph.** The agents, tools and model calls of the trace as a graph, each linked to its place in the
  timeline.
- **Span details.** Choosing a span opens its overview (status, level, model, tokens, cost, duration, its parent and
  child spans, its id) and its input, output, attributes and events. Model conversations show as messages by role
  (system, developer, user, assistant, tool); other JSON as a tree; text as text, labelled as cut at the size limit
  only when the span reports the input or output cut. The **Media** tab shows the
  images, audio, video and files the span references, when the server has object storage
  ([media](media.md#in-the-span-panel)).

The overview of a failed span shows its status and level. The trace views and the span rows of the REST API and the
MCP server don't show a span's status message or the `service.name` of the service that sent it; the SQL workspace
has both, as the `status_message` and `service_name` columns of `crucible_sql.spans`
([SQL workspace](sql-workspace.md)):

```sql
SELECT start_time, service_name, name, status_message
FROM crucible_sql.spans
WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)} AND status_code = 'error'
ORDER BY start_time DESC
```

Exceptions recorded on a span, OpenTelemetry's `exception` events with their type, message and stack trace, are in
the **Events** tab. They are the span's `events` payload field, which the REST API's
[payload route](../api/README.md#payloads) (`field=events`) and the MCP server's
[`get_span_payload`](mcp-server.md#tools) return.

The **Tags, flags and feedback** section of the trace page adds tags, the bookmark and public flags, and scores
([facts](facts.md)).

## Spans

**Spans** lists every span that started in the time range, newest first, with its type, model, status, duration
and cost, and takes the same filters as the traces list, type and metadata together included.

## Sessions and users

A span belongs to a session when it carries `crucible.session.id` (or `session.id` or `gen_ai.conversation.id`),
and to a user when it carries `crucible.user.id` (or `user.id`) ([sending traces](sending-traces.md#attributes-the-trace-views-read)).

- **Sessions** lists the sessions with traces in the time range: last seen, traces, spans, tokens, cost and status.
  A session's page shows its summary (first and last seen, its users and totals) and the traces in it.
- **Users** lists the users with traces in the time range: last seen, sessions, traces, spans, tokens, cost and
  status. A user's page shows the summary (first and last seen, first and latest trace, totals), the user's sessions
  and traces.

## Time range

The time range applies to every list, up to the project's widest window of 30 days. An empty page offers **Show last
30 days**. The trace page needs no range: it finds the trace's own bounds.
