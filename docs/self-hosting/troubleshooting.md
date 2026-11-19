# Troubleshooting

Every refusal says what to change. This page collects the common ones by where they show up, with the message the
server gives and the fix.

## Compose does not start

| Message | Fix |
|---|---|
| `required variable CLICKHOUSE_PASSWORD is missing a value: set CLICKHOUSE_PASSWORD in .env` (or `POSTGRES_PASSWORD`, `CRUCIBLE_MASTER_KEY`, `CRUCIBLE_CLICKHOUSE_URL`) | Make `.env` beside the Compose file, as in step 2 of the [quickstart](quickstart.md#2-make-env) |
| `container crucible-ready-1 is unhealthy` right after an upgrade or a restart | The `ready` service saw the server restarting. Once `curl -s http://127.0.0.1:8080/readyz` answers `ready`, run `docker compose up -d --wait` again |
| `error from registry: denied` while pulling `ghcr.io/elm-os/crucible` | Build the image and set `CRUCIBLE_IMAGE` to its tag in `.env` ([quickstart](quickstart.md#1-build-the-image)) |

## The configuration is refused

`crucible config check` reports every problem at once. The server refuses to start with the same messages.

| Message | Fix |
|---|---|
| `unknown key 'server.lisen'; did you mean 'server.listen'?` | Correct the key in the TOML file |
| `` `clickhouse.url` needs the admin password `` | Set `CRUCIBLE_CLICKHOUSE_PASSWORD` or `clickhouse.password_file` |
| `` `clickhouse.url` needs the master key `` or `` `postgres.url` needs the master key `` | Set `CRUCIBLE_MASTER_KEY` or `master_key_file`; `openssl rand -hex 32` makes one |
| `` `master_key` must be 64 hexadecimal characters (32 bytes) `` | Use 64 hexadecimal characters, such as the output of `openssl rand -hex 32` |
| `` `server.keys_file` (--keys-file) is for a server without Postgres `` | With Postgres, API keys live there: make them in **Settings** or with `crucible admin create-key --project default`, and remove the keys file setting |
| `` `security.require_https` = true needs `server.public_url` to be an https:// URL `` | Set `server.public_url` to your TLS proxy's `https` address, or turn `security.require_https` off |
| `` `egress.mode` = "closed" is not one of open, strict `` | Use `open` or `strict` |
| `wal backend unavailable: feature_disabled` | This binary has no `uring` backend (the image does not). Use `--wal-backend auto` or `portable` |
| `warning: the environment variable CRUCIBLE_CLICKHOUSE_URI is not a Crucible setting and is ignored; did you mean CRUCIBLE_CLICKHOUSE_URL?` | Correct the variable's name |

## The server is not ready

`GET /readyz` answers 503 with `not ready: <reason>` while the server cannot take ingest, and ingest answers 503 for
the same time. `docker compose logs crucible` shows the same reason. The checks retry on their own, so a reason
that names a database that is still starting clears by itself.

| Reason | Fix |
|---|---|
| `startup detection (the service users are missing, or were created under a different master key; see --migrate and --master-key-file)` with `AUTHENTICATION_FAILED` | The server runs with another master key than the one this ClickHouse was migrated with. Start it with the original master key. For a new ClickHouse, start once with `--migrate` |
| `N of M tables exist; start once with --migrate` | Start once with `--migrate` (Compose always does), or run `crucible migrate` |
| `ClickHouse 25.8 is older than the minimum, 26.3` | Upgrade ClickHouse to 26.3 or newer |
| The ClickHouse server is `Cloud` or `Clustered` | Crucible needs a single-node ClickHouse server |
| `the server lacks the text index or the text-search functions`, or `the server lacks the settings ...` | Use a ClickHouse release that has them: 26.3 or newer, with its default configuration |
| `applying the Postgres migrations: ... migration V5__export_jobs is missing from the filesystem` | An older image started on a database that a newer one migrated. Start the newer image, or restore the backup taken before the upgrade ([operations](operations.md#upgrades)) |

The server also logs a warning at startup when its memory budgets add up to more than 90% of the memory it may
use, with the amount to give it. With the defaults, give the container at least 2 GiB
([operations](operations.md#memory-and-disk)).

## Signing in

| Symptom or message | Fix |
|---|---|
| No setup link in the logs | The link is logged only while no owner exists, a few seconds after the start: `docker compose logs crucible \| grep 'setup/?token='`. Each restart logs a new one. Or create the owner with `crucible admin init` |
| `token_invalid`: `the link is invalid, expired or already used; ask for a new one` on the setup page | A setup link works once and for 24 hours. Restart the server for a new one, or use `crucible admin init` |
| `origin_invalid`: `the request's Origin is not this server's; open the UI at the server's own address, or add the origin to server.allowed_origins` | Open the UI at the address in `server.public_url`, or, without one, at the address the request reaches. A second hostname goes in `server.allowed_origins` |
| `invalid_credentials`: `the email or password is wrong` | Reset the password: `docker compose exec crucible crucible admin reset-password <email>` prints a one-time link valid for 30 minutes |
| `control_plane_unavailable` (503) | Postgres is unreachable. Signed-in sessions keep working for `auth.session_outage_grace`; new sign-ins wait until Postgres is back |
| `csrf_invalid` from a script: ``send `X-Crucible-CSRF: prelogin` with this request`` | Pre-session requests (sign-in, setup, reset, invitations) send `X-Crucible-CSRF: prelogin`; later cookie-authenticated writes send the session's `csrf_token` ([API](../api/README.md)) |

## Sending traces

| Answer | Fix |
|---|---|
| 401 ``send the project's secret key as `Authorization: Bearer <secret key>` `` | Send the key in `Authorization` ([sending traces](../guides/sending-traces.md#authentication)) |
| 401 `this server does not accept the secret key; check that it was copied whole and that the server lists it` | Check the key. A key made with `crucible admin create-key` reaches a running server within 5 seconds |
| 413 | The body is above 67,043,328 bytes after gzip is inflated, or holds more than 50,000 spans: send smaller batches |
| 415 | Send `application/x-protobuf` or `application/json`, plain or gzip |
| 429 or 503 with `Retry-After` | The server is busy, starting, or protecting its disk: retry after the delay. A 503 that lasts means the write-ahead log is not draining; check `/readyz`, ClickHouse and the disk ([operations](operations.md#memory-and-disk)) |
| 200 with `partialSuccess` | Some spans were rejected: an all-zero or missing trace or span id, or a start more than 400 days ago. A retry is rejected the same way |

## Reading data

| Answer | Fix |
|---|---|
| 404 `not_found` for a project you can see in the UI | The key belongs to another project: each key reads only its own |
| 403 ``this needs the `manage` permission on the project; ask a project admin`` (or another permission) | The role or the key's scopes do not allow it ([security](security.md#api-keys-and-roles)) |
| 422 `query_too_broad`: `the window is longer than the project's maximum of 30 days` | Ask for a window of at most 30 days |
| 400 `cursor_invalid` | The cursor expired (after 24 hours) or belongs to another query: start again from the first page |
| 428 `precondition_required` or 412 `precondition_failed` | Read the resource again and send its `ETag` in `If-Match` |
| 503 `telemetry_store_unavailable` | ClickHouse is not configured, or not reachable and the recent-span index cannot answer the read |

## Channels and alerts

| Message | Fix |
|---|---|
| `ssrf_blocked`: `169.254.169.254 is a cloud metadata address, which Crucible never calls; no setting allows it` | Point the channel at the receiver itself |
| `ssrf_blocked` naming a private address or `https` under `egress.mode = strict` | Add the range to `egress.allow_cidrs`, use an `https` URL, or set `egress.mode = open` ([alerts and egress](alerts-and-egress.md)) |
| `smtp_not_configured`: `email channels need the mail server of this deployment; set smtp.host and smtp.from` | Set both, and restart the server |
| `a Slack channel takes an incoming-webhook URL that starts with https://hooks.slack.com/` | Use a webhook channel for any other receiver |
| A channel turned itself off | It failed 50 deliveries in a row; the channel shows why. Fix the receiver, send a test, then turn the channel on |

## SQL workspace

| Message | Fix |
|---|---|
| `sql_workspace_unavailable` with a reason | See [the self-probe](sql-workspace.md#the-self-probe) |
| `sql_time_window_required`: `Filter on the time window: add a condition such as start_time >= {from:DateTime64(6)}` | Filter on the relation's time column with `{from:DateTime64(6)}` and `{to:DateTime64(6)}` |
| `sql_invalid`: `Use a table from the schema browser, for example crucible_sql.spans` | Query the `crucible_sql.*` views, not Crucible's own tables |
| `sql_invalid`: `The workspace is read-only: start the query with SELECT or WITH` | The workspace reads only |
| `sql_quota_exceeded` (429) | Wait for the next hour, or raise `sql.statements_per_hour` and restart the server |
| 403 ``this needs the `run_sql` permission`` | Ad hoc SQL needs the member role, or a key with the `sql` scope |
