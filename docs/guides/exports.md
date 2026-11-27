# Exports

An export writes a project's spans of a time window to a file: every span, or only the spans of some traces, of one
user or of one session. Exports start from the UI, the REST API, the MCP server or the `crucible export` command,
and need the control plane, except the command, which reads the store directly.

## Formats

| Format | One line per span |
|---|---|
| `crucible-jsonl` (**Crucible JSON lines**) | An object with `schema` (`crucible.span/1`), `span` (every field of the span record, both tiers, with its MCP operation under `span.mcp`) and `facts` (the span's and its trace's effective tags, scores, flags and corrections) |
| `otlp-json` (**OTLP JSON**) | One OTLP JSON document per span, with the OpenTelemetry GenAI keys and every other attribute. A span with a field cut at a size cap is left out, unless `keep_truncated` asks for it, as opaque text |

Either can be compressed with gzip.

## From the UI

**Settings**, **Exports** lists the project's exports with their state (**Queued**, **Ready**, **Failed** or
**Expired**) and a **Download** link while a file is ready. **Export spans** starts one over the time range at the top
of the page, in either format, optionally for one user or one session, optionally compressed. A project can start 5
exports an hour. A file is kept 7 days.

## From the API

```sh
curl -s -X POST -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: nightly-2026-11-19' "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/exports" \
  -d '{"format": "crucible-jsonl", "from": "2026-11-19T00:00:00Z", "to": "2026-11-21T00:00:00Z", "gzip": true}'
```

The answer is the export, `queued`. While it is `queued` or `running`, it carries `attempts` (the tries at writing
the file, 0 before the first; a failed attempt is tried again after a pause, up to 12 attempts), `waited_secs` (the
seconds since it was created) and `last_error` (what went wrong in the latest failed attempt, and what to do); a
failed export's `last_error` says why it ended. The list of exports carries the same fields.

Poll `GET .../exports/{export_id}` until `state` is `ready`, then download the
file from its `download_path` with the same key; the file's SHA-256 is the export's `checksum`. With object storage
the download answers `303` with a link to the file on the object store that works for 5 minutes, so follow it
(`curl -L`), and any server of the deployment answers; without object storage the server that wrote the file
streams it. The body takes
`trace_ids` (up to 10,000), `user_id` or `session_id` to narrow the spans, `gzip` and `keep_truncated`, and creating
an export needs the `read` scope or the member role. The [API reference](../api/README.md#the-rest-of-the-api)
shows the whole exchange.

## From the MCP server

A coding agent connected to `/mcp` calls `create_export` with the same arguments, then `get_export` until the export
is ready, and downloads the file from the REST API with its key ([MCP server](mcp-server.md)).

## With `crucible export`

The command reads the telemetry store with the server's configuration and writes to a file or to standard output,
without a job, a download link or the hourly limit:

```sh
docker compose exec -T crucible crucible export --format crucible-jsonl --project default \
  --from 2026-11-19T00:00:00Z --to 2026-11-21T00:00:00Z > spans.jsonl
```

It prints a summary on standard error, such as `exported 7 spans of project 01a1f613-... (0 skipped for a cut field)
as crucible-jsonl: 21894 bytes, sha256 9c065036...`. `--project` takes a slug or an id with a control plane, and a
project id from the keys file without one. `--trace-id` (repeatable), `--user-id` and `--session-id` narrow the
spans, `--gzip` compresses, `--keep-truncated` keeps cut spans in `otlp-json`, and `-o <file>` writes to a file.

## Validating models with rightmodeler

[rightmodeler](https://github.com/elm-os/rightmodeler) replays the model calls of a `crucible-jsonl` export to
validate cheaper or newer models against the outputs you accepted. Export the window you want rightmodeler to learn
from, in `crucible-jsonl`, and give the file to rightmodeler's `--traces` option, which recognizes the format by its
`schema` key. rightmodeler reads each generation's model, messages, output, token usage and cost, the trace's session
as the trajectory, and a span cut at Crucible's size cap as incomplete. Its own documentation covers the rest of the
workflow, from the call sites it ties the traces to, to the replay and the draft pull request.

## Deletion and expiry

An export file expires 7 days after it is written. A deletion request expires the export files made before it, and
an expired download answers 410 `export_expired`
([retention and deletion](../self-hosting/retention-and-deletion.md)).

## Where files are kept

Without object storage, each file is on the disk of the server that wrote it, under its data directory, and only that
server can send it. With object storage ([media](../self-hosting/media.md)), files go to the same bucket as media,
under `<prefix>/<project id>/exports/`, every server can hand out a link to them, and a deletion request or the
expiry removes the object.
