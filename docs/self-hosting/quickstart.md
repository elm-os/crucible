# Quickstart

This page runs Crucible with Docker Compose: the `crucible` server, ClickHouse 26.8 and Postgres 18 on one machine,
from an empty machine to a first trace. [Configuration](configuration.md), [operations](operations.md) (upgrades,
backups, health and metrics) and the rest of running Crucible are in [self-hosting](README.md).

## Requirements

- Docker Engine 25 or newer with Docker Compose v2 or newer.
- `curl`, `openssl` and a POSIX shell for the first-run commands: Linux, macOS, or WSL or Git Bash on Windows.

## 1. Get the Compose file and make `.env`

Make a folder for Crucible, download the Compose file of the release into it, and write `.env` beside it:

```sh
mkdir crucible && cd crucible
curl -fsSLO https://raw.githubusercontent.com/elm-os/crucible/v0.1.3/deploy/compose/compose.yaml
printf '%s\n' "CLICKHOUSE_PASSWORD=$(openssl rand -hex 16)" "POSTGRES_PASSWORD=$(openssl rand -hex 16)" "CRUCIBLE_MASTER_KEY=$(openssl rand -hex 32)" > .env
```

`.env` holds the ClickHouse admin password, the Postgres password and the master key, 32 random bytes as 64
hexadecimal characters. Keep the master key. Crucible derives the passwords of its four ClickHouse service users from
it, and the users are created once: a server started with a different master key cannot log in to the same
ClickHouse data. The API keys and the sign-in provider secrets in Postgres depend on it too. To start over, remove
the volumes with `docker compose down -v`.

## 2. Start

```sh
docker compose up -d --wait
curl -s http://127.0.0.1:8080/readyz     # "ready"
```

Compose pulls the image `ghcr.io/elm-os/crucible:latest`, ClickHouse 26.8 and Postgres 18 on the first start.
`--wait` returns once the `ready` service reports Crucible's `/readyz` as healthy. On start Crucible applies the
Postgres migrations and the ClickHouse schema (`--migrate`) and records each step in the ledger in Postgres, checks
the ClickHouse server (version 26.3 or newer, the settings and functions it relies on), inserts one block twice
to prove that deduplication works, and loads the API keys from Postgres. Until that has passed, ingest answers 503 and
`/readyz` says what is missing; `docker compose logs crucible` shows the same reason.

The ports are published on every interface, so other machines on your network can send traces and sign in. Add
`CRUCIBLE_BIND_ADDRESS=127.0.0.1` to `.env` to keep them on this machine.

## 3. Create the organization and its owner

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
page was opened at. On a network you do not trust, put a TLS proxy in front ([security](security.md#plain-http-and-tls)).

## 4. Make an API key

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

## 5. Send a trace

Any OpenTelemetry SDK or collector that exports OTLP over HTTP works. Point it at `http://127.0.0.1:4318/v1/traces`
and send the secret key as `Authorization: Bearer <secret>`. With the standard environment variables:

```sh
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <secret>"
```

The Compose file publishes OTLP over HTTP. To send OTLP/gRPC from outside the Compose network, add `- "4317:4317"`
to the `ports` of the `crucible` service.

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

## 6. See it

Open <http://127.0.0.1:8080/traces/>: the trace sent above is in the list, and choosing it opens the trace page. The
project menu in the header switches between projects.

The same data is available from the API with the key, for example the trace sent above:

```sh
curl -s -H "Authorization: Bearer <secret>" \
  "http://127.0.0.1:8080/api/v1/projects/<project id>/traces/5b8efff798038103d269b633813fc60c"
```

`crucible admin create-key` prints the project id as `project_id`. In the UI, **Settings**, **Project** shows the
project's name, slug and id, with **Copy id**, and project admins rename the project there; choosing a project in
the header's menu also puts its id in the address bar as `?project=<project id>`. The trace list takes a time window
of at most 30 days: `.../traces?from=<RFC 3339>&to=<RFC 3339>`.

## People and outages

- Invite people from **Settings**, **Members**: the invitation link is shown once, to pass on. Crucible sends no
  sign-in email, so a forgotten password is reset with
  `docker compose exec crucible crucible admin reset-password <email>`, which prints a one-time link valid for
  30 minutes. Without `server.public_url` it prints the path,
  `/reset/?token=...`, to open at the address you reach the server at.
- Sign-in through GitHub, Google or an OpenID Connect provider needs the address people use, which the provider
  redirects back to, as `server.public_url` ([configuration](configuration.md#server)). An `https` public URL also
  makes the session cookie `Secure`. Add providers in **Settings**, **Sign-in** ([sign-in](sign-in.md)).
- If Postgres becomes unreachable, open sessions keep working for `auth.session_outage_grace` (10 minutes by
  default), and keys the server already knows keep working for ingest (`auth.pg_outage_mode = "fail_open"`).
  Shipping to ClickHouse goes on too: the server keeps each project's salt for MCP user subjects in memory, loaded
  at startup and every 5 seconds. Only an MCP operation with a user subject of a project whose salt it has not
  loaded yet (a project created in the last few seconds, a project created by an earlier version that has not
  needed its salt yet, or any project before the server first reached Postgres) waits for Postgres, and holds back
  the telemetry received after it.

## Stop

```sh
docker compose down        # keeps the data
docker compose down -v     # removes it
```

## Building the image from source

To run an image you build yourself, for example from a commit on `main`, clone the repository and build it. The
build needs only Docker: it compiles the UI and the server inside the build and takes several minutes.

```sh
git clone https://github.com/elm-os/crucible.git && cd crucible
docker build -f deploy/docker/Dockerfile -t crucible:dev .
```

Then make `.env` in `deploy/compose/` as in step 1, name the image in it, and start:

```sh
cd deploy/compose
printf '%s\n' "CLICKHOUSE_PASSWORD=$(openssl rand -hex 16)" "POSTGRES_PASSWORD=$(openssl rand -hex 16)" "CRUCIBLE_MASTER_KEY=$(openssl rand -hex 32)" > .env
echo 'CRUCIBLE_IMAGE=crucible:dev' >> .env
docker compose up -d --wait
```

The rest of this page applies unchanged.

## Other ways to run it

- Against a ClickHouse and a Postgres you run elsewhere: `deploy/compose/compose.cloud.yaml`
  ([self-hosting](README.md#your-own-clickhouse-and-postgres)).
- Without Postgres, with project keys from a file and no sign-in:
  [without Postgres](README.md#without-postgres-keys-from-a-file).
- Without Compose: the release binaries install with a shell or PowerShell one-liner, and `crucible serve` runs the
  same way outside a container ([the binary](README.md#the-binary), [configuration](configuration.md)).
- With a binary you built in the dev container: `deploy/compose/compose.dev.yaml`
  ([development.md](../development.md#a-local-build-in-compose)).
