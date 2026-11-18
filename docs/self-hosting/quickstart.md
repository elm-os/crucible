# Quickstart

This page runs a development preview of Crucible with Docker Compose: the `crucible` server, ClickHouse 26.8 and
Postgres 18 on one machine. There is no release yet and no upgrade path, and the data format may change between commits. Do not put
data you need to keep in it. [README.md](README.md) covers configuration, ports, upgrades and backups.

What works in this preview: OTLP/HTTP trace ingest, the write-ahead log, shipping to ClickHouse, the control plane in
Postgres (sign-in, the organization, projects, members, API keys and the migrations ledger), the REST API for traces,
and the UI's traces table and trace page.

## Requirements

- Docker Engine 25 or newer with Docker Compose v2 or newer.
- `openssl` and a POSIX shell for the first-run commands: Linux, macOS, or WSL or Git Bash on Windows.
- A checkout of this repository, until the first release publishes the image.

## 1. Build the image

From the repository root:

```sh
docker build -f deploy/docker/Dockerfile -t crucible:dev .
```

The build needs only Docker: it compiles the UI and the server inside the build and takes a few minutes. Once
releases exist, skip this step; Compose pulls `ghcr.io/elm-os/crucible`.

## 2. Make `.env`

`.env` lives in `deploy/compose/` and is ignored by git. It holds the ClickHouse admin password, the Postgres password
and the master key, 32 random bytes as 64 hexadecimal characters. Make it once, with one line, and name the image you
built:

```sh
cd deploy/compose
printf 'CLICKHOUSE_PASSWORD=%s\nPOSTGRES_PASSWORD=%s\nCRUCIBLE_MASTER_KEY=%s\n' "$(openssl rand -hex 16)" "$(openssl rand -hex 16)" "$(openssl rand -hex 32)" > .env
echo 'CRUCIBLE_IMAGE=crucible:dev' >> .env
```

Keep the master key. Crucible derives the passwords of its four ClickHouse service users from it, and the users are
created once: a server started with a different master key cannot log in to the same ClickHouse data. The API keys
and the sign-in provider secrets in Postgres depend on it too. To start over, remove the volumes with
`docker compose down -v`.

## 3. Start

```sh
docker compose up -d --wait
curl -s http://127.0.0.1:8080/readyz     # "ready"
```

`--wait` returns once the `ready` service reports Crucible's `/readyz` as healthy. On start Crucible applies the
Postgres migrations and the ClickHouse schema (`--migrate`) and records each step in the ledger in Postgres, checks
the ClickHouse server (version 26.3 or newer, the settings and functions it relies on), inserts one block twice
to prove that deduplication works, and loads the API keys from Postgres. Until that has passed, ingest answers 503 and
`/readyz` says what is missing; `docker compose logs crucible` shows the same reason.

The ports are published on every interface, so other machines on your network can send traces and sign in. Add
`CRUCIBLE_BIND_ADDRESS=127.0.0.1` to `.env` to keep them on this machine.

## 4. Create the organization and its owner

A new server has no users. The first one, the owner, is created with a one-time setup link, which this page uses, or
with `crucible admin init` from the command line. Either way the server also creates the organization and a first
project, `Default`, whose slug is `default`.

While no owner exists, the server logs the setup link a few seconds after it starts:

```sh
docker compose logs crucible | grep 'setup/?token='
```

The line reads `no owner exists yet: open <this server's address>/setup/?token=<token> ...`. Open
`http://127.0.0.1:8080/setup/?token=<token>`, with the address you reach the server at in place of
`127.0.0.1:8080`, and give the organization's name, your name, your email and a password of 12 to 128 characters.
The page signs you in as the owner. The link works once and for 24 hours; a restart logs a new one.

For scripts, `crucible admin init` does the same without a browser and reads the password from standard input, as
`deploy/compose/smoke-test.sh` does:

```sh
printf '%s\n' "$OWNER_PASSWORD" | docker compose exec -T crucible \
  crucible admin init --org-name Acme --email you@example.com --name 'Your Name'
```

Then sign in at <http://127.0.0.1:8080/login/>.

Sign-in works over plain HTTP: the session cookie is sent without `Secure`, and the server accepts the address the
page was opened at. On a network you do not trust, put a TLS proxy in front ([README.md](README.md#security)).

## 5. Make an API key

Open **Settings** and then **API keys** (<http://127.0.0.1:8080/settings/api-keys/>). The page shows the keys of the
project chosen in the header, `Default` at first. To use another project, choose **New project** beside the Settings
tabs (owners and admins have it), give it a name, and the page moves to that project's API keys.

Choose **Create key** and copy the secret key, `csk_...`, from the dialog. It is shown once: Crucible keeps only a
hash of it. A new key may ingest, read and write (the scopes `ingest`, `read` and `write`).

From the command line, `crucible admin create-key` makes a key and prints `project_id`, `public_key` and `secret_key`:

```sh
docker compose exec crucible crucible admin create-key --project default
```

A key made there reaches the running server within 5 seconds; until then ingest answers 401 with it.

## 6. Send a trace

Any OpenTelemetry SDK or collector that exports OTLP over HTTP works. Point it at `http://127.0.0.1:4318/v1/traces`
and send the secret key as `Authorization: Bearer <secret>`. With the standard environment variables:

```sh
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <secret>"
```

Exporters that speak OTLP over gRPC use `http://127.0.0.1:4317` with the same header
(`OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=grpc`).

Or by hand:

```sh
NOW=$(date +%s)
curl -s -X POST http://127.0.0.1:4318/v1/traces \
  -H "Authorization: Bearer <secret>" -H 'Content-Type: application/json' \
  -d '{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"quickstart"}}]},
       "scopeSpans":[{"spans":[{"traceId":"5b8efff798038103d269b633813fc60c","spanId":"eee19b7ec3c1b174",
       "name":"hello crucible","kind":1,"startTimeUnixNano":"'$NOW'000000000","endTimeUnixNano":"'$NOW'250000000"}]}]}]}'
```

A request is acknowledged once it is durable in the write-ahead log.

## 7. See it

Open <http://127.0.0.1:8080/traces/>: the trace sent above is in the list, and choosing it opens the trace page. The
project menu in the header switches between projects.

The same data is available from the API with the key, for example the trace sent above:

```sh
curl -s -H "Authorization: Bearer <secret>" \
  "http://127.0.0.1:8080/api/v1/projects/<project id>/traces/5b8efff798038103d269b633813fc60c"
```

`crucible admin create-key` prints the project id as `project_id`; in the UI, choosing the project in the header's
menu puts it in the address bar as `?project=<project id>`. The trace list takes a time window of at most 30 days:
`.../traces?from=<RFC 3339>&to=<RFC 3339>`.

## People and outages

- Invite people from **Settings**, **Members**: the invitation link is shown once, to pass on. This version sends no
  email, so a forgotten password is reset with `docker compose exec crucible crucible admin reset-password <email>`,
  which prints a link valid for 30 minutes.
- Sign-in through GitHub, Google or an OpenID Connect provider needs the address people use, which the provider
  redirects back to, as `server.public_url` ([README.md](README.md#configuration)). An `https` public URL also makes
  the session cookie `Secure`.
- If Postgres becomes unreachable, open sessions keep working for `auth.session_outage_grace` (10 minutes by
  default), and keys the server already knows keep working for ingest (`auth.pg_outage_mode = "fail_open"`).

## Stop

```sh
docker compose down        # keeps the data
docker compose down -v     # removes it
```

## Other ways to run it

- Against a ClickHouse and a Postgres you run elsewhere: `deploy/compose/compose.cloud.yaml`, described in
  [README.md](README.md).
- Without Postgres, with project keys from a file and no sign-in:
  [README.md](README.md#without-postgres-keys-from-a-file).
- Without Compose: `crucible serve` runs the same way outside a container. The flags, their environment variables and
  the two modes (with ClickHouse, or ingest only) are in [development.md](../development.md#running-crucible-serve).
- With a binary you built in the dev container: `deploy/compose/compose.dev.yaml`, described in
  [development.md](../development.md#a-local-build-in-compose).
