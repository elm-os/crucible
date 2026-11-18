# Quickstart: a development build

This page runs a development preview of Crucible from a local build: one `crucible` container and one ClickHouse
26.8 container on your machine. There is no release yet, no published image and no upgrade path, and the data
format may change between commits. Do not put data you need to keep in it.

What works in this preview: OTLP/HTTP trace ingest, the write-ahead log, shipping to ClickHouse, the REST API for
traces, and the UI's traces table and trace page. Sign-in, projects and key management will arrive with the control
plane; until then project keys come from a file and the UI connects with a key (see "Open the UI").

## Requirements

- Docker with Compose.
- Node.js 24 and pnpm 11 to build the UI.
- The dev container image from [development.md](../development.md): `docker build -t crucible-dev:1 -f deploy/dev/Dockerfile deploy/dev`.

## 1. Build the binary with the UI inside

From the repository root:

```sh
pnpm install --frozen-lockfile
pnpm --filter crucible-ui build

docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 sh -c '
    cargo xtask ui compress &&
    cargo build --locked -p crucible --release --features embed-ui &&
    mkdir -p deploy/compose/bin && cp /target/release/crucible deploy/compose/bin/crucible'
```

Build the `crucible` package on its own, as above. A `--workspace` build also compiles test-only features of other
crates into the binary.

## 2. Write the three local files

All three live in `deploy/compose/` and are ignored by git.

`keys.toml` lists the project keys that ingest and the API accept. A project id is any UUID; a key pair is a public
key `cpk_` and a secret `csk_`, each followed by 43 characters from `0-9A-Za-z`:

```sh
cd deploy/compose
python3 - > keys.toml <<'PY'
import secrets, string, uuid
alphabet = string.digits + string.ascii_letters
# 43 characters encode 32 bytes only below 2^256; a leading zero keeps the value in range.
key = lambda prefix: prefix + "0" + "".join(secrets.choice(alphabet) for _ in range(42))
print(f'[[project_key]]\nproject_id = "{uuid.uuid4()}"\npublic_key = "{key("cpk_")}"\nsecret = "{key("csk_")}"')
PY
```

`.env` holds the ClickHouse admin password and the master key, 32 random bytes as 64 hexadecimal characters:

```sh
printf 'CLICKHOUSE_PASSWORD=%s\nCRUCIBLE_MASTER_KEY=%s\n' "$(openssl rand -hex 16)" "$(openssl rand -hex 32)" > .env
```

Keep the master key. Crucible derives the passwords of its four ClickHouse service users from it, and the users are
created once: a server started with a different master key cannot log in to the same ClickHouse data. To start over,
remove the volumes with `docker compose -f compose.dev.yaml down -v`.

## 3. Start

```sh
docker compose -f compose.dev.yaml up --build -d
curl -s http://127.0.0.1:8080/readyz     # "ready" once the startup checks have passed
```

On start Crucible applies the ClickHouse schema (`--migrate`), checks the server (version 26.3 or newer, the settings
and functions it relies on), and inserts one block twice to prove that deduplication works. Until that has passed,
ingest answers 503 and `/readyz` says what is missing; `docker compose -f compose.dev.yaml logs crucible` shows the
same reason.

## 4. Send a trace

Any OpenTelemetry SDK or collector that exports OTLP over HTTP works. Point it at `http://127.0.0.1:4318/v1/traces`
and send the secret from `keys.toml` as `Authorization: Bearer <secret>`. With the standard environment variables:

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

## 5. Open the UI

Open <http://127.0.0.1:8080/traces/>. The page asks you to connect: choose **Connect** in the header and paste the
project id and the secret from `keys.toml`.

The Connect dialog is temporary. This preview has no sign-in, so the UI sends the project key as
`Authorization: Bearer` on its API requests and remembers it in this browser until you press Disconnect. Browser sessions will replace the dialog in milestone 4. Do not expose this
preview to a network you do not trust.

The same data is available from the API, for example the trace sent above:

```sh
curl -s -H "Authorization: Bearer <secret>" \
  "http://127.0.0.1:8080/api/v1/projects/<project id>/traces/5b8efff798038103d269b633813fc60c"
```

The trace list takes a time window of at most 30 days: `.../traces?from=<RFC 3339>&to=<RFC 3339>`.

## With Postgres: sign-in, members and API keys

With a Postgres 15 or newer configured (`postgres.url`, `CRUCIBLE_POSTGRES_URL`), the server has a control plane:
people sign in, projects and members live in Postgres, and API keys are made in the UI instead of `keys.toml`. The
master key is required then; it also seals stored secrets.

- Start with `--migrate` once, so the Postgres tables exist. Until the API key snapshot has loaded from Postgres,
  `/readyz` answers 503 and ingest is closed.
- While no owner exists, the log prints a one-time link, `/setup/?token=...`, valid for 24 hours. Open it to create the
  organization and its owner, or run `crucible admin init --org-name <name> --email <email> --name <name>` with the
  password on standard input or in `--password-file`. Either way the server also creates a first project,
  `default`, so the owner can make an API key in Settings at once.
- Invite people from Settings: the invitation link is shown once, to pass on. This version sends no email, so a
  forgotten password is reset with `crucible admin reset-password <email>`, which prints a link valid for 30 minutes.
- Sign-in through GitHub, Google or an OpenID Connect provider needs `server.public_url` (the address people use,
  which the provider redirects back to). An `https` public URL also makes the session cookie `Secure`; over plain HTTP
  sign-in still works.
- If Postgres becomes unreachable, open sessions keep working for `auth.session_outage_grace` (10 minutes by
  default), and keys the server already knows keep working for ingest (`auth.pg_outage_mode = "fail_open"`).

## Without Compose

`crucible serve` runs the same way outside a container. The flags, their environment variables and the two modes
(with ClickHouse, or ingest only) are described in [development.md](../development.md#running-crucible-serve).

## Stop

```sh
docker compose -f compose.dev.yaml down        # keeps the data
docker compose -f compose.dev.yaml down -v     # removes it
```
