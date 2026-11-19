# REST API

The REST API reads back what Crucible stored: traces, their spans and payloads, and full-text search over them. This
page covers those reads. Sign-in, the organization, projects and API keys are in the OpenAPI document
(`specs/openapi/crucible-v1.json`) and the [authorization matrix](../architecture/authz-matrix.md). The rest of the
API that [contract 12](../architecture/contracts.md#12-api) describes, from dashboards to alerts and exports, is
planned.

The examples use four shell variables: the server's main listener, a project's secret and public keys, and that
project's id.
Every example on this page was run against a `crucible serve` binary with ClickHouse, and the responses are the
ones it gave.

```sh
export CRUCIBLE_URL=http://127.0.0.1:8080
export CRUCIBLE_SECRET_KEY=csk_...
export CRUCIBLE_PUBLIC_KEY=cpk_...
export PROJECT_ID=0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b
```

## Base path and versioning

The API lives under `/api/v1` on the main listener (`server.listen`, default `0.0.0.0:8080`). Paths under `/api/`
never fall back to the UI: an unknown path answers a problem 404, and a method a route does not take answers 405
with an `Allow` header. The read routes answer `HEAD` as well as `GET`.

Changes to `/api/v1` will be additive: responses may gain fields, so a client should ignore fields it does not
know. A breaking change would get a new base path. The `Deprecation` and `Sunset` headers that would announce a
retirement are planned.

Requests, in turn, are checked strictly: an unknown query parameter, a repeated one or an unknown member of a JSON
body is refused with `validation_failed`. A request body may hold at most 1 MiB, and a request that has not
finished after 30 s answers `request_timeout`. Every response carries an `X-Request-Id` header, a UUIDv7 that
problem documents repeat as `request_id`; an `X-Request-Id` sent by the client is ignored.

A server started without ClickHouse only ingests. Its API still checks the key and the parameters of every request,
then answers 503 `telemetry_store_unavailable`.

## Routes

All paths are under `/api/v1`, and `{project_id}` is the project's id.

| Method and path | Returns |
|---|---|
| `GET projects/{project_id}/data-bounds` | Where the project's data begins and ends, its retention horizon, the widest window a request may ask for and the server's clock |
| `GET projects/{project_id}/traces` | One page of traces whose head span started in the window, newest first; with `since`, the traces that arrived after a head cursor (live tail) |
| `GET projects/{project_id}/traces/{trace_id}` | One trace with its spans: every span up to 5,000 spans; above that the root spans and their direct children, at most 1,000, with `paged: true` |
| `GET projects/{project_id}/traces/{trace_id}/spans` | One page of the direct children of `parent_span_id`, in start order |
| `GET projects/{project_id}/spans/{trace_id}/{span_id}/payload` | One payload field of a span as raw bytes, whole or as a byte range |
| `POST projects/{project_id}/search` | One page of spans whose input, output or metadata hold a query, newest first, with the matching snippets |

The head span of a trace is its root span, or the earliest span while the root has not arrived. Ids are lowercase
hex in responses; uppercase hex in a path is accepted.

## Authentication

Send the project's secret key as a Bearer token:

```text
Authorization: Bearer <secret key>
```

With Postgres the key is made in Settings or with `crucible admin create-key`; without Postgres it comes from the
server's keys file (`server.keys_file`, described in [sending traces](../guides/sending-traces.md#authentication)).
Either way it can read only its own project.
`{project_id}` must be that project: any other id, a malformed one included, answers 404 `not_found`, so a key
learns nothing about projects it cannot read. A missing or unknown key answers 401 `unauthenticated` with
`WWW-Authenticate: Bearer`.

With Postgres the API also takes the key pair as Basic auth, a key needs the scope of the call (`read` for every
read on this page), and a signed-in browser sends its session cookie instead, with the `X-Crucible-CSRF` header on
writes. Without Postgres a key in the file has the scopes of a new key (`ingest`, `read` and `write`), and the key
pair as Basic auth works on ingest but not on the API, where it answers 401:

```sh
curl -s -u "$CRUCIBLE_PUBLIC_KEY:$CRUCIBLE_SECRET_KEY" "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/data-bounds"
```

```json
{
  "type": "urn:crucible:problem:unauthenticated",
  "title": "Unauthenticated",
  "status": 401,
  "detail": "a valid secret key is required as `Authorization: Bearer <key>`",
  "code": "unauthenticated",
  "request_id": "01a1f0b2-6d5a-7c3c-b1f4-b755386a1d75"
}
```

## Reading traces

Where the data is:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/data-bounds"
```

```json
{
  "spans": {
    "first": "2026-11-18T22:01:00.000000Z",
    "last": "2026-11-18T22:02:00.000000Z"
  },
  "mcp": {
    "client": {
      "first": null,
      "last": null
    },
    "server": {
      "first": null,
      "last": null
    }
  },
  "retention_horizon": null,
  "max_window_days": 30,
  "server_time": "2026-11-18T22:06:27.719510Z",
  "meta": {
    "as_of": "2026-11-18T22:06:27.719510Z",
    "partial": false,
    "consistency": "strong"
  }
}
```

`spans` and `mcp` give the first and last minute with data; the server does not record MCP operations yet, which is
planned, so `mcp` stays null. `retention_horizon` is null while the project keeps its data forever, which is the only
retention in this preview; retention settings are planned. `max_window_days` is the widest window a list or search may
ask for.

The newest trace in a window:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?from=2026-11-18T00:00:00Z&to=2026-11-19T00:00:00Z&limit=1"
```

```json
{
  "data": [
    {
      "trace_id": "5b8efff798038103d269b633813fc60c",
      "name": "hello crucible",
      "trace_name": "",
      "head_kind": "root",
      "start_time": "2026-11-18T22:02:33.790201Z",
      "duration_us": 250000,
      "status_code": "unset",
      "level": "default",
      "span_count": 1,
      "error_count": 0,
      "total_cost_usd": 0.0,
      "input_tokens": 0,
      "output_tokens": 0,
      "total_tokens": 0,
      "user_id": "",
      "session_id": "",
      "environment": "default",
      "release": "",
      "tags": [],
      "saturated": false
    }
  ],
  "next_cursor": "v1.master-0.AQEBAbkdQK_7Mk1UoLw52o1sWfjjsMRCmPwcFJr79MiZb7kkAAZglDj9LiEABmCoVtSOIQAGYIGxL6AAAAZglc8HAAAABmCUOP0uJAAGYJQrCl7598RE8FuO__eYA4ED0mm2M4E_xgw.7MPKMiRa1-vysBtkVV7Llw",
  "has_more": true,
  "head_cursor": "v1.master-0.AQIBAbkdQK_7Mk1UoLw52o1sWfjjsMRCmPwcFJr79MiZb7kkAAZglDj9LiEABmCoVtSOIQAGYIGxL6AAAAZglc8HAAAABmCUOP0uJAGh8Ko1nHyxhbf71s-RpDcAAAAAAAAACA.xTmbPXFonVh-dAzrmAH4Hw",
  "meta": {
    "as_of": "2026-11-18T22:06:27.806756Z",
    "partial": false,
    "consistency": "strong"
  }
}
```

The next page, with the same window and the `next_cursor` of the page before:

```sh
CURSOR='<next_cursor of the previous page>'
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?from=2026-11-18T00:00:00Z&to=2026-11-19T00:00:00Z&limit=1&cursor=$CURSOR"
```

```json
{
  "data": [
    {
      "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
      "name": "plan trip",
      "trace_name": "trip planner",
      "head_kind": "root",
      "start_time": "2026-11-18T22:01:33.790201Z",
      "duration_us": 1200000,
      "status_code": "unset",
      "level": "default",
      "span_count": 3,
      "error_count": 1,
      "total_cost_usd": 0.0042,
      "input_tokens": 1200,
      "output_tokens": 300,
      "total_tokens": 1500,
      "user_id": "user-7",
      "session_id": "conv-42",
      "environment": "staging",
      "release": "1.4.0",
      "tags": [
        "beta"
      ],
      "saturated": false
    }
  ],
  "next_cursor": null,
  "has_more": false,
  "head_cursor": null,
  "meta": {
    "as_of": "2026-11-18T22:06:27.806756Z",
    "partial": false,
    "consistency": "strong"
  }
}
```

One trace:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces/4bf92f3577b34da6a3ce929d0e0e4736"
```

```json
{
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "span_count": 3,
  "paged": false,
  "spans": [
    {
      "span_id": "00f067aa0ba902b7",
      "parent_span_id": null,
      "name": "plan trip",
      "observation_type": "agent",
      "start_time": "2026-11-18T22:01:33.790201Z",
      "end_time": "2026-11-18T22:01:34.990201Z",
      "duration_us": 1200000,
      "status_code": "unset",
      "level": "default",
      "model": "",
      "total_cost_usd": 0.0,
      "input_tokens": 0,
      "output_tokens": 0,
      "total_tokens": 0,
      "child_count": null,
      "io_shape": "raw",
      "saturated": false
    },
    {
      "span_id": "b7ad6b7169203331",
      "parent_span_id": "00f067aa0ba902b7",
      "name": "chat",
      "observation_type": "generation",
      "start_time": "2026-11-18T22:01:33.890201Z",
      "end_time": "2026-11-18T22:01:34.590201Z",
      "duration_us": 700000,
      "status_code": "unset",
      "level": "default",
      "model": "model-large-2026-05",
      "total_cost_usd": 0.0042,
      "input_tokens": 1200,
      "output_tokens": 300,
      "total_tokens": 1500,
      "child_count": null,
      "io_shape": "raw",
      "saturated": false
    },
    {
      "span_id": "e457b5a2e4d86bd1",
      "parent_span_id": "00f067aa0ba902b7",
      "name": "lookup_order",
      "observation_type": "tool",
      "start_time": "2026-11-18T22:01:34.690201Z",
      "end_time": "2026-11-18T22:01:34.890201Z",
      "duration_us": 200000,
      "status_code": "error",
      "level": "error",
      "model": "",
      "total_cost_usd": 0.0,
      "input_tokens": 0,
      "output_tokens": 0,
      "total_tokens": 0,
      "child_count": null,
      "io_shape": "raw",
      "saturated": false
    }
  ],
  "meta": {
    "as_of": "2026-11-18T22:06:27.922435Z",
    "partial": false,
    "consistency": "strong"
  },
  "saturated": false
}
```

The trace detail takes no window: it finds the trace's time bounds itself. It accepts `hint_time`, the trace's
start from the list, and checks it, but does not need it yet. A trace with more than 5,000 spans comes back with
`paged: true`, its root spans and their direct children, each with `child_count`; read the levels below with the
children route:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces/4bf92f3577b34da6a3ce929d0e0e4736/spans?parent_span_id=00f067aa0ba902b7"
```

```json
{
  "data": [
    {
      "span_id": "b7ad6b7169203331",
      "parent_span_id": "00f067aa0ba902b7",
      "name": "chat",
      "observation_type": "generation",
      "start_time": "2026-11-18T22:01:33.890201Z",
      "end_time": "2026-11-18T22:01:34.590201Z",
      "duration_us": 700000,
      "status_code": "unset",
      "level": "default",
      "model": "model-large-2026-05",
      "total_cost_usd": 0.0042,
      "input_tokens": 1200,
      "output_tokens": 300,
      "total_tokens": 1500,
      "child_count": 0,
      "io_shape": "raw",
      "saturated": false
    },
    {
      "span_id": "e457b5a2e4d86bd1",
      "parent_span_id": "00f067aa0ba902b7",
      "name": "lookup_order",
      "observation_type": "tool",
      "start_time": "2026-11-18T22:01:34.690201Z",
      "end_time": "2026-11-18T22:01:34.890201Z",
      "duration_us": 200000,
      "status_code": "error",
      "level": "error",
      "model": "",
      "total_cost_usd": 0.0,
      "input_tokens": 0,
      "output_tokens": 0,
      "total_tokens": 0,
      "child_count": 0,
      "io_shape": "raw",
      "saturated": false
    }
  ],
  "next_cursor": null,
  "has_more": false,
  "meta": {
    "as_of": "2026-11-18T22:06:27.986906Z",
    "partial": false,
    "consistency": "strong"
  }
}
```

A payload field of one span: `field` is one of `input`, `output`, `system_instructions`, `attributes`,
`resource_attributes`, `events`, `links`, `tool_definitions` and `tool_calls`, and `start_time` is the span's
`start_time` as the API returned it, which locates the payload.

```sh
START='<start_time of the span>'
curl -s -i -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/spans/4bf92f3577b34da6a3ce929d0e0e4736/00f067aa0ba902b7/payload?field=input&start_time=$START"
```

```text
HTTP/1.1 200 OK
content-type: text/plain; charset=utf-8
accept-ranges: bytes
etag: "4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1795039293790201-input-25"
cache-control: private, max-age=31536000, immutable
content-security-policy: default-src 'none'; frame-ancestors 'none'
x-content-type-options: nosniff
referrer-policy: same-origin
cross-origin-opener-policy: same-origin
cross-origin-resource-policy: same-origin
content-length: 25
x-request-id: 01a1f0b2-6ed1-7c95-a142-c9c8d80f385d

Where should I go in May?
```

The body is the field's bytes as stored, always as `text/plain`, so a browser never renders it. A field that the
span lists in `truncated_fields` was cut at the size cap and may no longer be valid JSON (see
[size caps](../guides/sending-traces.md#size-caps)). A `Range: bytes=<first>-[<last>]` header asks for part of the
field and gets 206 with `Content-Range`; a range that starts past the end gets 416 with
`Content-Range: bytes */<length>`. `download=1` adds `Content-Disposition: attachment`. A missing payload answers
404.

## Search

```sh
curl -s -X POST -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/search" \
  -d '{"query": "lisbon", "from": "2026-11-18T00:00:00Z", "to": "2026-11-19T00:00:00Z"}'
```

```json
{
  "data": [
    {
      "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
      "span": {
        "span_id": "00f067aa0ba902b7",
        "parent_span_id": null,
        "name": "plan trip",
        "observation_type": "agent",
        "start_time": "2026-11-18T22:01:33.790201Z",
        "end_time": "2026-11-18T22:01:34.990201Z",
        "duration_us": 1200000,
        "status_code": "unset",
        "level": "default",
        "model": "",
        "total_cost_usd": 0.0,
        "input_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
        "child_count": null,
        "io_shape": "raw",
        "saturated": false
      },
      "matches": [
        {
          "field": "output",
          "fragments": [
            {
              "text": "Lisbon",
              "highlighted": true
            }
          ],
          "cut_before": false,
          "cut_after": false
        }
      ]
    }
  ],
  "next_cursor": null,
  "has_more": false,
  "meta": {
    "as_of": "2026-11-18T22:06:28.102395Z",
    "partial": false,
    "consistency": "strong"
  }
}
```

| Member | Meaning |
|---|---|
| `query` | Required; at most 1,024 bytes and 64 tokens. For `input` and `output` it splits into tokens at ASCII punctuation, spaces and controls, compared without regard to ASCII case. For `metadata` each whitespace-separated term is a whole `key=value` entry, compared as written |
| `mode` | `all` (default): every token occurs. `any`: at least one does. `phrase`: the tokens occur in order and adjacent |
| `fields` | Any of `input`, `output` and `metadata`, each once; default `input` and `output` |
| `from`, `to` | Required: the window over the spans' start times |
| `limit` | 1 to 200, default 50 |
| `cursor` | The `next_cursor` of the previous page, with the same query, mode, fields and window |

Search reads ClickHouse only, so a span acknowledged a moment ago may be missing; the page then says
`consistency: lagging`.

## Time windows

The trace list and search require `from` and `to`. The window is half open, `[from, to)`, over the start time of
each trace's head span (for search, of each span). It may be at most the project's maximum, 30 days by default; a
window of exactly the maximum passes. In this preview the maximum cannot be changed, and `data-bounds` reports it as
`max_window_days`. The trace detail, the children route and `data-bounds` take no window, and the payload route
takes the span's `start_time` instead.

Times are RFC 3339. Inputs may use any offset and precision and must fall between 1970 and 2100; responses are
always UTC with six fractional digits, such as `2026-11-18T09:30:00.000000Z`.

A window wider than the maximum:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?from=2026-10-01T00:00:00Z&to=2026-11-19T00:00:00Z"
```

```json
{
  "type": "urn:crucible:problem:query_too_broad",
  "title": "Query too broad",
  "status": 422,
  "detail": "the window is longer than the project's maximum of 30 days",
  "code": "query_too_broad",
  "request_id": "01a1f0b2-6f4c-7ebb-8341-aa3ba7c09c71",
  "limit": {
    "name": "max_window_days",
    "value": 30
  }
}
```

## Pagination and cursors

A list answers `{ data, next_cursor, has_more, meta }`, with no total count. `next_cursor` is null on the last
page. To read the next page, send the same request with `cursor` set to `next_cursor`.

| List | `limit` |
|---|---|
| Trace list | 1 to 200, default 50 |
| Children of a span | 1 to 2,000, default 500 |
| Search | 1 to 200, default 50 |

- **Cursors are signed.** A cursor reads `v1.<key id>.<payload>.<mac>`: the payload holds the position, and the MAC
  is an HMAC-SHA256 over the rest. Treat it as an opaque string. It is bound to the project, the route, the
  filters, the sort and the window of the request that issued it, and it expires 24 hours after it was issued. Any
  other cursor answers 400 `cursor_invalid`; start again from the first page.
- **Pages do not shift.** The first page fixes `meta.as_of`, and later pages show only traces that arrived at or
  before it, so new traces never push rows onto a page you have not read yet.
- **Cursors survive restarts.** The signing key is derived from the master key, which every server with ClickHouse
  has, so a cursor still opens after a restart, and every server started with the same master key opens the
  cursors of the others.
- **Live tail.** A first page of the trace list also carries `head_cursor`. Send it as `since`, with the same
  window and without `cursor`, to get the traces that arrived after it, with `new_count` (how many arrived, counted
  up to 100) and a fresh `head_cursor` for the next poll.

A cursor that does not open:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?from=2026-11-18T00:00:00Z&to=2026-11-19T00:00:00Z&cursor=v1.master-0.AAAA.AAAA"
```

```json
{
  "type": "urn:crucible:problem:cursor_invalid",
  "title": "Cursor invalid",
  "status": 400,
  "detail": "the cursor is malformed; restart from the first page",
  "code": "cursor_invalid",
  "request_id": "01a1f0b2-6f79-77e8-a390-f068770f900c"
}
```

## Response metadata

Every JSON response of the routes above carries `meta`:

| Member | Meaning |
|---|---|
| `as_of` | The instant the response describes; for a list, the one its first page fixed |
| `partial` | True when rows may be missing, for the reason in `partial_reason`: `store_unavailable`, `deadline` or `quarantined` |
| `consistency` | `strong`: the response reflects every span this server acknowledged before the request arrived. `lagging`: it may trail |

Counts are JSON numbers and stop at 2^53 - 1; an object with a count that stopped there has `saturated: true`.
Costs are JSON numbers in USD, in fields named `*_usd`.

## Errors

Every error is a problem document (RFC 9457) with the media type `application/problem+json`:

```sh
curl -s -i -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?limit=500&sort=name"
```

```text
HTTP/1.1 422 Unprocessable Entity
content-type: application/problem+json
content-security-policy: default-src 'none'; frame-ancestors 'none'
x-content-type-options: nosniff
referrer-policy: same-origin
cross-origin-opener-policy: same-origin
cross-origin-resource-policy: same-origin
content-length: 561
x-request-id: 01a1f0b2-6fa6-71e3-93eb-24eef63afd76

{"type":"urn:crucible:problem:validation_failed","title":"Validation failed","status":422,"detail":"the request is invalid; see `errors`","code":"validation_failed","request_id":"01a1f0b2-6fa6-71e3-93eb-24eef63afd76","errors":[{"pointer":"/from","code":"required","message":"the parameter is required"},{"pointer":"/to","code":"required","message":"the parameter is required"},{"pointer":"/limit","code":"out_of_range","message":"expected an integer from 1 to 200"},{"pointer":"/sort","code":"unknown_parameter","message":"the endpoint has no such parameter"}]}
```

| Member | Meaning |
|---|---|
| `type` | `urn:crucible:problem:<code>` |
| `title`, `detail` | For logs and developers; show your own text to users |
| `status` | The HTTP status |
| `code` | The stable code: branch on this |
| `request_id` | The same value as the `X-Request-Id` header |
| `errors` | For `validation_failed`: one entry per invalid part, with a JSON `pointer` (`/from` for the query parameter `from`), a `code` and a `message` |
| `limit` | For `query_too_broad`: the limit that was exceeded, such as `{"name": "max_window_days", "value": 30}` |

The codes the routes on main answer:

| Code | Status | When |
|---|---|---|
| `cursor_invalid` | 400 | The cursor is tampered, expired, from another project, or bound to other filters, sort or window |
| `unauthenticated` | 401 | No secret key, or one the server does not accept |
| `not_found` | 404 | An unknown path, a project the key cannot read, an unknown trace, or a payload that is not there |
| `method_not_allowed` | 405 | The route does not take the method; see `Allow` |
| `request_timeout` | 408 | The request did not finish within 30 s |
| `payload_too_large` | 413 | The body is above 1 MiB |
| `validation_failed` | 422 | A path, query or body value is invalid; see `errors` |
| `query_too_broad` | 422 | The window is above the maximum, or the read would cover too much; see `limit` |
| `query_memory_exceeded` | 422 | The read hit its memory limit |
| `telemetry_store_unavailable` | 503 | ClickHouse is not configured, or it is unreachable and the recent-span index cannot answer |
| `query_timeout` | 504 | The read hit its time limit |
| `internal_error` | 500 | Anything else |

The codes in `errors[].code` are stable too: `required`, `repeated`, `unknown_parameter`, `invalid_timestamp`,
`out_of_range`, `invalid_window`, `invalid_id`, `invalid_query`, `conflict` (`since` with `cursor`), `invalid`,
`too_long`, `too_many_tokens` and `no_tokens`.

The API's code list already holds the codes of planned features, and the OpenAPI document lists them all:
`invalid_credentials`, `forbidden`, `csrf_invalid`, `origin_invalid`, `conflict`, `idempotency_key_reused`,
`precondition_failed`, `unsupported_media_type`, `token_invalid`, `precondition_required`, `rate_limited`,
`control_plane_unavailable`, `ingest_unavailable`, `feature_disabled`, `ssrf_blocked`, `media_not_supported`,
`media_too_large`, `media_hash_mismatch`, `sql_invalid`, `sql_forbidden`, `sql_time_window_required`,
`sql_quota_exceeded`, `sql_workspace_unavailable` and `export_expired`. No route on main answers them yet.

## Concurrency and idempotency

Every route on main reads: `GET` and `HEAD`, and `POST` for search, which changes nothing. So no route needs
`If-Match` or `Idempotency-Key`, and none reads them. Both are planned for the routes that will change data:

- Mutable resources such as dashboards and settings will carry a `version`, also sent as `ETag`. `PATCH`, `PUT` and
  `DELETE` will require `If-Match`, answering `precondition_required` without it and `precondition_failed` when
  it does not match.
- Creates of exports, deletion requests, media and fact writes will honour `Idempotency-Key` (1 to 128 characters
  of `[A-Za-z0-9_-]`) for 24 hours: a replay returns the first response, and a different request under the same key
  answers `idempotency_key_reused`.

The payload route already sends an `ETag` and `Cache-Control: private, max-age=31536000, immutable`, because a
stored payload never changes. OTLP ingest does not read `Idempotency-Key`; how it handles a retried export is
described under [retries and duplicates](../guides/sending-traces.md#what-an-acknowledgement-means).

## Rate limits

The API has no rate limits in this preview. Limits per key and per session, which will answer 429 `rate_limited`
with `Retry-After` and `retry_after_s`, are planned
([contract 11.9](../architecture/contracts.md#119-rate-limits)). Ingest is not rate limited either: when the server
cannot take more, it answers 429 or 503 with `Retry-After`, as
[sending traces](../guides/sending-traces.md#what-an-acknowledgement-means) describes.

## OpenAPI document

The OpenAPI 3.1 document of the API is committed at `specs/openapi/crucible-v1.json`. It is generated from the
route handlers, and `crucible openapi` prints the same document from the binary; CI fails when the committed file
no longer matches the handlers. Its `info.version` is the binary's version. The server does not serve the document
over HTTP yet; that is planned.
