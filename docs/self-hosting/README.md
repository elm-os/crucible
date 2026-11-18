# Self-hosting

Crucible is a development preview. Nothing is released yet, the data format may change between commits, and there
is no upgrade path from one preview to the next, so do not put data you need to keep in it. To get a server running,
follow [quickstart.md](quickstart.md).

A deployment is one `crucible` server and one ClickHouse. Postgres will join when the control plane lands (sign-in,
projects and key management); until then the server does not use it, and project keys come from a file.

| Way to run it | Files | State |
|---|---|---|
| Docker Compose with a bundled ClickHouse 26.8 LTS | `deploy/compose/compose.yaml` | Works, with an image you build until the first release publishes one |
| Docker Compose with a ClickHouse you run elsewhere | `deploy/compose/compose.cloud.yaml` | Works with a single-node ClickHouse 26.3 or newer; ClickHouse Cloud is planned |
| Prebuilt binaries for Linux, macOS and Windows | GitHub releases | Planned: the first version tag will publish them |
| From source | [development.md](../development.md) | Works |

## The image

The first version tag will publish `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and
provenance attestations. A tag `v1.2.3` will push the image tags `1.2.3`, `1.2` and `latest`; a pre-release tag such as
`v1.2.3-rc.1` pushes only its own. Until then, build the image from a checkout of this repository. It needs only
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

`deploy/compose/compose.yaml` runs three services:

- `clickhouse`: ClickHouse 26.8 LTS with its data in the volume `clickhouse-data`, and the admin user `crucible_admin`,
  which may manage access;
- `crucible`: the server with `--migrate`, its data in the volume `crucible-data`, and the project keys from
  `keys.toml` beside the Compose file;
- `ready`: a curl container that reports Crucible's `/readyz` as its health. The Crucible image has no shell, so it
  cannot run a health check inside itself; with this service `docker compose up --wait` returns once Crucible is
  ready, and `docker compose ps` shows whether it is.

Every refusal says what to change: a missing `.env` setting stops Compose with the setting's name, a missing
`keys.toml` stops the start with the missing path, and a server that cannot take ingest yet answers `/readyz` with the
reason.

**Why there is no Postgres service.** Nothing in this preview connects to Postgres. An idle database would cost memory
and a password and add a volume to back up, for no benefit. When the control plane lands, the Compose file will gain a
Postgres 18 service and volume; a server that ran without them has nothing to migrate.

`deploy/compose/compose.cloud.yaml` runs the server and the `ready` service against a ClickHouse elsewhere, configured
through `.env`. This preview supports a single-node ClickHouse only: it refuses a ClickHouse Cloud service at startup
and `/readyz` gives the reason. ClickHouse Cloud and a managed Postgres are planned; their settings will go in the same
file.

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
| `CLICKHOUSE_PASSWORD` | none, required | The password of `crucible_admin` in the bundled ClickHouse (`compose.yaml`) |
| `CRUCIBLE_MASTER_KEY` | none, required | 32 random bytes as 64 hexadecimal characters. Crucible derives the passwords of its ClickHouse service users and the key that signs API cursors from it |
| `CRUCIBLE_CLICKHOUSE_URL` | none, required by `compose.cloud.yaml` | `https://<host>:8443` or `http://<host>:8123` |
| `CRUCIBLE_CLICKHOUSE_USER` | `crucible_admin` | The admin user in `compose.cloud.yaml`; it needs access management |
| `CRUCIBLE_CLICKHOUSE_PASSWORD` | none, required by `compose.cloud.yaml` | That user's password |
| `CRUCIBLE_IMAGE` | `ghcr.io/elm-os/crucible:latest` | The Crucible image. Pin a version here so that a pull never changes it |
| `CRUCIBLE_BIND_ADDRESS` | `0.0.0.0` | The host address the ports are published on. `127.0.0.1` keeps them on this machine |
| `CRUCIBLE_HTTP_PORT` | `8080` | Host port of the UI, the REST API and OTLP/HTTP ingest |
| `CRUCIBLE_OTLP_HTTP_PORT` | `4318` | Host port of OTLP/HTTP ingest |

The defaults work on an internal network over plain HTTP. This preview has no sign-in: the API and ingest need a
project key, but anyone who can reach port 8080 can load the UI's pages. Set `CRUCIBLE_BIND_ADDRESS=127.0.0.1`, or
put a TLS proxy in front, on a network you do not trust.

The server's own flags and environment variables are listed in
[development.md](../development.md#running-crucible-serve); to change them, edit the `command` of the `crucible`
service. Two notes for containers:

- Secrets can come from files instead of the environment: `--clickhouse-password-file` and `--master-key-file`, for
  example from Docker secrets mounted under `/run/secrets/`.
- The recent-span index takes 10% of the machine's memory, at most 1 GiB, by default. The server reads the host's
  total memory, not a container limit, so a container with a memory limit should pass `--recent-max-bytes` below it.

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

`GET /healthz` answers `ok` while the process runs. `GET /readyz` answers `ready` once the WAL accepts writes and the
startup checks against ClickHouse have passed, and 503 with the reason before that; ingest answers 503 for the same
time. The checks retry with a pause that grows from 1 s to 15 s, so Crucible may start before ClickHouse does.
`docker compose logs crucible` shows the same reason.

## Upgrades

A development preview has no upgrade path: when the data format changes, start over with
`docker compose down -v`. Once releases exist, an upgrade will be:

1. Set `CRUCIBLE_IMAGE` in `.env` to the new version tag.
2. Run `docker compose pull` and `docker compose up -d --wait`. `--migrate` applies the schema steps the new version
   adds.

Keep `CRUCIBLE_MASTER_KEY` across upgrades. Crucible creates its ClickHouse service users once, with passwords
derived from that key, so a server started with another key cannot log in to the same ClickHouse data.

## Backups

A backup is three things:

1. `.env` and `keys.toml`. Without the master key, restored ClickHouse data cannot be used.
2. The volume `clickhouse-data`: the telemetry.
3. The volume `crucible-data`: the WAL, which holds requests that were acknowledged but are not yet in ClickHouse.

Stop the stack first, so that the server drains its open requests and ClickHouse writes nothing while the volumes are
copied, then archive each volume. With the project name `crucible` from `compose.yaml`:

```sh
docker compose stop
for v in clickhouse-data crucible-data; do
  docker run --rm -v "crucible_$v:/data:ro" -v "$PWD:/backup" busybox tar -czf "/backup/$v.tar.gz" -C /data .
done
docker compose start
```

To restore, create the volumes on the new machine, unpack each archive into its volume the same way (with
`tar -xzf`), put `.env` and `keys.toml` back, and start the stack with the same ClickHouse version.
