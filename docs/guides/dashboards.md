# Dashboards

A dashboard is a page of widgets over a project's metrics: charts, single numbers, tables and text, laid out on a
grid of 12 columns. Every widget reads the metrics that `POST metrics/query` serves ([metrics](../api/README.md#metrics))
over the time range picked at the top of the page. This guide covers the starter dashboards, your own dashboards, the
editor, the widget kinds and their limits, and who can change what.

## Starter dashboards

Every project has five built-in dashboards. They are read-only and are served with or without a control plane.

| Starter | What it shows |
|---|---|
| Agent overview | Traces, spans, errors and cost at a glance |
| Cost and tokens | What the models cost, and the tokens behind it |
| Latency | How long spans take, and how long models take to answer |
| Errors | Where spans fail: by type, model and environment |
| MCP servers | Calls, errors, error rate and latency of the MCP servers, by server and tool, on the server side |

To change a starter, duplicate it: the copy is one of your dashboards, named after the starter with " (copy)".

## Your dashboards

**Dashboards** in the navigation lists your dashboards, oldest first, then the starters. **New dashboard** asks for a
name and opens the new dashboard in the editor, where you add widgets (on a screen narrower than 768 px, it opens the
dashboard itself). Your dashboards are stored in Postgres, so a server without a control plane (no `postgres.url`)
shows the starters only.

A dashboard page draws each widget on its own: one that is loading, failed (with a retry for that widget) or has no
data leaves the others as they are. On a screen at least 768 px wide the widgets sit on the 12-column grid, and a
widget whose content needs more room grows taller instead of clipping; on a narrower screen they stack in one column,
in reading order.

**Duplicate** makes a copy you can edit and opens it. On the project's own dashboards, members and admins also get
**Edit** and **Delete**; Delete asks first, and removes the dashboard and its widgets for everyone in the project.

## Editing

The editor needs a screen at least 768 px wide; on a narrower one the dashboard says so instead of offering Edit.

- **Add a widget** opens a sheet: the title, the kind, the data it reads (agent spans, traces or MCP operations),
  the measures, up to two group-by dimensions, how many keys to show, the bucket width, and for a time series lines,
  bars or areas. For agent spans, **Filters** keeps the spans whose environment, release, level or type is, or is
  not, a value: level and type are chosen from their lists, and environment and release offer the values the time
  range holds as you type, or take a value as written. Each filter is a chip that removes it. A widget over MCP
  operations reads one measure, from one side (the server side unless you choose the client side) and one capture
  source (Automatic picks each server's SDK, else its proxy, else OTLP), and counts calls that failed because of
  what the caller sent only when **Include caller faults** is on; a second measure is refused when you save. A
  preview draws the widget as soon as it is complete. A text widget takes its text instead of a query.
- **Layout**: drag a widget by its move handle and resize it by its corner, or focus a handle and use the arrow keys.
  A move that would overlap another widget is refused and announced.
- **Save** stores the name, the description and every widget in one change, so a layout that moves several widgets
  is kept whole or not at all. If someone saved the dashboard after you opened it, the editor offers **Load their
  version** (which drops your changes) or saving yours over it.

## Widget kinds

| Kind | What it shows | Query | Smallest size |
|---|---|---|---|
| Time series | The measures over time, as lines, bars per bucket, or areas (a light fill under each line, the series laid over one another) | Measures of one unit, at most four series: up to four measures, or one measure split by dimensions with `top_n` keys and the `other` series fitting in four | 3 by 2 |
| Stat | One number for the time range, with its trend | One measure, no dimensions | 2 by 1 |
| Bar | One measure for each key, as horizontal bars | One measure, split by one or two dimensions | 3 by 2 |
| Table | The measures of each key | Any metrics query | 3 by 2 |
| Text | Headings, lists, emphasis, code and links, up to 10,000 characters | None | 2 by 1 |

The measures, dimensions, filters and bucket widths are those of the metrics schema
([metrics](../api/README.md#metrics)). A widget's query is checked like a `metrics/query` body, and the dashboard as a
whole on every save:

- at most 50 widgets;
- a widget at most 12 columns wide and 12 rows tall, no smaller than its kind allows, and never overlapping another;
- a name and widget titles of 1 to 120 characters, and a description of at most 1,000.

A refused save names the widget and the member to fix.

## Who can edit

| Who | Read dashboards | Create, edit, duplicate and delete |
|---|---|---|
| Project viewers | Yes | No |
| Project members and admins | Yes | Yes |
| API keys with `read` | Yes | No: keys never edit dashboards |

So dashboards are changed in the UI, or over the API with a signed-in session. The routes, the `ETag` that every change
carries and the answers to a conflict are in the [API reference](../api/README.md#dashboards-and-widgets).
