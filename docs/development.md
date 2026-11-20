# Development

## Requirements

- Docker (Docker Desktop on Windows and macOS).
- Node.js 24 and pnpm 11 for the UI and the TypeScript SDK.
- Python 3.12 and uv for the Python SDK.
- Git with `core.autocrlf=false` for this repository (`.gitattributes` keeps every text file LF).

The Rust workspace builds and tests inside the dev container, which pins the toolchain from `rust-toolchain.toml`
and carries the Linux-only tools (simulation guards, Kani, cargo-fuzz).

## Dev container

```sh
docker build -t crucible-dev:1 -f deploy/dev/Dockerfile deploy/dev

# Run any cargo command with the repository mounted and caches in named volumes:
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo nextest run --workspace
```

On Windows PowerShell, replace `"$PWD"` with `"${PWD}"`.

## The web UI in the binary

`crucible serve` answers every path that no other route claims with the UI. Where the UI comes from:

- A default build has none: every page answers 404 with a note on how to add one.
- `crucible serve --ui-dir ui/out` serves a built export from a directory. The files are read once at startup, so
  restart the server after a UI rebuild.
- The `embed-ui` feature compiles `ui/out` into the binary:

```sh
pnpm --filter crucible-ui build                         # writes ui/out
cargo xtask ui compress                                 # writes .br and .gz variants beside the text files
cargo build --release -p crucible --features embed-ui
```

Run `cargo xtask ui compress` after every UI build, because the build replaces `ui/out`; without the variants the
server sends every file uncompressed. `embed-ui` fails to compile without `ui/out`, so it is off by default and CI
builds without it.

## Sending traces to a local server

`crucible serve` accepts OTLP/HTTP traces at `POST /v1/traces` on `--ingest-listen` (default `0.0.0.0:4318`) and on
the main listener, and OTLP/gRPC (`TraceService/Export`, HTTP/2 without TLS) on `--grpc-listen` (default
`0.0.0.0:4317`). Bodies are protobuf or JSON, optionally gzip-compressed, and a request is acknowledged only once its
WAL frame under `--data-dir` is durable. Both HTTP ingest listeners also serve the compat ingest routes listed in
`crates/crucible-compat/src/routes.rs`, which take other SDKs' OTLP traffic with the same bodies, limits and
acknowledgement.

Requests authenticate with a project's secret key as `Authorization: Bearer <secret key>`, or with its key pair as
`Authorization: Basic base64(<public key>:<secret key>)`. With Postgres configured the keys live there: make them in
Settings or with `crucible admin create-key --project <slug or id>`. Without Postgres, list the keys in a TOML file
passed with `--keys-file` (without one, every ingest request answers 401):

```toml
[[project_key]]
project_id = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
public_key = "cpk_..."
secret = "csk_..."
```

A public key is `cpk_` and a secret `csk_`, each followed by 43 base62 characters (`0-9A-Za-z`) that encode 32
random bytes. One way to make a pair, the public key first:

```sh
python3 -c "import secrets, string as s; a = s.digits + s.ascii_uppercase + s.ascii_lowercase; key = lambda p: p + '0' + ''.join(secrets.choice(a) for _ in range(42)); print(key('cpk_')); print(key('csk_'))"
```

## The REST API and its generated files

The REST API is `crates/crucible-api`, mounted by `crucible serve` at `/api/v1` on the main listener. Its DTOs live
in `crates/crucible-api-types`, where each struct derives serde, the OpenAPI schema and the TypeScript type at once.
Two generated outputs are committed:

- `specs/openapi/crucible-v1.json`, the OpenAPI 3.1 document, printed by the binary:
  `cargo run -p crucible -- openapi > specs/openapi/crucible-v1.json`;
- `ui/src/api/generated/`, one TypeScript file per DTO, written by `cargo xtask gen-ts`.

Regenerate both after changing a handler or a DTO, and commit the result. CI regenerates them on every pull request
and fails on any difference, or on `bigint` in a generated type (`.github/workflows/drift.yml`); a test of
`crucible-api` also fails while the committed OpenAPI document is stale.

Requests authenticate with a project's secret key as `Authorization: Bearer <key>` or, with Postgres, with a
signed-in session's cookie, which writes pair with its CSRF token (contract 11.4). Without a
telemetry store (see the next section), `crucible serve` checks the key, the project and the parameters of each
request, cursors included, and then answers 503 with the problem code `telemetry_store_unavailable`.

List cursors (`next_cursor`, `head_cursor`) are signed with HMAC-SHA256, bound to the request's project, filters,
sort and window, and expire 24 hours after they are issued; any other cursor answers 400 `cursor_invalid`, and the
client starts again from the first page.

## Running crucible serve

`crucible serve` has two modes.

**Ingest only**, without `--clickhouse-url`: requests are made durable in the WAL and stay there, because nothing
ships them. The API answers 503 as described above.

**With ClickHouse**, the mode [the quickstart](self-hosting/quickstart.md) runs:

| Flag | Environment variable | Meaning |
|---|---|---|
| `--clickhouse-url` | `CRUCIBLE_CLICKHOUSE_URL` | The HTTP endpoint, `http://host:8123` or `https://host:8443` |
| `--clickhouse-user` | `CRUCIBLE_CLICKHOUSE_USER` | The admin user the operator created; default `crucible_admin`. It needs access management |
| `--clickhouse-password-file` | `CRUCIBLE_CLICKHOUSE_PASSWORD_FILE` | A file with the admin password. Without it the password is read from `CRUCIBLE_CLICKHOUSE_PASSWORD` |
| `--master-key-file` | `CRUCIBLE_MASTER_KEY_FILE` | A file with the master key, 64 hexadecimal characters. Without it the key is read from `CRUCIBLE_MASTER_KEY` |
| `--migrate` | | Apply the schema and create the service identities before the startup checks |
| `--recent-max-bytes` | `CRUCIBLE_RECENT_MAX_BYTES` | Memory bound of the recent-span index; default 10% of the container's memory limit (`/sys/fs/cgroup/memory.max`), or of RAM without one, at most 1 GiB |

Secrets have no flag that takes the value itself, so they never appear in a process list.

### Configuration

Every setting can come from three places, each overriding the one before: a TOML file (`--config` or
`CRUCIBLE_CONFIG`), a `CRUCIBLE_*` environment variable, and a flag. A setting's key is its place in the file, and
its variable is the key in upper case: `server.listen` is `listen` under `[server]` and `CRUCIBLE_SERVER_LISTEN`.
The crate `crucible-config` holds the list (`SETTINGS`) and is the only code that reads the process environment.

```toml
master_key_file = "/run/secrets/master"   # top-level keys come before the first section

[server]
listen = "0.0.0.0:8080"
ingest_listen = "0.0.0.0:4318"
grpc_listen = "0.0.0.0:4317"
metrics_listen = "127.0.0.1:9464"
public_url = "https://crucible.example.com"

[data]
dir = "data"
wal_backend = "auto"

[clickhouse]
url = "http://clickhouse:8123"
password_file = "/run/secrets/clickhouse"

[postgres]
url = "postgres://crucible@postgres:5432/crucible"
password_file = "/run/secrets/postgres"
```

- A secret (`master_key`, `clickhouse.password`, `postgres.password`) is given as the value or as a path in its
  `_file` twin. Given both in one layer, the file is used and a warning says so.
- `sql.statements_per_hour` (default 1,800) is the hourly allowance of each project's SQL workspace identity. A run
  through the API is three statements, two checks and the query. The value is applied to ClickHouse at every start.
- `cpu.permits` (`CRUCIBLE_CPU_PERMITS`) is the CPU budget: how many blocking jobs (the inflate, transcode, scan
  and frame compression of ingest requests) run at once. The default is 8 per CPU the process may use, which
  counts its CPU affinity and cgroup quota. A request beyond it waits its turn without holding a thread, and the
  runtime keeps 8 blocking threads more for the blocking work that takes no permit (disk reads, name lookups,
  password hashing). The default is a multiple of the CPUs because the jobs share the CPUs with the runtime's
  workers, which run the rest of ingest and the shipper: with one permit per CPU, a burst on the perf gate's small
  rig acknowledged 12 to 26% fewer spans per second than with an unbounded blocking pool.
- An unknown key in the file is an error that names the nearest known key. An unknown `CRUCIBLE_*` variable is a
  warning.
- `server.keys_file` is for a server without `[postgres]`; given both, the configuration is refused with what to do.
- `crucible config check` loads the configuration and reports every problem at once; `crucible config print`
  shows the effective configuration with the source of each value and with secrets redacted. Both take the same
  flags as `serve`.

### Postgres and the migrations ledger

`[postgres]` is optional: without it there is no sign-in and the project keys come from `--keys-file`. With it, the
server keeps sign-in, the organization, projects, members and API keys there and needs the master key, and
`crucible migrate` (or `serve --migrate`) applies the Postgres migrations of `crates/crucible-control-pg/migrations`
and then the ClickHouse steps, and records each in the ledger table `schema_migrations` (contract 6.6). A step that
the ledger holds with the same checksum is skipped. The ClickHouse steps run under the `migrations` lease, and each
is recorded in a transaction fenced by the lease's token (contract 11.7), so two nodes that migrate at once do not
interleave. `crucible migrate` with only `[postgres]` migrates Postgres alone.

Startup, in order:

1. The WAL is opened and recovered, and the listeners start. Ingest answers 503 and `/readyz` answers 503 with the
   reason until step 4 has finished.
2. With `--migrate`, schema step `0001_schema_base` and access step `0002_access_service` are applied as the admin
   user. Both are idempotent, so the flag can stay on. Without Postgres nothing records which steps ran and both
   are applied each time; with Postgres the ledger decides (see above). This preview migrates a single-node
   server only.
3. Startup detection and the self-tests run (ClickHouse 26.3 or newer, every setting and function Crucible uses,
   one block inserted twice with one token leaves one row, every service identity sees a known row). A failure is
   logged and retried with a pause that grows from 1 s to 15 s, so Crucible can start before ClickHouse does.
   Detection also reads the server's `keep_alive_timeout`, and from then on an idle connection to ClickHouse is
   kept 1 s less than that (2 s when the value cannot be read). `clickhouse.idle_timeout`, in seconds, sets it
   instead, for a balancer in front of ClickHouse that drops idle connections sooner than the server does.
4. The recent-span index is created, the shipper starts, the API is mounted over the store and ingest opens. The
   shipper replays the retained WAL: sealed batches without a checkpoint are probed and shipped again with their
   original tokens, and the replay also refills the recent-span index.

The passwords of the four generated ClickHouse users (`crucible_ingest`, `crucible_read_ui`, `crucible_read_agent`,
`crucible_maint`) are never configured or stored. Each is HKDF-SHA256 of the master key with the info string
`crucible/clickhouse-identity/v1/<user>/<generation>` (generation 0), as 64 hexadecimal characters; ClickHouse
receives only its SHA-256. The users are created once, so a ClickHouse that was migrated under one master key
refuses a server started with another: detection fails and `/readyz` says so. Key rotation will arrive with
`crucible admin rotate-keys`.

The key that signs API cursors is derived the same way, with the info string `crucible/cursor-signing/v1/0`, until
signing keys move to the control plane. Every server started with the same master key opens the cursors of the
others, across restarts. Without ClickHouse no list issues a cursor, and a random key made at startup only lets
the API refuse the cursors it is sent.

On SIGTERM or Ctrl-C the listeners stop accepting and finish their open requests, whose WAL appends complete; the
shipper stops at its next step and abandons the inserts in flight. That is safe: after a restart every pair of
every sealed batch is probed before anything is sent again. Tasks get 30 s before they are aborted.

## A local build in Compose

[The quickstart](self-hosting/quickstart.md) runs the image that `deploy/docker/Dockerfile` builds. To iterate on a
binary built in the dev container instead, `deploy/compose/compose.dev.yaml` wraps `deploy/compose/bin/crucible` in
a small image beside ClickHouse 26.8, without Postgres. Build the binary with the UI inside, from the repository root:

```sh
pnpm install --frozen-lockfile
pnpm --filter crucible-ui build

docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 sh -c '
    cargo xtask ui compress &&
    cargo build --locked -p crucible --release --features embed-ui &&
    mkdir -p deploy/compose/bin && cp /target/release/crucible deploy/compose/bin/crucible'
```

Build the `crucible` package on its own, as above: a `--workspace` build also compiles test-only features of other
crates into the binary. It runs without Postgres, so it takes its project keys from a file. In `deploy/compose/`, make
`.env` as in the quickstart's step 2 (the `CRUCIBLE_IMAGE` line is not needed) and `keys.toml` as in
[Without Postgres](self-hosting/README.md#without-postgres-keys-from-a-file), and start it from that directory:

```sh
docker compose -f compose.dev.yaml up --build -d
curl -s http://127.0.0.1:8080/readyz
```

It publishes the ports on `127.0.0.1` only. `docker compose -f compose.dev.yaml down -v` removes its volumes.

## The UI against a running server

With Postgres, people sign in. The shell asks `GET /api/v1/auth/providers` whether the server has a control plane,
then loads the session and sends a browser without one to `/login/`; the code is `ui/src/lib/auth.ts` and
`ui/src/components/shell/auth-gate.tsx`. Without Postgres that check answers 404 `feature_disabled`, and the shell
has a **Connect** dialog instead, labelled as temporary: it takes a project id and that project's secret key, checks
them against the API, keeps them in this browser's local storage until you press Disconnect (a new tab or a restart
does not ask again) and sends the key as `Authorization: Bearer` on API requests. A section that answers 401 offers
the same dialog. The code is `ui/src/lib/connection.ts` and `ui/src/components/shell/connect-dialog.tsx`.

## The end-to-end test

`crates/crucible/tests/e2e.rs` runs the real binary against a real ClickHouse: OTLP traces as protobuf, JSON and
gRPC, and one on a compat ingest route with the key pair as Basic auth, come back through the API (the time from acknowledgement to visibility is recorded), a 20,000-span burst and two `kill -9` and one SIGTERM
in the middle of further runs, each followed by a restart, must lose no acknowledged span and duplicate no row (checked against ClickHouse
directly, through the load generator's `SequenceReader`), and, when a Chrome binary is given, the embedded UI's
traces table must show the traces after connecting through the dialog. Without `CRUCIBLE_E2E_CLICKHOUSE_URL` the
test passes as skipped, which is what the ordinary CI jobs see; `.github/workflows/e2e.yml` runs it for real. With
`CRUCIBLE_E2E_POSTGRES_URL` as well, naming an empty database, a second test runs the control-plane path:
`crucible admin init`, sign-in, a project and an API key made through the API, a key from
`crucible admin create-key`, ingest and read-back with each key, revocation and sign-out.

```sh
docker network create crucible-e2e
docker run -d --rm --name crucible-e2e-ch --network crucible-e2e \
  -e CLICKHOUSE_USER=crucible_admin -e CLICKHOUSE_PASSWORD=e2e-admin -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 \
  clickhouse/clickhouse-server:26.8

pnpm --filter crucible-ui build
docker run --rm --network crucible-e2e -v "$PWD":/work -v crucible-target:/target \
  -v crucible-cargo:/usr/local/cargo/registry \
  -e CRUCIBLE_E2E_BIN=/target/release/crucible -e CRUCIBLE_E2E_OUT=/work/e2e-out \
  -e CRUCIBLE_E2E_CLICKHOUSE_URL=http://crucible-e2e-ch:8123 -e CRUCIBLE_E2E_CLICKHOUSE_PASSWORD=e2e-admin \
  crucible-dev:1 sh -c '
    cargo xtask ui compress &&
    cargo build --locked -p crucible --release --features embed-ui &&
    cargo test --locked -p crucible --test e2e -- --nocapture'
```

The server under test is always the binary named by `CRUCIBLE_E2E_BIN`, built on its own: the one cargo builds
beside the test carries the test-only features of the test's dependencies. The UI check also needs
`CRUCIBLE_E2E_CHROME` (a Chrome or Chromium binary) and Node 24 where the test runs, which the dev container does
not have; `tools/e2e/ui-traces.mjs` is the script it runs. The report, the server logs and the screenshot land in
`CRUCIBLE_E2E_OUT`.

## Hooks

`pnpm install` at the repository root installs the git hooks (lefthook):

- `pre-commit` checks staged files against the naming rule;
- `commit-msg` rejects AI attribution and naming-rule violations in the message.

Run the checks by hand with `node tools/policy/policy.mjs files` and `node tools/policy/policy.mjs selftest`.

## Long paths on Windows

Some third-party repositories exceed the 260-character path limit. Keep vendor checkouts under a short root such as
`C:\src\`, and set `core.longpaths=true` on those clones.
