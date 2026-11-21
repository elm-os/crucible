# Retention and deletion

Each project keeps its data for as long as its retention settings say, forever by default. Data can also be deleted
on request: a list of traces, or everything of one user or one session, and deleting a project deletes all of its
data. All of this needs the control plane (Postgres).

## Retention

A project has three settings, all in days. Project admins change them in **Settings**, **Retention**, or through
`PUT /api/v1/projects/{project_id}/retention` ([API](../api/README.md)).

| Setting | Field in the API | What it keeps |
|---|---|---|
| Keep data for | `retention_days` | Everything of the project, counted from when each span started. Empty (`null`) keeps data forever, the default |
| Keep inputs and outputs for | `payload_retention_days` | Span inputs, outputs, attributes and the other large fields, and corrections. At most `retention_days`; empty keeps them as long as the data |
| Keep MCP arguments and results for | `mcp_content_retention_days` | The captured arguments and results of MCP operations. At most `retention_days`; empty keeps them as long as the data |

- Reads hide expired data when the setting is saved: the API, the UI, the MCP server and exports stop showing data
  older than the cut-off at once. The SQL workspace follows within 5 minutes, when the job that keeps each project's
  ClickHouse user rewrites the project's row policies ([SQL workspace](sql-workspace.md#provisioning)).
- Shortening retention applies to the data already stored. Lengthening it never brings back data that a shorter
  setting already hid; `GET .../retention` reports what stays hidden in `hidden_before`.
- Ingest refuses spans older than the cut-off: a span that started before it is rejected through OTLP partial
  success, as one that started more than 400 days ago is
  ([sending traces](../guides/sending-traces.md#what-an-acknowledgement-means)), so expired data is not stored
  again. Every server applies a changed setting within 5 seconds.
- The retention job runs hourly on the server that holds the `lifecycle` lease. It drops whole partitions where it
  can and deletes the expired rows of the rest, at day boundaries in UTC. ClickHouse removes deleted rows from
  storage within 24 hours.

## Deletion requests

A deletion request removes data from a project for good:

| Kind | Selects | Who may request it |
|---|---|---|
| `trace` | 1 to 10,000 trace ids | Project members and admins; keys with the `delete` scope |
| `user` | Every trace of one `user_id`, with its facts and MCP operations | Project admins; keys with the `delete` scope |
| `session` | Every trace of one `session_id`, with its facts and MCP operations | Project admins; keys with the `delete` scope |
| `project` | Everything of the project | Made by deleting the project ([below](#deleting-a-project)) |

Request one of the first three in **Settings**, **Retention**, **Delete data**, or with
`POST /api/v1/projects/{project_id}/deletion-requests`. A deletion is point in time: data received after the
request is new data and is kept.

A request moves through these states, which the settings page shows with the place-by-place progress:

| State | Shown as | What holds |
|---|---|---|
| `pending` | Hidden | The data is hidden from every page, API answer and MCP tool answer: at once on the server that took the request, within 5 seconds on the others. The servers are recording the request in their write-ahead logs |
| `logged` | Deleting | Every server has recorded it; the deletes are running |
| `applied` | Deleted | Every place reports its delete done, and the data is gone from every read, the SQL workspace included |
| `verified`, `completed` | Removed | Each place was counted again and is empty, and the deleted rows are removed from storage |
| `failed` | Failed | A step ran out of attempts; the server log says which. Some of the data may be readable again |

The places a request reaches are the span tables, the trace heads and trace statistics, the session and user
rollups, the four fact tables (scores, tags, flags, corrections), the MCP operations and their arguments and
results, export files made before the request, which expire and are removed (from the object store once the
expiry is recorded, when export files are kept there), and with an object store, the uses of media by the traces
deleted: media follow `payload_retention_days`, and an object is removed once nothing uses it
([media](media.md#deletion-and-retention)). The minute rollups that power charts and alerts, and the hourly rollup
of MCP calls, hold counts with no user, session or content, and are kept, unless the whole project is deleted.

A request also reaches data that is not in ClickHouse yet. Spans received before the request that still wait in a
server's write-ahead log, because ClickHouse was slow or unreachable, are dropped before they are sent, and the
in-memory index of recent spans forgets them at once. A replay of the log after a restart therefore never stores
deleted data again.

Typically a request reaches `applied` within minutes, once every server's write-ahead log has passed it, and
physical removal follows within 24 hours. A server that is down does not hold a request back. When it starts again
it reads from Postgres every deletion request made after the oldest data in its write-ahead log arrived, before it
sends ClickHouse the data it had not sent yet, so it does not store again what a request deleted while it was down.
Older requests are left out, because a deletion selects only data received before the request. Until Postgres
answers, the server keeps accepting data into its write-ahead log and sends nothing. An export reads the stored
rows, so an export made while a request is still `pending` or `logged` can include the data it names.

## Deleting a project

An org owner or admin deletes a project with `DELETE /api/v1/projects/{project_id}`. The answer comes at once, and
the project leaves every list; its API keys stop working, on every server within 5 seconds. Its dashboards with
their widgets and its media records go with it, and with an object store every object under the project's key prefix
is deleted next ([media](media.md#deletion-and-retention)). A trace that records media in the project while it is
being deleted makes the request answer 503 `control_plane_unavailable` with `Retry-After` and delete nothing; send it
again.

The rest of the project's data goes in the background, through a deletion request of kind `project` that the
deletion makes in the same transaction:

1. Every server records the request in its write-ahead log within 5 seconds, and data of the project that still
   waits there is dropped before it is sent.
2. Once every server's write-ahead log has passed the request, every table of the project is emptied: spans and
   their payloads, trace heads and trace statistics, MCP operations with their arguments and results, the four fact
   tables, the session and user rollups, the minute rollups, the hourly rollup of MCP calls, the MCP deploys and the
   tool inventory. A partition that holds rows of deleted projects only, and ended more than an hour before the
   deletion, is dropped whole; the project's rows are deleted from every other one. Rows that servers took in the
   seconds after the deletion, before their keys caught up, go too.
3. After the next mask cycle, within 24 hours, every table is checked to hold no row of the project and no deleted
   row on disk, and the request is `verified`.
4. Then the project's export files and every object left under its key prefix are removed, and so are its rows in
   Postgres: memberships, alert rules with their state and history, notification channels with their sealed secrets,
   saved queries, SQL runs, exports, the revoked API keys, its retention floor and its salt. The request is
   `completed`.

From the moment of deletion nothing runs for the project: its alert rules are not evaluated, nothing is sent to its
channels, an export waiting to run ends with `project_deleted`, and the SQL workspace drops the project's ClickHouse
user at its next pass. The project's row and its deletion requests stay in Postgres, so that a server that was down
reads them before it sends anything of the project it still holds.

Org owners and admins follow the purge with `GET /api/v1/projects/{project_id}/deletion-requests`, under the deleted
project's id: the request of kind `project` lists each table with its deletes, as any deletion request does, and the
place `control_plane` once the Postgres rows are gone. Nobody else can read a deleted project's requests. The purge
removes the data of projects deleted on this version; a project deleted on an earlier version keeps the telemetry it
left in ClickHouse, where no page, API answer or MCP tool reads it.
