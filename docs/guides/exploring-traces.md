# Exploring traces

The UI's pages for reading telemetry: the home page, traces and the trace page, spans, sessions and users. Every
page reads the project chosen in the header and the time range beside it; links between pages keep both. The same
data is in the [REST API](../api/README.md#reading-traces).

## Home

**Home** shows the project's activity over the time range: traces over time, spans and errors per bucket, span
latency percentiles, and the totals of traces, spans, errors and the error rate, tokens and cost. A project that has
received nothing yet shows the setup steps instead: send a first trace, instrument an MCP server, create an API key
and invite your team.

## Traces

**Traces** lists the traces whose head span (the root, or the earliest span while the root has not arrived)
started in the time range, newest first: name, status, start, duration, span count and cost. While you watch, a
banner says when new traces arrived, with a button such as **Show 3 new traces** that loads them.

**Filter by** adds filters: environment, level, type, user, session, tag, metadata (`key=value`, for example
`team=search`), release and version, and **Name contains** matches part of a name. Each value of one filter
widens the match, and different filters must all hold. On the traces list, type and metadata keep traces that have a
matching span; the other filters test the trace's head span. Type and metadata cannot be combined on the traces list;
the spans list filters by both.

## The trace page

Choosing a trace opens its page: the trace's name, start, duration, span count, tokens and cost, and three views of
its spans.

- **The tree and timeline.** Each span on one row under its parent, with its type and a bar for its duration on
  the trace's timeline. A trace with more than 5,000 spans opens with its root spans and their direct children, and
  expanding a span loads its children.
- **Agent graph.** The agents, tools and model calls of the trace as a graph, each linked to its place in the
  timeline.
- **Span details.** Choosing a span opens its overview (status, level, model, tokens, cost, duration, its parent
  and child spans, its id) and its input, output, attributes and events. Model conversations show as messages by
  role (system, developer, user, assistant, tool); other JSON as a tree; text as text.

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
