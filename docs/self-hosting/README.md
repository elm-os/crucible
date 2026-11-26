# Self-hosting

A Crucible deployment is one `crucible` server, one ClickHouse and one Postgres:

- **The server** is one binary. It ingests OTLP, keeps every acknowledged request in a write-ahead log on its own
  disk until ClickHouse has it, and serves the UI, the REST API and the MCP server.
- **ClickHouse** stores the telemetry. Crucible needs ClickHouse 26.3 or newer, as a single server or as a cluster of
  one shard with several replicas ([clustered ClickHouse](clustered.md)), and an admin user that may manage access,
  which it uses to create its own service users. ClickHouse Cloud is refused at startup.
- **Postgres** holds the control plane: sign-in, the organization, projects, members, API keys, alert rules and
  channels, saved queries, retention settings, deletion requests, export jobs and the migrations ledger. Crucible
  needs Postgres 15 or newer. A server without Postgres takes its project keys from a file instead and has no
  sign-in ([without Postgres](#without-postgres-keys-from-a-file)).

To get a server running, follow the [quickstart](quickstart.md).

| Page | What it covers |
|---|---|
| [Quickstart](quickstart.md) | Docker Compose with a bundled ClickHouse and Postgres, from an empty machine to a first trace |
| [Configuration](configuration.md) | Every setting, with its key, environment variable, flag and default |
| [Operations](operations.md) | Upgrades, backups and restores, health checks, Prometheus metrics and logs |
| [Security](security.md) | What is protected by default, and the switches that harden a deployment |
| [Sign-in and the organization](sign-in.md) | Project access, sign-up by email domain, and GitHub, Google and OpenID Connect sign-in |
| [Retention and deletion](retention-and-deletion.md) | How long data is kept, and deleting a trace, a user or a session |
| [Alerts and egress](alerts-and-egress.md) | The outbound calls of webhook, Slack and email channels, and how to limit them |
| [SQL workspace](sql-workspace.md) | The per-project ClickHouse users behind the SQL workspace, and its quota |
| [Troubleshooting](troubleshooting.md) | Each common refusal, what it means and how to fix it |
| [Media](media.md) | Images, audio and files on S3-compatible object storage, with the bundled gateway or a bucket of your own |
| [Clustered ClickHouse](clustered.md) | A replicated ClickHouse cluster with Keeper: the bundled two-replica Compose file, pointing Crucible at your own cluster, and what happens when a replica is down |

## Ways to run it

| Way | Files |
|---|---|
| Docker Compose with a bundled ClickHouse 26.8 and Postgres 18 | `deploy/compose/compose.yaml`, in the [quickstart](quickstart.md) |
| Docker Compose against a ClickHouse and a Postgres you run elsewhere | `deploy/compose/compose.cloud.yaml`, [below](#your-own-clickhouse-and-postgres) |
| Docker Compose with two ClickHouse replicas, ClickHouse Keeper and Postgres 18 | `deploy/compose/compose.cluster.yaml`, in [clustered ClickHouse](clustered.md) |
| Docker Compose with a binary you built, without Postgres, for development | `deploy/compose/compose.dev.yaml`, in [development.md](../development.md#a-local-build-in-compose) |
| The binary on its own | [below](#the-binary) |

## The image

Each release is published as `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and provenance
attestations. A version such as `v0.1.0` is tagged `0.1.0`, `0.1` and `latest`; the Compose files run `latest`
unless `CRUCIBLE_IMAGE` in `.env` names another tag.

```sh
docker pull ghcr.io/elm-os/crucible:0.1.3
```

To build the image yourself from a checkout of this repository, which needs only Docker:

```sh
docker build -f deploy/docker/Dockerfile -t crucible:dev .
```

The build compiles the UI and the server inside Docker and takes several minutes. Set `CRUCIBLE_IMAGE=crucible:dev`
in `.env` to run it with Compose.

The image (`deploy/docker/Dockerfile`) holds the `crucible` binary with the UI embedded, on a distroless base
(`gcr.io/distroless/cc-debian12`). It has no shell. It runs as uid 65532, keeps its data in the volume
`/var/lib/crucible`, and carries `LICENSE` and `THIRD_PARTY_NOTICES` in `/usr/share/doc/crucible/`. Its default
command is `serve --data-dir=/var/lib/crucible/data`. A bind mount in place of the volume must be writable by uid
65532 (`chown -R 65532:65532 <dir>`).

## Docker Compose

`deploy/compose/compose.yaml` runs four services:

- `clickhouse`: ClickHouse 26.8 with its data in the volume `clickhouse-data`, and the admin user `crucible_admin`,
  which may manage access;
- `postgres`: Postgres 18 with its data in the volume `postgres-data`, and the user and database `crucible`;
- `crucible`: the server, started with `--migrate`, with its data in the volume `crucible-data`;
- `ready`: a curl container that reports Crucible's `/readyz` as its health. The Crucible image has no shell, so it
  cannot run a health check inside itself; with this service `docker compose up --wait` returns once Crucible is
  ready, and `docker compose ps` shows whether it is.

Every start migrates: `--migrate` applies the Postgres migrations and the ClickHouse schema steps that are missing,
records each in the ledger table `schema_migrations` in Postgres, and skips the ones the ledger already holds. The
ClickHouse steps run under a lease, so two servers that start at once do not interleave them. The server writes
only to its data volume; the container's root filesystem is read-only.

`.env` beside the Compose file holds the passwords, the master key and the optional settings
([configuration](configuration.md#docker-compose-variables)).

`deploy/compose/compose.media.yaml` adds media to `compose.yaml`: an S3-compatible gateway with its own volume, which
holds the images, audio and files that SDKs upload beside their traces
(`docker compose -f compose.yaml -f compose.media.yaml up -d --wait`). Without it media stay inline in payloads. See
[media](media.md).

### Your own ClickHouse and Postgres

`deploy/compose/compose.cloud.yaml` runs the server and the `ready` service against a ClickHouse and a Postgres
that run elsewhere. Its `.env` names them:

```sh
CRUCIBLE_CLICKHOUSE_URL=http://clickhouse.internal:8123
CRUCIBLE_CLICKHOUSE_USER=crucible_admin
CRUCIBLE_CLICKHOUSE_PASSWORD=<that user's password>
CRUCIBLE_POSTGRES_URL=postgres://crucible:<password>@postgres.internal:5432/crucible
CRUCIBLE_MASTER_KEY=<openssl rand -hex 32>
```

```sh
curl -fsSLO https://raw.githubusercontent.com/elm-os/crucible/v0.1.3/deploy/compose/compose.cloud.yaml
docker compose -f compose.cloud.yaml up -d --wait
```

- The ClickHouse user needs access management, because Crucible creates its service users and their row policies
  with it. With the official ClickHouse image, `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` gives it to the user the
  image creates, as `compose.yaml` does.
- For a replicated cluster, point `CRUCIBLE_CLICKHOUSE_URL` at one replica and add
  `CRUCIBLE_CLICKHOUSE_CLUSTER=<the cluster's name in remote_servers>`; [clustered ClickHouse](clustered.md) lists
  what the cluster needs.
- `https://<host>:8443` connects to ClickHouse over TLS, and `?sslmode=require` at the end of the Postgres URL
  connects to Postgres over TLS. `crucible config print` shows the URL with the password redacted.
- Create the owner the same way as with `compose.yaml` ([quickstart](quickstart.md#3-create-the-organization-and-its-owner)),
  with `docker compose -f compose.cloud.yaml exec crucible crucible admin init ...`.

### Without Postgres: keys from a file

Without Postgres the server has no control plane: nobody signs in, and there are no members, no key management,
no alerts, no SQL workspace, no exports through the API and no deletion requests. Ingest and the read API accept
the project keys listed in a TOML file passed with `--keys-file` (`server.keys_file`,
`CRUCIBLE_SERVER_KEYS_FILE`), and the UI connects with one of those keys through its **Connect** dialog. This mode
suits an ingest-only setup, such as a collector or a test rig, and development: `deploy/compose/compose.dev.yaml`
runs it. A server with Postgres refuses a keys file.

```toml
[[project_key]]
project_id = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
public_key = "cpk_..."
secret = "csk_..."
```

A project id is any UUID. A key pair is a public key `cpk_` and a secret `csk_`, each followed by 43 characters from
`0-9A-Za-z` that encode 32 bytes, which a leading `0` always keeps in range. One way to make the file:

```sh
key() { printf '%s0%s' "$1" "$(openssl rand -base64 96 | tr -dc '0-9A-Za-z' | head -c 42)"; }
printf '[[project_key]]\nproject_id = "%s"\npublic_key = "%s"\nsecret = "%s"\n' \
  "$(openssl rand -hex 16 | sed -E 's/(.{8})(.{4})(.{4})(.{4})(.{12})/\1-\2-\3-\4-\5/')" \
  "$(key cpk_)" "$(key csk_)" > keys.toml
```

A key in the file has the scopes `ingest`, `read` and `write`. The file is read at startup; to change the keys,
edit it and restart the server. In the image the server reads it as uid 65532, so keep it readable by others
(mode 644) or give it to that user (`chown 65532 keys.toml`). Without Postgres nothing records which schema steps
ran, so `--migrate` applies every step, each of which is idempotent, on every start.

## The binary

The server runs the same way outside a container. Each release carries the `crucible` binary, with the UI embedded,
for Linux (x86_64 and aarch64, glibc and musl), macOS (x86_64 and Apple silicon) and Windows (x86_64), as archives
with checksums, and installers for a shell and for PowerShell:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/elm-os/crucible/releases/download/v0.1.3/crucible-installer.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/elm-os/crucible/releases/download/v0.1.3/crucible-installer.ps1 | iex"
```

The shell installer needs `curl`, and `tar` with xz support (`xz-utils` on Debian and Ubuntu). It installs `crucible`
into `$HOME/.cargo/bin` and says how to put that directory on your `PATH`. Each archive is also on the release page
with its SHA-256 checksum.

The write-ahead log's durability, that an acknowledged request survives a crash or a power loss, holds on Linux and
macOS, where a sync reaches the disk (`fdatasync`, and `F_FULLFSYNC` on macOS). On Windows a directory sync is not a
durability barrier, so the Windows binary is for development.

The glibc builds need glibc 2.34 or newer, as on Debian 12 or Ubuntu 22.04 and later; the musl builds have no such
requirement. The image holds the same binary at `/usr/local/bin/crucible`, and `docker cp` copies it out of a
created container. To build it with cargo instead, see [development.md](../development.md#a-local-build-in-compose).
`crucible serve --help` lists its flags; [configuration](configuration.md) lists every setting.

## Ports

| Port | Serves | Published by `compose.yaml` |
|---|---|---|
| 8080 | The UI at `/`, the REST API under `/api/v1`, the MCP server at `/mcp`, OTLP/HTTP at `/v1/traces`, `/healthz` and `/readyz` | Yes |
| 4318 | OTLP/HTTP at `/v1/traces` | Yes |
| 4317 | OTLP/gRPC | No: add `- "${CRUCIBLE_BIND_ADDRESS:-0.0.0.0}:4317:4317"` to the `ports` of the `crucible` service, so that `CRUCIBLE_BIND_ADDRESS` applies to it too |
| 9464 | Prometheus `/metrics`, on the container's loopback interface by default | No ([operations](operations.md#metrics)) |
