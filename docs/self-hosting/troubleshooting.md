# Troubleshooting

Every refusal says what to change. This page collects the common ones by where they show up, with the message the
server gives and the fix.

## Compose does not start

| Message | Fix |
|---|---|
| `required variable CLICKHOUSE_PASSWORD is missing a value: set CLICKHOUSE_PASSWORD in .env` (or `POSTGRES_PASSWORD`, `CRUCIBLE_MASTER_KEY`, `CRUCIBLE_CLICKHOUSE_URL`) | Make `.env` beside the Compose file, as in step 1 of the [quickstart](quickstart.md#1-get-the-compose-file-and-make-env) |
| `container crucible-ready-1 is unhealthy` | The server has not answered `/readyz` with `ready` for a minute: it is down, or its startup checks fail. `curl -s http://127.0.0.1:8080/readyz` and `docker compose logs crucible` give the reason; once it answers `ready`, the `ready` service turns healthy at its next check, within 10 seconds |
| `ghcr.io/elm-os/crucible:<tag>: not found`, `error from registry: denied`, or `pull access denied for <image>, repository does not exist` | `CRUCIBLE_IMAGE` names an image that is not published. Give a published image by its full reference, such as `ghcr.io/elm-os/crucible:0.1.3`, `ghcr.io/elm-os/crucible:0.1` or `ghcr.io/elm-os/crucible:latest`: a bare tag such as `0.1` names no Crucible image. Or build the image yourself ([quickstart](quickstart.md#building-the-image-from-source)) |

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

## A listener does not start

| Message | Fix |
|---|---|
| `binding the metrics listener on 127.0.0.1:9464: address already in use; set server.metrics_listen (CRUCIBLE_SERVER_METRICS_LISTEN) to another address`, or the same for the `api`, `ingest` or `grpc` listener | Another process holds the address: stop it, or give the listener another address with the setting the message names |

## The server is not ready

`GET /readyz` answers 503 with `not ready: <reason>` while the server cannot take ingest, and ingest answers 503 for
the same time. `docker compose logs crucible` shows the same reason. The checks retry on their own, so a reason
that names a database that is still starting clears by itself.

| Reason | Fix |
|---|---|
| `startup detection (the service users are missing, or were created under a different master key; see --migrate and --master-key-file)` with `AUTHENTICATION_FAILED` | The server runs with another master key than the one this ClickHouse was migrated with. Start it with the original master key |
| `the migrations ledger in Postgres records ClickHouse migration step 0002_access_service, ... but ClickHouse holds none of them`, or `but no replica in cluster <name> holds any of them` (also from `crucible migrate`, which exits with 1) | The Postgres database holds the ledger of a deployment on another ClickHouse, such as a single-server deployment's beside a new cluster, or this ClickHouse lost its data. Give the new ClickHouse a Postgres database of its own. To keep this database's projects, users and API keys without the old telemetry, stop the servers that use the other ClickHouse, run `DELETE FROM schema_migrations WHERE kind LIKE 'clickhouse%'` in it, and migrate again |
| `crucible migrate` exits with 1: `ClickHouse migration step 0003_access_workspace changed since the migrations ledger recorded it, and the service identities refuse this server's master key (...), so it is not applied` | The command ran with another master key than the one this ClickHouse was migrated with, and a step changed since then (a new version, or another `sql.statements_per_hour`). Run `crucible migrate` again with the original master key; the other steps are applied |
| `N of M tables exist; start once with --migrate` | Start once with `--migrate` (Compose always does), or run `crucible migrate` |
| `this version needs ClickHouse migration step <id>, which the migrations ledger does not record` | The server runs without `--migrate`, and no server of this version has migrated yet. Run `crucible migrate`, or start one server with `--migrate`; within 15 seconds of that the server opens |
| `ClickHouse <version> is older than the minimum, 26.3` | Upgrade ClickHouse to 26.3 or newer |
| `this server is a ClickHouse Cloud service, which Crucible does not support` | Point `clickhouse.url` at a self-hosted ClickHouse: a single server, or one replica in a cluster ([clustered ClickHouse](clustered.md)) |
| `the tables in database crucible are replicated, so this server belongs to a cluster; set clickhouse.cluster ...` | Set `clickhouse.cluster` (`CRUCIBLE_CLICKHOUSE_CLUSTER`) to the cluster's name in `remote_servers` |
| `clickhouse.cluster names cluster <name>, but the tables ... in database crucible are not replicated` | The tables were created for a single server. Unset `clickhouse.cluster` to keep using that server, or point `clickhouse.url` at a cluster whose database `crucible` holds no tables |
| `replica <host>:9000 (...) in cluster <name> holds tables in database crucible that are not replicated`, or `replicas <host>:9000 (...) and <host>:9000 (...) in cluster <name> hold tables ...` | The replicas the message names hold tables created for a single server, which Crucible's `ON CLUSTER` statements would leave as they are. Put an empty replica in the place of each, or run `DROP DATABASE crucible SYNC` on each of them, which deletes its rows, then migrate again |
| `the server's remote_servers defines no cluster named <name>`, or `cluster <name> has N shards` | Set `clickhouse.cluster` to the name of a cluster of one shard that holds the replica in `clickhouse.url`, or define that cluster in `remote_servers` on every replica |
| `the server defines no macro shard or replica`, `the server reaches no ClickHouse Keeper`, or `the server setting distributed_ddl_use_initial_user_and_roles is 0` | Add what the message names to every replica's configuration ([what the cluster needs](clustered.md#point-crucible-at-a-cluster-you-run)) |
| `the replicas in cluster <name> do not run each other's queries as the user who sent them` | Add a `<secret>` to the cluster's entry in `remote_servers` on every replica, and let every replica reach the others on their native port |
| `<host>:9000 in cluster <name> did not answer, and every replica runs the ON CLUSTER statements: start it, then migrate again` | A migration step needs every replica. Start the replica; the server applies the step on its next try |
| `Number of alive replicas (1) is less than requested quorum (2/2)` (`TOO_FEW_LIVE_REPLICAS`) | An insert needs a majority of the replicas, which with two replicas is both. Start the replica that is down; three replicas keep inserting with one down |
| `the server lacks the text index or the text-search functions`, or `the server lacks the settings ...` | Use a ClickHouse release that has them: 26.3 or newer, with its default configuration |
| `applying the Postgres migrations: ... migration V5__export_jobs is missing from the filesystem` | An older image started on a database that a newer one migrated. Start the newer image, or restore the backup taken before the upgrade ([operations](operations.md#upgrades)) |
| `migration step 0003_access_workspace: ... access step A1 has not created the service policy of every base table` | An older image, such as 0.1.1, started on a database that a newer one migrated after adding a table. Start the newer image, or restore the backup taken before the upgrade ([operations](operations.md#upgrades)) |

The server also logs a warning at startup when its memory budgets add up to more than 90% of the memory it may
use, with the amount to give it. With the defaults, give the container at least 2 GiB
([operations](operations.md#memory-and-disk)).

## The web UI shows a code

When a page says **If this keeps happening, give your administrator this code**, the request failed on the server
for a reason the page does not show. The server logs the request with that code as its `request_id`, so the log
line gives the reason. Search for the code the page showed, such as:

```sh
docker compose logs crucible | grep 01a2163b-9ad5-78ef-91d3-d9c7b2ebc3a2
```

A failure the page can explain, such as a missing role, a deleted item, a conflict, a field to fix or a rate
limit, shows no code.

## Signing in

| Symptom or message | Fix |
|---|---|
| No setup link in the logs | The link is logged only while no owner exists, a few seconds after the start: `docker compose logs crucible \| grep 'setup/?token='`. Each restart logs a new one. Or create the owner with `crucible admin init` |
| `token_invalid`: `the link is invalid, expired or already used; ask for a new one` on the setup page | A setup link works once and for 24 hours. Restart the server for a new one, or use `crucible admin init` |
| `origin_invalid`: `the request's Origin is not this server's; open the UI at the server's own address, or add the origin to server.allowed_origins` | Open the UI at the address in `server.public_url`, or, without one, at the address the request reaches. A second hostname goes in `server.allowed_origins` |
| `invalid_credentials`: `the email or password is wrong` | Reset the password: `docker compose exec crucible crucible admin reset-password <email>` prints a one-time link valid for 30 minutes |
| Nobody can sign in: password sign-in is off and the sign-in provider fails at the identity provider (a deleted OAuth app, an expired client secret, a changed issuer). The server log says `nobody can sign in: password sign-in is off and the sign-in page offers no provider` when no provider is offered at all | Turn password sign-in back on from the shell: `docker compose exec crucible crucible admin enable-password-login`. It takes the server's configuration, writes to Postgres, and running servers use it at the next sign-in. Then sign in with your email and password at `/login/`. Someone who joined through a provider has no password yet: `crucible admin reset-password <email>` prints a link that sets one. Fix the provider under **Settings > Sign-in**, then turn password sign-in off again if you want |
| Logged at start: `the configuration no longer declares [auth_oidc], but password sign-in is off and no other sign-in provider is offered, so its row stays on` (or `[auth_github]`, `[auth_google]`) | That section held the last way to sign in, and its client secret, so this server signs nobody in through it. Put the section back and restart. To remove the provider, turn password sign-in on first (`docker compose exec crucible crucible admin enable-password-login`, or **Settings > Organization**), then restart: the start turns its row off |
| `conflict`: `password sign-in is the only way to sign in: add or turn on a sign-in provider first, then turn password sign-in off` | Crucible keeps a way to sign in. Turn on a provider under **Settings > Sign-in** first; without `server.public_url` the sign-in page offers no provider, so set it too |
| `conflict`: `password sign-in is off and no other sign-in provider is offered, so nobody could sign in` | Turn password sign-in on under **Settings > Organization**, or turn on another provider, before you turn off or delete this one |
| `/login/?error=not_invited&domains=...`: "This provider doesn't accept your email address" | The provider lets only those email domains sign in, at every sign-in. Sign in with an account at one of them, or change the provider's allowed domains |
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
| 504 `query_timeout` | The read hit its time limit. A read made with an API key or through the MCP server runs under a 5-second limit, and first waits, within its deadline, for one of the 16 ClickHouse statements that each Crucible server allows these reads at once; with several servers, a read that ClickHouse refuses because their reads together reached its 16 is sent again within the same deadline ([rate limits](../api/README.md#rate-limits)): narrow the time range, or send fewer reads at once. A metrics query, such as a dashboard widget's, also waits within its deadline for one of its project's `metrics.queries_per_project` turns (4 by default): show fewer widgets at once, or raise the setting ([configuration](configuration.md)) |

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
