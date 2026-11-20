# REST API

The REST API serves everything the UI shows and does: traces, spans, sessions and users, search and payloads,
facts, metrics, the SQL workspace, alerts and channels, exports, retention and deletion, and the organization,
projects, members and keys. This page covers how to call it and lists every route; the OpenAPI document
(`specs/openapi/crucible-v1.json`) has every request and response schema.

The examples use three shell variables: the server's main listener, a project's secret key and that project's id.
Every example on this page was run against a `crucible serve` binary with ClickHouse and Postgres, and the
responses are the ones it gave; long ones are cut where they repeat.

```sh
export CRUCIBLE_URL=http://127.0.0.1:8080
export CRUCIBLE_SECRET_KEY=csk_...
export PROJECT_ID=01a1f633-d59a-7014-8007-ae2dcfb94996
```

## Base path and versioning

The API lives under `/api/v1` on the main listener (`server.listen`, default `0.0.0.0:8080`). Paths under `/api/`
never fall back to the UI: an unknown path answers a problem 404, and a method a route does not take answers 405
with an `Allow` header. The read routes answer `HEAD` as well as `GET`.

Changes to `/api/v1` are additive: responses may gain fields, so a client should ignore fields it does not know. A
breaking change gets a new base path.

Requests are checked strictly: an unknown query parameter, a repeated one or an unknown member of a JSON body is
refused with `validation_failed`. A request body may hold at most 1 MiB, and a request that has not finished after
30 s answers `request_timeout` (SQL runs have their own limit). Every response carries an `X-Request-Id` header, a
UUIDv7 that problem documents repeat as `request_id`; an `X-Request-Id` sent by the client is ignored.

A server started without ClickHouse only ingests. Its API still checks the credential and the parameters of every
request, then answers 503 `telemetry_store_unavailable` for telemetry reads.

## Authentication

A request authenticates in one of three ways:

| Form | Header | Who uses it |
|---|---|---|
| A project's secret key | `Authorization: Bearer <secret key>` | Programs, agents and scripts |
| A project's key pair | `Authorization: Basic base64(<public key>:<secret key>)` | Programs whose exporter sends Basic auth; with Postgres only |
| A session | The session cookie, plus `X-Crucible-CSRF` on writes | The UI, and scripts that sign in as a person |

A request with an `Authorization` header is authenticated by the key alone; the cookie is ignored.

**Keys** are made in **Settings**, **API keys**, through `POST projects/{project_id}/api-keys`, or with
`crucible admin create-key`. A key belongs to one project and acts as a project principal with the permissions its
scopes grant: `ingest` (send telemetry), `read` (every read, exports and the MCP server), `write` (facts), `delete`
(deletion requests) and `sql` (SQL runs). A new key has `ingest`, `read` and `write`. A key never manages keys,
members, channels, alert rules or settings. `{project_id}` must be the key's project: any other id, a malformed one
included, answers 404 `not_found`, so a key learns nothing about projects it cannot read. A missing or unknown key
answers 401 `unauthenticated` with `WWW-Authenticate: Bearer`. Without Postgres the keys come from the server's keys
file, each with the scopes `ingest`, `read` and `write`, and the API takes them as Bearer only: the key pair as Basic
answers 401 there.

**Sessions** need the control plane. Sign in with `POST auth/login`; the server sets the session cookie,
`crucible_session` over plain HTTP or `__Host-crucible_session` with `Secure` when `server.public_url` is an `https`
address, valid for 7 days idle and 30 days at most. A cookie-authenticated request other than `GET` and `HEAD`
(`POST` reads such as `search` and `metrics/query` included) must carry the session's CSRF token, the `csrf_token`
of the sign-in answer or of `GET auth/session`, as `X-Crucible-CSRF`, and must come from the server's own origin:
`server.public_url` when set, otherwise the request's `Host`, or one of `server.allowed_origins`. The requests made
before a session exists (sign-in, first-owner setup, password reset, accepting an invitation) send
`X-Crucible-CSRF: prelogin` and pass the same origin check.

```sh
curl -s -c cookies.txt -X POST "$CRUCIBLE_URL/api/v1/auth/login" \
  -H 'Content-Type: application/json' -H 'X-Crucible-CSRF: prelogin' -H "Origin: $CRUCIBLE_URL" \
  -d '{"email": "owner@example.com", "password": "<password>"}'
```

The answer is the session: the user, the organization and the user's role in it, each project the user may see
with the user's role and permissions there, the sign-in options, `csrf_token`, the session's expiry, and the
server's features:

```json
{
  "user": {"id": "01a1f633-d59a-7014-8007-ae2dcfb94995", "email": "owner@example.com", "name": "Owner"},
  "org": {"id": "01a1f633-d59a-7014-8007-ae2dcfb94994", "name": "Acme", "role": "owner"},
  "projects": [
    {
      "id": "01a1f633-d59a-7014-8007-ae2dcfb94996",
      "name": "Default",
      "slug": "default",
      "role": "admin",
      "permissions": ["read", "write_facts", "edit_content", "run_sql", "export", "run_saved_query", "channels", "manage", "delete_traces", "delete_by_user"]
    }
  ],
  "providers": [],
  "password_login_enabled": true,
  "csrf_token": "<token>",
  "session": {"expires_at": "2026-12-20T00:11:32.825262Z", "idle_expires_at": "2026-11-27T00:38:46.795830Z", "auth_method": "password"},
  "features": {"sql_workspace": {"enabled": true}, "media": {"enabled": false, "max_bytes": 0}, "smtp": false, "signup_mode": "invite_only"},
  "server": {"version": "0.0.0"}
}
```

A write with the cookie then sends the token:

```sh
curl -s -b cookies.txt -X POST "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/api-keys" \
  -H 'Content-Type: application/json' -H "X-Crucible-CSRF: <csrf_token>" -H "Origin: $CRUCIBLE_URL" \
  -d '{"note": "exporter", "scopes": ["ingest"]}'
```

**Permissions.** Every route needs a permission. People get project permissions from their project role (`admin`,
`member`, `viewer`) and organization permissions from their organization role (`owner`, `admin`, `member`,
`viewer`); keys get project permissions from their scopes. A caller who may not see the project gets 404
`not_found`; one who sees it without the permission gets 403 `forbidden`, with a message that names the permission.
[The authorization matrix](../architecture/authz-matrix.md) lists the permission of every route and which roles and
scopes grant it; the tables below repeat it.

## Routes

All paths are under `/api/v1`. The permission column names the project permission a route needs, and the key scope
that grants it ("no key" means that only a signed-in person may call it); `org` permissions and `session` need a
signed-in person, and `none` needs no credential.

### Traces, spans and payloads

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/data-bounds` | Get data bounds | `read`; key `read` |
| `GET projects/{project_id}/spans` | List spans | `read`; key `read` |
| `GET projects/{project_id}/spans/{trace_id}/{span_id}/payload` | Read a span's payload field | `read`; key `read` |
| `GET projects/{project_id}/traces` | List traces | `read`; key `read` |
| `GET projects/{project_id}/traces/{trace_id}` | Get a trace | `read`; key `read` |
| `GET projects/{project_id}/traces/{trace_id}/spans` | List the children of a span | `read`; key `read` |

### Sessions

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/sessions` | List sessions | `read`; key `read` |
| `GET projects/{project_id}/sessions/{session_id}` | Get a session | `read`; key `read` |

### Users

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/users` | List users | `read`; key `read` |
| `GET projects/{project_id}/users/{user_id}` | Get a user | `read`; key `read` |
| `GET projects/{project_id}/users/{user_id}/sessions` | List the sessions of a user | `read`; key `read` |

### Search

| Method and path | What it does | Permission |
|---|---|---|
| `POST projects/{project_id}/search` | Search span payloads | `read`; key `read` |

### Metrics and filter values

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/filter-values` | Get filter values | `read`; key `read` |
| `POST projects/{project_id}/metrics/query` | Query metrics | `read`; key `read` |
| `GET projects/{project_id}/metrics/schema` | Get the metrics schema | `read`; key `read` |

### Dashboards

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/dashboards` | List dashboards | `read`; key `read` |
| `POST projects/{project_id}/dashboards` | Create a dashboard | `edit_content`; no key |
| `GET projects/{project_id}/dashboards/{dashboard_id}` | Get a dashboard | `read`; key `read` |
| `DELETE projects/{project_id}/dashboards/{dashboard_id}` | Delete a dashboard | `edit_content`; no key |
| `PATCH projects/{project_id}/dashboards/{dashboard_id}` | Change a dashboard | `edit_content`; no key |
| `POST projects/{project_id}/dashboards/{dashboard_id}/duplicate` | Duplicate a dashboard | `edit_content`; no key |
| `POST projects/{project_id}/dashboards/{dashboard_id}/widgets` | Add a widget | `edit_content`; no key |
| `DELETE projects/{project_id}/dashboards/{dashboard_id}/widgets/{widget_id}` | Delete a widget | `edit_content`; no key |
| `PATCH projects/{project_id}/dashboards/{dashboard_id}/widgets/{widget_id}` | Change a widget | `edit_content`; no key |

### Facts

| Method and path | What it does | Permission |
|---|---|---|
| `POST projects/{project_id}/corrections` | Correct an output | `write_facts`; key `write` |
| `GET projects/{project_id}/facts` | Get the facts of a target | `read`; key `read` |
| `POST projects/{project_id}/feedback` | Record feedback | `write_facts`; key `write` |
| `POST projects/{project_id}/flags` | Set a flag | `write_facts`; key `write` |
| `POST projects/{project_id}/tags` | Edit a tag | `write_facts`; key `write` |

### SQL workspace

| Method and path | What it does | Permission |
|---|---|---|
| `POST projects/{project_id}/sql/export` | Export SQL | `run_sql`; key `sql` |
| `POST projects/{project_id}/sql/run` | Run SQL | `run_sql`; key `sql` |
| `GET projects/{project_id}/sql/runs` | List SQL runs | `read`; key `read` |
| `DELETE projects/{project_id}/sql/runs/{run_id}` | Cancel a SQL run | `run_saved_query`; key `sql` |
| `GET projects/{project_id}/sql/saved-queries` | List saved queries | `read`; key `read` |
| `POST projects/{project_id}/sql/saved-queries` | Create a saved query | `edit_content`; no key |
| `GET projects/{project_id}/sql/saved-queries/{query_id}` | Get a saved query | `read`; key `read` |
| `DELETE projects/{project_id}/sql/saved-queries/{query_id}` | Delete a saved query | `edit_content`; no key |
| `PATCH projects/{project_id}/sql/saved-queries/{query_id}` | Update a saved query | `edit_content`; no key |
| `POST projects/{project_id}/sql/saved-queries/{query_id}/run` | Run a saved query | `run_saved_query`; key `sql` |
| `GET projects/{project_id}/sql/schema` | Get the SQL schema | `read`; key `read` |

### Alerts

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/alerts` | List alert rules | `read`; key `read` |
| `POST projects/{project_id}/alerts` | Create an alert rule | `edit_content`; no key |
| `POST projects/{project_id}/alerts/preview` | Preview an alert rule | `read`; key `read` |
| `GET projects/{project_id}/alerts/{alert_id}` | Get an alert rule | `read`; key `read` |
| `DELETE projects/{project_id}/alerts/{alert_id}` | Delete an alert rule | `edit_content`; no key |
| `PATCH projects/{project_id}/alerts/{alert_id}` | Change an alert rule | `edit_content`; no key |
| `GET projects/{project_id}/alerts/{alert_id}/history` | Get the history of an alert rule | `read`; key `read` |

### Channels

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/channels` | List channels | `edit_content`; no key |
| `POST projects/{project_id}/channels` | Create a channel | `channels`; no key |
| `GET projects/{project_id}/channels/{channel_id}` | Get a channel | `channels`; no key |
| `DELETE projects/{project_id}/channels/{channel_id}` | Delete a channel | `channels`; no key |
| `PATCH projects/{project_id}/channels/{channel_id}` | Change a channel | `channels`; no key |
| `POST projects/{project_id}/channels/{channel_id}/test` | Send a test notification | `channels`; no key |

### Exports

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/exports` | List exports | `read`; key `read` |
| `POST projects/{project_id}/exports` | Create an export | `export`; key `read` |
| `GET projects/{project_id}/exports/{export_id}` | Get an export | `read`; key `read` |
| `GET projects/{project_id}/exports/{export_id}/download` | Download an export | `read`; key `read` |

### Media

Only with an object store and Postgres ([media](../self-hosting/media.md)); otherwise each answers 501
`media_not_supported`.

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/media` | List the media a trace or a span uses | `read`; key `read` |
| `POST projects/{project_id}/media` | Ask to upload media | key `ingest` only |
| `GET projects/{project_id}/media/{media_id}` | Get media with a download link | `read`; key `read` |
| `PATCH projects/{project_id}/media/{media_id}` | Confirm an upload | key `ingest` only |

### Retention and deletion

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects/{project_id}/deletion-requests` | List deletion requests | `read`; key `read` |
| `POST projects/{project_id}/deletion-requests` | Request a deletion | `delete_traces`; key `delete` |
| `GET projects/{project_id}/deletion-requests/{request_id}` | Get a deletion request | `read`; key `read` |
| `GET projects/{project_id}/retention` | Get retention | `read`; key `read` |
| `PUT projects/{project_id}/retention` | Set retention | `manage`; no key |

### Sign-in and account

| Method and path | What it does | Permission |
|---|---|---|
| `GET account` | Get the account | session |
| `PATCH account` | Update the account | session |
| `POST auth/invitations/accept` | Accept an invitation | none |
| `POST auth/login` | Sign in | none |
| `POST auth/logout` | Sign out | session |
| `GET auth/oauth/{provider_id}/callback` | Finish a provider sign-in | none |
| `GET auth/oauth/{provider_id}/start` | Start a provider sign-in | none |
| `POST auth/password/change` | Change the password | session |
| `POST auth/password/reset-confirm` | Reset the password | none |
| `POST auth/password/reset-request` | Ask for a password reset | none |
| `GET auth/providers` | List sign-in options | none |
| `GET auth/session` | Get the session | session |
| `POST auth/setup` | Set up the first owner | none |

### Organization

| Method and path | What it does | Permission |
|---|---|---|
| `GET org` | Get the organization | org `org_read` |
| `PATCH org` | Update the organization | org `org_settings` |
| `GET org/auth-providers` | List sign-in providers | org `org_settings` |
| `POST org/auth-providers` | Add a sign-in provider | org `org_settings` |
| `DELETE org/auth-providers/{provider_id}` | Delete a sign-in provider | org `org_settings` |
| `PATCH org/auth-providers/{provider_id}` | Change a sign-in provider | org `org_settings` |
| `GET org/invitations` | List invitations | org `org_read` |
| `POST org/invitations` | Invite someone | org `org_members` |
| `DELETE org/invitations/{invitation_id}` | Revoke an invitation | org `org_members` |
| `GET org/members` | List members | org `org_read` |
| `DELETE org/members/{user_id}` | Remove a member | org `org_members` |
| `PATCH org/members/{user_id}` | Change a member's role | org `org_members` |
| `POST org/transfer-ownership` | Transfer ownership | org `org_transfer_ownership` |

### Projects, members and keys

| Method and path | What it does | Permission |
|---|---|---|
| `GET projects` | List projects | session |
| `POST projects` | Create a project | org `org_projects` |
| `GET projects/{project_id}` | Get a project | session |
| `DELETE projects/{project_id}` | Delete a project | org `org_projects` |
| `PATCH projects/{project_id}` | Rename a project | `manage`; no key |
| `GET projects/{project_id}/api-keys` | List API keys | `manage`; no key |
| `POST projects/{project_id}/api-keys` | Create an API key | `manage`; no key |
| `DELETE projects/{project_id}/api-keys/{key_id}` | Revoke an API key | `manage`; no key |
| `PATCH projects/{project_id}/api-keys/{key_id}` | Change an API key | `manage`; no key |
| `GET projects/{project_id}/members` | List project members | `manage`; no key |
| `POST projects/{project_id}/members` | Add a project member | `manage`; no key |
| `DELETE projects/{project_id}/members/{user_id}` | Remove a project member | `manage`; no key |
| `PATCH projects/{project_id}/members/{user_id}` | Change a project member's role | `manage`; no key |

## Reading traces

Where the data is:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/data-bounds"
```

```json
{
  "spans": {
    "first": "2026-11-19T23:45:00.000000Z",
    "last": "2026-11-20T00:42:00.000000Z"
  },
  "mcp": {
    "client": {
      "first": "2026-11-19T23:45:00.000000Z",
      "last": "2026-11-19T23:45:00.000000Z"
    },
    "server": {
      "first": "2026-11-20T00:14:00.000000Z",
      "last": "2026-11-20T00:24:00.000000Z"
    }
  },
  "retention_horizon": "2026-08-22T00:00:00.000000Z",
  "max_window_days": 30,
  "server_time": "2026-11-20T00:45:39.660331Z",
  "meta": {
    "as_of": "2026-11-20T00:45:39.660331Z",
    "partial": false,
    "consistency": "strong"
  }
}
```

`spans` and `mcp` give the first and last minute with spans and with MCP operations on each side.
`retention_horizon` is where the project's retention cuts off, null while it keeps its data forever (the default).
`max_window_days` is the widest window a list or search may ask for.

The traces of a window, newest first:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?from=2026-11-19T00:00:00Z&to=2026-11-21T00:00:00Z&environment=staging"
```

```json
{
  "data": [
    {
      "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
      "name": "plan trip",
      "trace_name": "trip planner",
      "head_kind": "root",
      "start_time": "2026-11-19T23:45:01.000000Z",
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
        "reviewed",
        "triaged"
      ],
      "facts": {
        "tags": [
          "reviewed",
          "triaged"
        ],
        "bookmarked": true,
        "public": null,
        "scores": [
          {
            "name": "helpful",
            "score_id": null,
            "data_type": "numeric",
            "value": 1.0,
            "source": "feedback",
            "comment": "",
            "metadata": null,
            "fact": {
              "fact_id": "01a1f65b-b4a4-70eb-83eb-6dc856127f2d",
              "version": 1795134567588787,
              "created_at": "2026-11-20T00:29:27.588787Z",
              "author_kind": "api_key",
              "author_id": "01a1f633-d84d-7d84-9ab9-d78f6df778d4"
            }
          }
        ],
        "correction": null
      },
      "saturated": false
    }
  ],
  "next_cursor": null,
  "has_more": false,
  "head_cursor": "v1.master-0.AQIBAQGh9jPVmnAUgAeuLc-5SZZiEnetaS6enWK0uoaxn6T-AAZgqpAq5ZEABmC-rgJFkQAGYJXPBwAAAAZgvgq1wAAABmCqkCrlkwGh9jO66XwQgPYQVILJB_EAAAAAAAAAIg.uKM66evQQvDJjEsaAbe2KQ",
  "meta": {
    "as_of": "2026-11-20T00:45:39.701139Z",
    "partial": false,
    "consistency": "strong"
  }
}
```

The head span of a trace is its root span, or the earliest span while the root has not arrived. `tags` and
`facts` show the trace's effective tags, flags, scores and correction ([facts](../guides/facts.md)). Ids are
lowercase hex in responses; uppercase hex in a path is accepted.

One trace, with its spans:

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
      "start_time": "2026-11-19T23:45:01.000000Z",
      "end_time": "2026-11-19T23:45:02.200000Z",
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
      "start_time": "2026-11-19T23:45:01.100000Z",
      "end_time": "2026-11-19T23:45:01.800000Z",
      "duration_us": 700000,
      "status_code": "unset",
      "level": "default",
      "model": "model-large-2026-05",
      "total_cost_usd": 0.0042,
      "input_tokens": 1200,
      "output_tokens": 300,
      "total_tokens": 1500,
      "child_count": null,
      "io_shape": "genai_messages",
      "saturated": false
    },
    {
      "span_id": "e457b5a2e4d86bd1",
      "parent_span_id": "00f067aa0ba902b7",
      "name": "tools/call lookup_order",
      "observation_type": "tool",
      "start_time": "2026-11-19T23:45:01.900000Z",
      "end_time": "2026-11-19T23:45:02.100000Z",
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
  "facts": {
    "tags": [
      "reviewed",
      "triaged"
    ],
    "bookmarked": true,
    "public": null,
    "scores": [
      {
        "name": "helpful",
        "score_id": null,
        "data_type": "numeric",
        "value": 1.0,
        "source": "feedback",
        "comment": "good answer",
        "metadata": null,
        "fact": {
          "fact_id": "01a1f65b-b4a4-70eb-83eb-6dc856127f2d",
          "version": 1795134567588787,
          "created_at": "2026-11-20T00:29:27.588787Z",
          "author_kind": "api_key",
          "author_id": "01a1f633-d84d-7d84-9ab9-d78f6df778d4"
        }
      }
    ],
    "correction": null
  },
  "meta": {
    "as_of": "2026-11-20T00:45:39.757459Z",
    "partial": false,
    "consistency": "strong"
  },
  "saturated": false
}
```

The trace detail takes no window: it finds the trace's time bounds itself. It accepts `hint_time`, the trace's start
from the list. A trace with more than 5,000 spans comes back with `paged: true`, its root spans and their direct
children, each with `child_count`; read the levels below with the children route,
`GET projects/{project_id}/traces/{trace_id}/spans?parent_span_id=<span id>`, which pages through the direct
children of one span in start order.

## Lists and filters

| List | Rows | Takes |
|---|---|---|
| `GET projects/{project_id}/traces` | One per trace whose head span started in the window, newest first | `from`, `to`, `limit` (1 to 200, default 50), `cursor`, `since`, and the filters below |
| `GET projects/{project_id}/spans` | One per span that started in the window, newest first, with its trace id, environment, user and session | `from`, `to`, `limit`, `cursor`, and the filters below |
| `GET projects/{project_id}/sessions` | One per session with traces in the window: first and last sighting, trace, span and error counts, tokens, cost and its users | `from`, `to`, `limit`, `cursor` |
| `GET projects/{project_id}/users` | One per user with traces in the window, with the same sums and a session count | `from`, `to`, `limit`, `cursor` |
| `GET projects/{project_id}/users/{user_id}/sessions` | The sessions of one user | `from`, `to`, `limit`, `cursor` |

`GET .../sessions/{session_id}` and `GET .../users/{user_id}` return one session or user over a window.

The trace and span lists take these filters as query parameters. Repeating a parameter matches any of its values;
different parameters must all hold; each takes at most 100 values.

| Filter | Matches |
|---|---|
| `environment`, `release`, `version`, `level`, `user_id`, `session_id` | The field's value |
| `observation_type` | A span type, such as `generation` or `tool` |
| `tags` | A tag of the trace (trace list) or the span (span list) |
| `metadata` | A metadata entry, as `key=value` (split at the first `=`) |
| `name` | A case-insensitive substring of the name, once |

On the trace list, `observation_type` and `metadata` keep the traces that have a matching span, and every other
filter tests the trace's head span: `level=error` finds traces whose head span is an error, and
`observation_type=tool` finds traces with a tool span anywhere. The trace list refuses `observation_type` together
with `metadata` (422 `validation_failed`, code `conflict`); the span list takes both.

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?from=2026-11-19T00:00:00Z&to=2026-11-21T00:00:00Z&environment=staging&environment=production&metadata=plan=pro"
```

`GET projects/{project_id}/filter-values?source=spans&field=environment&from=...&to=...` lists the values a filter
field takes in a window, with counts, for building a picker:

```json
{"data":[{"value":"default","count":10},{"value":"staging","count":3},{"value":"production","count":1}],"truncated":false,"saturated":false,"meta":{"as_of":"2026-11-20T00:45:39.871662Z","partial":false,"consistency":"strong"}}
```

Filtered lists read ClickHouse only, so a trace acknowledged a moment ago may be missing; the page then says
`consistency: lagging`.

**Live tail.** A first page of the trace list also carries `head_cursor`. Send it as `since`, with the same window and
filters and without `cursor`, to get the traces that arrived after it, with `new_count` (how many arrived, counted
up to 100) and a fresh `head_cursor` for the next poll. The UI's traces page polls this way.

## Searching

```sh
curl -s -X POST -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/search" \
  -d '{"query": "lisbon", "from": "2026-11-19T00:00:00Z", "to": "2026-11-21T00:00:00Z"}'
```

```json
{
  "data": [
    {
      "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
      "span": {
        "span_id": "b7ad6b7169203331",
        "parent_span_id": "00f067aa0ba902b7",
        "name": "chat",
        "observation_type": "generation",
        "start_time": "2026-11-19T23:45:01.100000Z",
        "end_time": "2026-11-19T23:45:01.800000Z",
        "duration_us": 700000,
        "status_code": "unset",
        "level": "default",
        "model": "model-large-2026-05",
        "total_cost_usd": 0.0042,
        "input_tokens": 1200,
        "output_tokens": 300,
        "total_tokens": 1500,
        "child_count": null,
        "io_shape": "genai_messages",
        "saturated": false
      },
      "matches": [
        {
          "field": "output",
          "fragments": [
            {"text": "[{\"role\": \"assistant\", \"parts\": [{\"type\": \"text\", \"content\": \"", "highlighted": false},
            {"text": "Lisbon", "highlighted": true},
            {"text": "\"}], \"finish_reason\": \"stop\"}]", "highlighted": false}
          ],
          "cut_before": false,
          "cut_after": false
        }
      ]
    }
  ],
  "next_cursor": null,
  "has_more": false,
  "meta": {"as_of": "2026-11-20T00:45:39.906382Z", "partial": false, "consistency": "strong"}
}
```

The answer is cut to its first hit; the second is the agent span, whose output holds the same word.

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

## Payloads

`GET projects/{project_id}/spans/{trace_id}/{span_id}/payload?field=<field>&start_time=<start>` returns one payload
field of a span as raw bytes. `field` is one of `input`, `output`, `system_instructions`, `attributes`,
`resource_attributes`, `events`, `links`, `tool_definitions` and `tool_calls`, and `start_time` is the span's
`start_time` as the API returned it, which locates the payload.

```sh
curl -s -i -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/spans/4bf92f3577b34da6a3ce929d0e0e4736/00f067aa0ba902b7/payload?field=input&start_time=2026-11-19T23:45:01.000000Z"
```

```text
HTTP/1.1 200 OK
content-type: text/plain; charset=utf-8
accept-ranges: bytes
etag: "4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1795131901000000-input-25"
cache-control: private, max-age=31536000, immutable
content-security-policy: default-src 'none'; frame-ancestors 'none'
x-content-type-options: nosniff
referrer-policy: same-origin
cross-origin-opener-policy: same-origin
cross-origin-resource-policy: same-origin
content-length: 25
x-request-id: 01a1f665-4622-7f7e-99bc-7aa2c15748cf

Where should I go in May?
```

The body is the field's bytes as stored, always as `text/plain`, so a browser never renders it. A field that the span
lists in `truncated_fields` was cut at the size cap and may no longer be valid JSON
([size caps](../guides/sending-traces.md#size-caps)). A `Range: bytes=<first>-[<last>]` header asks for part of the
field and gets 206 with `Content-Range`; a range that starts past the end gets 416 with
`Content-Range: bytes */<length>`. `download=1` adds `Content-Disposition: attachment`. A missing payload answers 404.

## Metrics

`GET projects/{project_id}/metrics/schema` lists the sources, their measures (with units and aggregations),
dimensions, filter fields, granularities and limits. `POST projects/{project_id}/metrics/query` returns time series:

```sh
curl -s -X POST -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/metrics/query" \
  -d '{"source": "spans", "measures": [{"measure": "count", "aggregation": "sum"}, {"measure": "duration", "aggregation": "p99"}],
       "dimensions": ["environment"], "granularity": "hour", "from": "2026-11-19T21:30:00Z", "to": "2026-11-20T01:30:00Z"}'
```

```json
{
  "series": [
    {
      "measure": "count",
      "aggregation": "sum",
      "unit": "count",
      "key": [{"dimension": "environment", "value": "default"}],
      "other": false,
      "points": [
        {"bucket": "2026-11-19T21:00:00.000000Z", "value": 0.0},
        {"bucket": "2026-11-19T22:00:00.000000Z", "value": 0.0},
        {"bucket": "2026-11-19T23:00:00.000000Z", "value": 0.0},
        {"bucket": "2026-11-20T00:00:00.000000Z", "value": 10.0},
        {"bucket": "2026-11-20T01:00:00.000000Z", "value": 0.0}
      ]
    }
  ],
  "granularity": "hour",
  "truncated": false,
  "source_tier": "rollup",
  "as_of": "2026-11-20T00:45:39.973996Z",
  "partial_last_bucket": true,
  "saturated": false,
  "meta": {"as_of": "2026-11-20T00:45:39.973996Z", "partial": false, "consistency": "strong"}
}
```

| Member | Meaning |
|---|---|
| `source` | `spans`, or `traces` (which has the one measure `traces`) |
| `measures` | One to four, each a measure id and an aggregation it allows: `sum`, or a percentile `p50`, `p90`, `p95`, `p99`. The `spans` measures are `count`, `error_count`, `input_tokens`, `output_tokens`, `total_tokens`, `total_cost` (USD), `duration` and `ttft` (time to first token, microseconds) |
| `dimensions` | Up to two of `environment`, `observation_type`, `model`, `level` and `release`; each combination of values is one series |
| `filter` | Conditions over the filter fields, combined with `and`, `or` and `not` |
| `granularity` | `auto` (default), `minute`, `five_minutes`, `fifteen_minutes`, `hour`, `day` or `week` |
| `from`, `to` | The window, at most the project's maximum |
| `top_n` | With dimensions, how many keys to return, 1 to 20 (default 20); the rest of an additive measure is summed into an `other` series |
| `fill` | `zero` (default): every bucket is returned, 0 for a sum and null for a percentile. `none`: empty buckets are left out |

The answer above is cut to its first series; the query returns one series per measure and environment. It also says
which granularity it used, whether the last bucket may still grow (`partial_last_bucket`), and where the measures
were read (`source_tier`: the minute rollup or the span table).

## Dashboards and widgets

A dashboard is a grid of 12 columns holding at most 50 widgets. A widget has a `query`, which is the body of
`POST metrics/query` without `from` and `to` (the dashboard's time range supplies the window), a `display` (its
`kind`: `timeseries`, `stat`, `bar`, `table` or `markdown`; a `title`; `chart` `line` or `bar` for a time series;
`text` for Markdown, which takes no query) and a `layout` in cells (`x`, `y`, `w`, `h`, each widget at most 12 by
12, and no two widgets overlapping). The [dashboards guide](../guides/dashboards.md) lists what each kind may query
and its smallest size.

`GET projects/{project_id}/dashboards` lists the project's dashboards, oldest first, then the built-in starters,
whose `id` is their `builtin_key`:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/dashboards"
```

```json
{
  "data": [
    {"id": "overview", "builtin_key": "overview", "name": "Agent overview", "description": "Traces, spans, errors and cost at a glance.", "widget_count": 8, "version": 1, "updated_at": null},
    {"id": "cost-and-tokens", "builtin_key": "cost-and-tokens", "name": "Cost and tokens", "description": "What the models cost, and the tokens behind it.", "widget_count": 6, "version": 1, "updated_at": null},
    {"id": "latency", "builtin_key": "latency", "name": "Latency", "description": "How long spans take, and how long models take to answer.", "widget_count": 7, "version": 1, "updated_at": null},
    {"id": "errors", "builtin_key": "errors", "name": "Errors", "description": "Where spans fail: by type, model and environment.", "widget_count": 6, "version": 1, "updated_at": null}
  ]
}
```

`GET .../dashboards/{dashboard_id}` returns one dashboard with its widgets, in reading order, and its `version` as
`ETag`. A key with `read` reads dashboards; every change needs `edit_content`, which project members and admins hold
and keys never do, so changes are made with a session. Creating one:

```sh
curl -s -i -b cookies.txt -X POST "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/dashboards" \
  -H 'Content-Type: application/json' -H "X-Crucible-CSRF: <csrf_token>" -H "Origin: $CRUCIBLE_URL" \
  -d '{"name": "Agent health",
       "widgets": [{"display": {"kind": "stat", "title": "Errors"}, "layout": {"x": 0, "y": 0, "w": 3, "h": 2},
                    "query": {"source": "spans", "measures": [{"measure": "error_count", "aggregation": "sum"}]}}]}'
```

The answer is 201 with the dashboard, each widget with its new `id`, and `ETag: "1"`.

- **Changes carry the version.** `PATCH` and `DELETE` of a dashboard or of a widget send the `ETag` they read as
  `If-Match`. Without it the answer is 428 `precondition_required`; with an older one, 412 `precondition_failed`
  ("the dashboard changed since it was read; read it again and repeat the change with its new ETag"). Each change
  answers the dashboard's next version as `ETag`, except deleting the dashboard, which answers 204.
- **Adding a widget** (`POST .../widgets`) takes no `If-Match`: it is placed only where it overlaps no widget, checked
  again if another change lands first, and answers 201 with the widget and the dashboard's new `ETag`.
- **`PATCH .../dashboards/{dashboard_id}` with `widgets`** replaces the whole set in one change: a widget with the
  `id` of an existing one rewrites it, one without an `id` is added, and one left out is deleted.
- **Rules are checked on every write**, and a refusal is 422 `validation_failed` whose `errors` point at the member
  to fix, for example `{"pointer": "/layout", "code": "overlaps", "message": "the widget overlaps \"Errors\"; move
  or resize one of them"}`.
- **Starters are read-only.** A change to one answers 409 `conflict` and says to duplicate it.
  `POST .../dashboards/{dashboard_id}/duplicate` copies any dashboard, starters included, as one of the project's own;
  the copy is named after the original with " (copy)" unless the body gives a `name`.
- **Without the control plane** the starters are still listed and read; creating or duplicating a dashboard answers
  404 `feature_disabled`.

## The rest of the API

- **Facts**: tags, flags, scores and corrections, written with `POST feedback`, `tags`, `flags` and `corrections`
  and read with `GET facts`; see [facts](../guides/facts.md).
- **SQL workspace**: `GET sql/schema` lists the `crucible_sql.*` relations; `POST sql/run` runs one read-only
  statement over a window (`{from:DateTime64(6)}` and `{to:DateTime64(6)}`) and returns at most 10,000 rows;
  `POST sql/export` returns the result as CSV, JSONL or Parquet; saved queries keep a statement with declared
  parameters. Each project's statements run as its own ClickHouse user
  ([SQL workspace](../self-hosting/sql-workspace.md)).
- **Alerts and channels**: alert rules over the `spans` metrics measures, a preview of what a rule would read now,
  each rule's history, and webhook, Slack and email channels with a test send; see [alerts](../guides/alerts.md).
- **Exports**: `POST exports` starts a job that writes a project's spans of a window as `crucible-jsonl` or
  `otlp-json`, optionally gzip-compressed; poll `GET exports/{export_id}` until `state` is `ready`, then download it
  from `download_path`. Files are kept 7 days, and a project may start 5 exports an hour.
- **Retention and deletion**: `GET` and `PUT retention`, and deletion requests for traces, a user or a session; see
  [retention and deletion](../self-hosting/retention-and-deletion.md).
- **Media**: `POST media` with the content type, length, SHA-256 and the trace that uses them answers a presigned
  upload link valid for 1 hour, whose content type, length and checksum headers must be sent exactly as given, or
  no link when the project already stores the same bytes. `PATCH media/{media_id}` confirms the upload once the
  object store holds it; `GET media/{media_id}` answers a download link valid for 5 minutes; `GET media` lists the
  uses in a trace or a span. An object is at most `media.max_bytes`, 64 MiB by default; see
  [media](../self-hosting/media.md).
- **Identity**: sign-in, sessions, password reset, invitations, the organization, its members and sign-in providers,
  projects, project members and API keys, as listed under [routes](#routes).

An export, from request to file:

```sh
curl -s -X POST -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: nightly-2026-11-19' "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/exports" \
  -d '{"format": "crucible-jsonl", "from": "2026-11-19T00:00:00Z", "to": "2026-11-21T00:00:00Z", "gzip": true}'
```

```json
{"id":"01a1f668-4f9c-7741-be70-1a9b12fd18c5","format":"crucible-jsonl","state":"queued","from":"2026-11-19T00:00:00.000000Z","to":"2026-11-21T00:00:00.000000Z","trace_count":0,"gzip":true,"keep_truncated":false,"created_at":"2026-11-20T00:43:13.691811Z","as_of":"2026-11-20T00:43:13.691811Z","spans":0,"skipped":0,"bytes":0,"saturated":false}
```

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/exports/01a1f668-4f9c-7741-be70-1a9b12fd18c5"
```

```json
{"id":"01a1f668-4f9c-7741-be70-1a9b12fd18c5","format":"crucible-jsonl","state":"ready","from":"2026-11-19T00:00:00.000000Z","to":"2026-11-21T00:00:00.000000Z","trace_count":0,"gzip":true,"keep_truncated":false,"created_at":"2026-11-20T00:43:13.691811Z","as_of":"2026-11-20T00:43:13.691811Z","spans":14,"skipped":0,"bytes":4538,"checksum":"689f31c869b3f49243e0ec4c3ca27bd0359bea86c4c9153871d505f6c708fb37","expires_at":"2026-11-27T00:43:15.920434Z","download_path":"/api/v1/projects/01a1f633-d59a-7014-8007-ae2dcfb94996/exports/01a1f668-4f9c-7741-be70-1a9b12fd18c5/download","saturated":false}
```

```sh
curl -s -o spans.jsonl.gz -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/exports/01a1f668-4f9c-7741-be70-1a9b12fd18c5/download"
```

The download is `application/gzip` (or `application/x-ndjson` without gzip) with
`Content-Disposition: attachment`, and its SHA-256 is the export's `checksum`.

## Time windows

Lists, search, metrics, exports and SQL runs take `from` and `to`. The window is half open, `[from, to)`, over the
start time of each trace's head span (for spans, search and exports, of each span). It may be at most the project's
maximum, 30 days, which `data-bounds` reports as `max_window_days`; a window of exactly the maximum passes. The trace
detail, the children route and `data-bounds` take no window, and the payload route takes the span's `start_time`
instead.

Times are RFC 3339. Inputs may use any offset and precision and must fall between 1970 and 2100; responses are always
UTC with six fractional digits, such as `2026-11-18T09:30:00.000000Z`.

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_SECRET_KEY" \
  "$CRUCIBLE_URL/api/v1/projects/$PROJECT_ID/traces?from=2026-09-01T00:00:00Z&to=2026-11-19T00:00:00Z"
```

```json
{"type":"urn:crucible:problem:query_too_broad","title":"Query too broad","status":422,"detail":"the window is longer than the project's maximum of 30 days","code":"query_too_broad","request_id":"01a1f66a-8b41-7bca-8685-3021012f2fc9","limit":{"name":"max_window_days","value":30}}
```

## Pagination and cursors

A list answers `{ data, next_cursor, has_more, meta }`, with no total count. `next_cursor` is null on the last page.
To read the next page, send the same request with `cursor` set to `next_cursor`.

| List | `limit` |
|---|---|
| Trace list, span list, search | 1 to 200, default 50 |
| Children of a span | 1 to 2,000, default 500 |

- **Cursors are signed.** A cursor reads `v1.<key id>.<payload>.<mac>`, with an HMAC-SHA256 over the rest. Treat it
  as an opaque string. It is bound to the project, the route, the filters, the sort and the window of the request
  that issued it, and it expires 24 hours after it was issued. Any other cursor answers 400 `cursor_invalid`; start
  again from the first page.
- **Pages do not shift.** The first page fixes `meta.as_of`, and later pages show only traces that arrived at or
  before it, so new traces never push rows onto a page you have not read yet.
- **Cursors survive restarts.** The signing key is derived from the master key, so a cursor still opens after a
  restart, and every server started with the same master key opens the cursors of the others.

## Response metadata

Every JSON response of a telemetry read carries `meta`:

| Member | Meaning |
|---|---|
| `as_of` | The instant the response describes; for a list, the one its first page fixed |
| `partial` | True when rows may be missing, for the reason in `partial_reason`: `store_unavailable`, `deadline` or `quarantined` |
| `consistency` | `strong`: the response reflects every span this server acknowledged before the request arrived. `lagging`: it may trail |

Counts are JSON numbers and stop at 2^53 - 1; an object with a count that stopped there has `saturated: true`. Costs
are JSON numbers in USD, in fields named `*_usd`.

## Errors

Every error is a problem document (RFC 9457) with the media type `application/problem+json`:

```text
HTTP/1.1 422 Unprocessable Entity
content-type: application/problem+json

{"type":"urn:crucible:problem:validation_failed","title":"Validation failed","status":422,"detail":"the request is invalid; see `errors`","code":"validation_failed","request_id":"01a1f66a-8b5e-7f50-9a5f-a2e5800fdb67","errors":[{"pointer":"/from","code":"required","message":"the parameter is required"},{"pointer":"/to","code":"required","message":"the parameter is required"},{"pointer":"/limit","code":"out_of_range","message":"expected an integer from 1 to 200"},{"pointer":"/sort","code":"unknown_parameter","message":"the endpoint has no such parameter"}]}
```

| Member | Meaning |
|---|---|
| `type` | `urn:crucible:problem:<code>` |
| `title`, `detail` | For logs and developers; show your own text to users. `detail` says what to change |
| `status` | The HTTP status |
| `code` | The stable code: branch on this |
| `request_id` | The same value as the `X-Request-Id` header |
| `errors` | For `validation_failed`: one entry per invalid part, with a JSON `pointer` (`/from` for the query parameter `from`), a `code` and a `message` |
| `limit` | For `query_too_broad`: the limit that was exceeded, such as `{"name": "max_window_days", "value": 30}` |
| `retry_after_s` | For `rate_limited`, `control_plane_unavailable` and the SQL limits: seconds to wait, also sent as `Retry-After` |
| `line`, `column` | For SQL errors: where in the statement |
| `reason` | For `sql_workspace_unavailable`: why the workspace is off |

| Code | Status | When |
|---|---|---|
| `cursor_invalid` | 400 | The cursor is tampered, expired, from another project, or bound to other filters, sort or window |
| `unauthenticated` | 401 | No credential, or one the server does not accept |
| `invalid_credentials` | 401 | Sign-in with a wrong email or password |
| `forbidden` | 403 | The role or scope does not grant the route's permission |
| `csrf_invalid` | 403 | A cookie-authenticated write without the session's `X-Crucible-CSRF` token |
| `origin_invalid` | 403 | A cookie-authenticated write from another origin |
| `sql_forbidden` | 403 | SQL that is not read-only, or that ClickHouse refused for the workspace user |
| `not_found` | 404 | An unknown path, a project the caller cannot see, or an unknown or deleted resource |
| `feature_disabled` | 404 | The route needs a module this server does not run, such as the control plane |
| `method_not_allowed` | 405 | The route does not take the method; see `Allow` |
| `request_timeout` | 408 | The request did not finish in time |
| `conflict` | 409 | A unique or state conflict |
| `idempotency_key_reused` | 409 | An `Idempotency-Key` sent again with a different request |
| `export_expired` | 410 | The export file expired, or a deletion request expired it |
| `precondition_failed` | 412 | `If-Match` does not match the resource's version |
| `payload_too_large` | 413 | The body is above 1 MiB |
| `unsupported_media_type` | 415 | The body's media type is not accepted |
| `validation_failed` | 422 | A path, query or body value is invalid; see `errors` |
| `token_invalid` | 422 | A setup, invitation or reset link that is invalid, expired or used |
| `query_too_broad` | 422 | The window is above the maximum, or the read would cover too much; see `limit` |
| `query_memory_exceeded` | 422 | The read hit its memory limit |
| `ssrf_blocked` | 422 | A channel URL the egress policy refuses |
| `sql_invalid` | 422 | SQL that is not one read-only statement, or that ClickHouse rejected |
| `sql_time_window_required` | 422 | SQL that does not filter on the window |
| `precondition_required` | 428 | A change to a versioned resource without `If-Match` |
| `rate_limited` | 429 | Too many requests of a limited kind; see `Retry-After` |
| `sql_quota_exceeded` | 429 | The project used its SQL statements for the hour, or a run limit |
| `internal_error` | 500 | Anything else |
| `control_plane_unavailable` | 503 | Postgres is unreachable |
| `telemetry_store_unavailable` | 503 | ClickHouse is not configured, or it is unreachable and the recent-span index cannot answer |
| `ingest_unavailable` | 503 | The write-ahead log cannot take a fact or deletion write now; see `Retry-After` |
| `sql_workspace_unavailable` | 503 | The SQL workspace is off; see `reason` |
| `query_timeout` | 504 | The read hit its time limit |

The codes in `errors[].code` are stable too, such as `required`, `repeated`, `unknown_parameter`,
`invalid_timestamp`, `out_of_range`, `invalid_window`, `invalid_id`, `conflict` (`since` with `cursor`), `invalid`,
`too_long` and `unknown_field`.

## Concurrency and idempotency

- **Versions.** Retention, alert rules, channels, saved queries, projects and other mutable resources carry a
  `version`, also sent as `ETag`. A `PATCH`, `PUT` or `DELETE` of them requires `If-Match` with that value: without it
  the answer is 428 `precondition_required`, and with a stale one 412 `precondition_failed`, with the current version
  in the message.
- **Idempotency.** The fact writes (`feedback`, `tags`, `flags`, `corrections`), `POST exports` and
  `POST deletion-requests` honour an `Idempotency-Key` header of 1 to 128 characters of `[A-Za-z0-9_-]`: a replay
  returns the first answer (an export, its current state), and the same key with a different request answers 409
  `idempotency_key_reused`.

The payload route sends an `ETag` and `Cache-Control: private, max-age=31536000, immutable`, because a stored payload
never changes. OTLP ingest does not read `Idempotency-Key`; how it handles a retried export is described under
[retries and duplicates](../guides/sending-traces.md#what-an-acknowledgement-means).

## Rate limits

| What | Limit | Answer |
|---|---|---|
| Exports | 5 per hour per project | 429 `rate_limited` |
| Channel test sends | 6 per minute per channel | 429 `rate_limited` |
| SQL runs | 1 at a time per person, 4 per project, 30 per minute per person, and the hourly statement quota | 429 `sql_quota_exceeded` |
| MCP tool calls | 50 per second per key, bursts of 200 | A tool error with `rate_limited` |
| Password checks | At most 4 run at once; sign-in answers when all are busy | 429 `rate_limited` |

Ingest is not rate limited: when the server cannot take more, it answers 429 or 503 with `Retry-After`, as
[sending traces](../guides/sending-traces.md#what-an-acknowledgement-means) describes.

## OpenAPI document

The OpenAPI 3.1 document of the API is committed at `specs/openapi/crucible-v1.json`, generated from the route
handlers; `crucible openapi` prints the same document from the binary, and CI fails when the committed file no
longer matches the handlers. Its `info.version` is the binary's version.
