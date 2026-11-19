# Self-hosting

Crucible is a development preview. Nothing is released yet, the data format may change between commits, and there
is no upgrade path from one preview to the next, so do not put data you need to keep in it. To get a server running,
follow [quickstart.md](quickstart.md).

A deployment is one `crucible` server, one ClickHouse and one Postgres. Postgres holds the control plane: sign-in, the
organization, projects, members, API keys and the migrations ledger. A server without Postgres takes its project keys
from a file instead and has no sign-in ([Without Postgres](#without-postgres-keys-from-a-file)).

| Way to run it | Files | State |
|---|---|---|
| Docker Compose with a bundled ClickHouse 26.8 LTS and Postgres 18 | `deploy/compose/compose.yaml` | Works, with an image you build until the first release publishes one |
| Docker Compose with a ClickHouse and a Postgres you run elsewhere | `deploy/compose/compose.cloud.yaml` | Works with a single-node ClickHouse 26.3 or newer and Postgres 15 or newer; ClickHouse Cloud is planned |
| Prebuilt binaries for Linux, macOS and Windows | GitHub releases | Planned: the first version tag will publish them |
| From source | [development.md](../development.md) | Works |

## The image

The first version tag will publish `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and
provenance attestations. A tag `v1.2.3` will push the image tags `1.2.3`, `1.2` and `latest`; a pre-release tag such as
`v1.2.3-rc.1` will push only its own. Until then, build the image from a checkout of this repository. It needs only
Docker:

```sh
docker build -f deploy/docker/Dockerfile -t crucible:dev .
```

The image (`deploy/docker/Dockerfile`) holds the `crucible` binary with the web UI embedded, on a distroless base
(`gcr.io/distroless/cc-debian12`). It runs as uid 65532, keeps its data under the volume `/var/lib/crucible`, and
carries `LICENSE` and `THIRD_PARTY_NOTICES` in `/usr/share/doc/crucible/`. Its default command is
`serve --data-dir=/var/lib/crucible/data`. A bind mount in place of the volume must be writable by uid 65532
(`chown -R 65532:65532 <dir>`).

## Docker Compose

`deploy/compose/compose.yaml` runs four services:

- `clickhouse`: ClickHouse 26.8 LTS with its data in the volume `clickhouse-data`, and the admin user `crucible_admin`,
  which may manage access;
- `postgres`: Postgres 18 with its data in the volume `postgres-data`, and the user and database `crucible`;
- `crucible`: the server with `--migrate` and its data in the volume `crucible-data`; it keeps sign-in, projects and
  API keys in the bundled Postgres;
- `ready`: a curl container that reports Crucible's `/readyz` as its health. The Crucible image has no shell, so it
  cannot run a health check inside itself; with this service `docker compose up --wait` returns once Crucible is
  ready, and `docker compose ps` shows whether it is.

Every refusal says what to change: a missing `.env` setting stops Compose with the setting's name, and a server that
cannot take ingest yet answers `/readyz` with the reason. A server given both Postgres and a keys file
(`--keys-file`) refuses to start and says to make the keys in Settings or with `crucible admin create-key` instead.

On the first start no one can sign in yet: the server logs a one-time setup link for the owner, or
`docker compose exec crucible crucible admin init` creates the owner from the command line
([quickstart.md](quickstart.md#4-create-the-organization-and-its-owner)). `--migrate` records each schema step in the
ledger table `schema_migrations` and skips it on the next start, and it applies the ClickHouse steps under a lease, so
two servers that migrate at once do not interleave.

`deploy/compose/compose.cloud.yaml` runs the server and the `ready` service against a ClickHouse and a Postgres
elsewhere, configured through `.env`. This preview supports a single-node ClickHouse only: it refuses a ClickHouse Cloud
service at startup and `/readyz` gives the reason. ClickHouse Cloud support is planned. The Postgres, 15 or newer, is
required and given as `CRUCIBLE_POSTGRES_URL` with its password in the URL and `?sslmode=require` for TLS;
`crucible config print` shows the URL with the password redacted.

## Without Postgres: keys from a file

Without Postgres the server has no control plane: nobody signs in, and there are no members and no key management.
Ingest and the API accept the project keys listed in a TOML file passed with `--keys-file`
(`CRUCIBLE_SERVER_KEYS_FILE`, `server.keys_file`), and the UI connects with one of those keys through its Connect
dialog. This mode is meant for ingest-only setups, such as a collector or a test rig, and for development:
`deploy/compose/compose.dev.yaml` runs it. With Postgres configured the server refuses a keys file.

A project id is any UUID; a key pair is a public key `cpk_` and a secret `csk_`, each followed by 43 characters from
`0-9A-Za-z` that encode 32 bytes, which a leading `0` always keeps in range. One way to make the file:

```sh
key() { printf '%s0%s' "$1" "$(openssl rand -base64 96 | tr -dc '0-9A-Za-z' | head -c 42)"; }
printf '[[project_key]]\nproject_id = "%s"\npublic_key = "%s"\nsecret = "%s"\n' \
  "$(openssl rand -hex 16 | sed -E 's/(.{8})(.{4})(.{4})(.{4})(.{12})/\1-\2-\3-\4-\5/')" \
  "$(key cpk_)" "$(key csk_)" > keys.toml
```

The file is read at startup; to change the keys, edit it and restart the server. In the image the server reads it as
uid 65532, so keep it readable by others (the default mode 644) or give it to that user (`chown 65532 keys.toml`).
Without Postgres nothing records which schema steps ran, so `--migrate` applies every idempotent step on each start.

## Binaries (planned)

The first version tag will publish archives with sha256 checksums for seven targets: Linux on x86-64 and arm64 (glibc
and static musl builds), macOS on x86-64 and Apple silicon, and Windows on x86-64. Each archive will hold the binary
with the UI embedded, `LICENSE`, `README.md` and `THIRD_PARTY_NOTICES`. Two installers will come with every release:

```sh
# Linux and macOS
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/elm-os/crucible/releases/latest/download/crucible-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/elm-os/crucible/releases/latest/download/crucible-installer.ps1 | iex"
```

`cargo-dist` builds them from `dist-workspace.toml` with `.github/workflows/release.yml`, which it generates. The
binary runs the same way as in the image; [development.md](../development.md#running-crucible-serve) describes
`crucible serve` and its flags.

## Configuration

Compose reads these from `.env` beside the Compose file:

| Variable | Default | Meaning |
|---|---|---|
| `CLICKHOUSE_PASSWORD` | none, required by `compose.yaml` | The password of `crucible_admin` in the bundled ClickHouse |
| `POSTGRES_PASSWORD` | none, required by `compose.yaml` | The password of `crucible` in the bundled Postgres |
| `CRUCIBLE_MASTER_KEY` | none, required | 32 random bytes as 64 hexadecimal characters. Crucible derives the passwords of its ClickHouse service users, the key that signs API cursors, the key that seals the secrets it stores in Postgres and the key that looks up API keys from it |
| `CRUCIBLE_CLICKHOUSE_URL` | none, required by `compose.cloud.yaml` | `https://<host>:8443` or `http://<host>:8123` |
| `CRUCIBLE_CLICKHOUSE_USER` | `crucible_admin` | The admin user in `compose.cloud.yaml`; it needs access management |
| `CRUCIBLE_CLICKHOUSE_PASSWORD` | none, required by `compose.cloud.yaml` | That user's password |
| `CRUCIBLE_POSTGRES_URL` | none, required by `compose.cloud.yaml` | `postgres://<user>:<password>@<host>:5432/<database>`, with `?sslmode=require` for TLS |
| `CRUCIBLE_IMAGE` | `ghcr.io/elm-os/crucible:latest` | The Crucible image. Pin a version here so that a pull never changes it |
| `CRUCIBLE_BIND_ADDRESS` | `0.0.0.0` | The host address the ports are published on. `127.0.0.1` keeps them on this machine |
| `CRUCIBLE_HTTP_PORT` | `8080` | Host port of the UI, the REST API and OTLP/HTTP ingest |
| `CRUCIBLE_OTLP_HTTP_PORT` | `4318` | Host port of OTLP/HTTP ingest |

The server's own flags and environment variables are listed in
[development.md](../development.md#running-crucible-serve); to change them, edit the `command` or the `environment`
of the `crucible` service. Every setting is also a `CRUCIBLE_*` variable named after its key, so a setting without a
flag is a line in that `environment`. For example the address people use, which sign-in through GitHub, Google or an
OpenID Connect provider needs:

```yaml
      CRUCIBLE_SERVER_PUBLIC_URL: https://crucible.example.com
```

Notes for containers:

- Every setting can also come from a TOML file (`--config`); `crucible config check` reports every problem at once
  and `crucible config print` shows the effective values with secrets redacted.
- Secrets can come from files instead of the environment: `--clickhouse-password-file`, `--postgres-password-file`
  and `--master-key-file`, for example from Docker secrets mounted under `/run/secrets/`.
- Idle connections to ClickHouse are kept a little less than the server's `keep_alive_timeout`. A balancer between
  Crucible and ClickHouse that drops idle connections sooner needs `CRUCIBLE_CLICKHOUSE_IDLE_TIMEOUT` (seconds)
  below its own idle timeout.
- The recent-span index takes 10% of the memory the server may use, at most 1 GiB, by default: of the container's
  memory limit (cgroup v2), or of the machine's memory without one. At startup the server adds up its memory
  budgets (the recent-span index, 512 MiB for ingest and 1 GiB for the shipper) and warns when they exceed 90% of
  that memory; the warning says what to change.

## Security

The defaults work on an internal network over plain HTTP. People sign in with a password or a configured provider,
and the API and ingest take a session or a project key; anyone who can reach port 8080 can still load the UI's static
pages. Over plain HTTP, passwords and session cookies cross the network unencrypted, so on a network you do not trust,
set `CRUCIBLE_BIND_ADDRESS=127.0.0.1` or put a TLS proxy in front and set `server.public_url` to its `https`
address. A public URL limits cookie-authenticated writes to its origin (`server.allowed_origins` adds others), and an
`https` one makes the session cookie `Secure`. With `security.require_https = true` the server refuses sign-in until
the public URL is `https`.

## Ports

| Port | Serves | Published by the Compose files |
|---|---|---|
| 8080 | The UI at `/`, the REST API under `/api/v1`, OTLP/HTTP at `/v1/traces`, `/healthz` and `/readyz` | Yes |
| 4318 | OTLP/HTTP at `/v1/traces` | Yes |
| 4317 | OTLP/gRPC (planned; nothing listens on it yet) | No |
| 9464 | Prometheus `/metrics`, on the container's loopback interface by default | No |

To scrape metrics from outside the container, add `--metrics-listen=0.0.0.0:9464` to the server's command and publish
the port.

## Health

`GET /healthz` answers `ok` while the process runs. `GET /readyz` answers `ready` once the WAL accepts writes, the
startup checks against ClickHouse have passed and, with Postgres, the API keys have loaded from it, and 503 with the
reason before that; ingest answers 503 for the same time. The checks retry with a pause that grows from 1 s to 15 s,
so Crucible may start before ClickHouse does. `docker compose logs crucible` shows the same reason.

## Upgrades

A development preview has no upgrade path: when the data format changes, start over with
`docker compose down -v`. Once releases exist, an upgrade will be:

1. Set `CRUCIBLE_IMAGE` in `.env` to the new version tag.
2. Run `docker compose pull` and `docker compose up -d --wait`. `--migrate` applies the schema steps the new version
   adds.

Keep `CRUCIBLE_MASTER_KEY` across upgrades. Crucible creates its ClickHouse service users once, with passwords
derived from that key, so a server started with another key cannot log in to the same ClickHouse data, and the API
keys and provider secrets in Postgres stop working under another key.

## Backups

A backup is four things:

1. `.env`. Without the master key, restored ClickHouse data cannot be used, and neither can the API keys and provider
   secrets in Postgres, so keep `.env` with the `postgres-data` archive it belongs to.
2. The volume `clickhouse-data`: the telemetry.
3. The volume `postgres-data`: the control plane: the organization, its users and members, projects, API keys,
   sign-in providers and the migrations ledger.
4. The volume `crucible-data`: the WAL, which holds requests that were acknowledged but are not yet in ClickHouse.

Stop the stack first, so that the server drains its open requests and the databases write nothing while the volumes
are copied, then archive each volume. With the project name `crucible` from `compose.yaml`:

```sh
docker compose stop
for v in clickhouse-data postgres-data crucible-data; do
  docker run --rm -v "crucible_$v:/data:ro" -v "$PWD:/backup" busybox:1.37 tar -czf "/backup/$v.tar.gz" -C /data .
done
docker compose start
```

To restore, put `.env` back beside the Compose file, let Compose create the empty volumes, unpack the archives into
them and start:

```sh
docker compose create
for v in clickhouse-data postgres-data crucible-data; do
  docker run --rm -v "crucible_$v:/data" -v "$PWD:/backup:ro" busybox:1.37 tar -xzf "/backup/$v.tar.gz" -C /data
done
docker compose up -d --wait
```
