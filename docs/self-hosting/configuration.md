# Configuration

Every setting of the `crucible` server, where it can be set, its default and what it does. The list is the one in
`crates/crucible-config/src/settings.rs`, the only code that reads the process environment.

## Where settings come from

A setting can be set in three places. Each overrides the one before it:

1. A TOML file, named by `--config` or `CRUCIBLE_CONFIG`.
2. An environment variable: `CRUCIBLE_` and the key in upper case, with `_` for `.`. `server.listen` is
   `CRUCIBLE_SERVER_LISTEN`.
3. A flag of `crucible serve`, where the setting has one.

A key's place in the file is its section and name: `server.listen` is `listen` under `[server]`. Top-level keys
come before the first section:

```toml
master_key_file = "/run/secrets/master"   # top-level keys come before the first section

[server]
listen = "0.0.0.0:8080"
ingest_listen = "0.0.0.0:4318"
grpc_listen = "0.0.0.0:4317"
metrics_listen = "127.0.0.1:9464"
public_url = "https://crucible.example.com"

[data]
dir = "/var/lib/crucible/data"

[clickhouse]
url = "http://clickhouse:8123"
password_file = "/run/secrets/clickhouse"

[postgres]
url = "postgres://crucible@postgres:5432/crucible"
password_file = "/run/secrets/postgres"

[smtp]
host = "mail.example.com"
from = "crucible@example.com"
```

Two commands read the configuration without starting anything. Both take the same flags as `serve`:

- `crucible config check` reports every problem at once and exits with 1 when there is one.
- `crucible config print` prints the effective configuration as TOML, each value with where it came from
  (`default`, `file`, `env CRUCIBLE_...` or `flag`), with secrets redacted.

```console
$ crucible config check --config crucible.toml
Error: the configuration has 1 problem:
  - crucible.toml: unknown key `server.lisen`; did you mean `server.listen`?
```

An unknown key in the file is an error that names the nearest known key. An unknown `CRUCIBLE_*` environment
variable is a warning, also with the nearest name, and is otherwise ignored.

### Secrets

The master key, the three passwords and the sign-in providers' client secrets are secrets. Each can be given as its
value, in an environment variable or the file, or as the path of a file that holds it, in its `_file` twin. No flag
takes a secret's value, so a secret never appears in a process list; the flags take the file. The file's content,
without a trailing line break, is the value. When both forms are given, the one from the later layer wins; in the
same layer the file wins and a warning says so.

| Secret | Value | File |
|---|---|---|
| The master key | `master_key`, `CRUCIBLE_MASTER_KEY` | `master_key_file`, `CRUCIBLE_MASTER_KEY_FILE`, `--master-key-file` |
| The ClickHouse admin password | `clickhouse.password`, `CRUCIBLE_CLICKHOUSE_PASSWORD` | `clickhouse.password_file`, `CRUCIBLE_CLICKHOUSE_PASSWORD_FILE`, `--clickhouse-password-file` |
| The Postgres password | `postgres.password`, `CRUCIBLE_POSTGRES_PASSWORD` | `postgres.password_file`, `CRUCIBLE_POSTGRES_PASSWORD_FILE`, `--postgres-password-file` |
| The mail server password | `smtp.password`, `CRUCIBLE_SMTP_PASSWORD` | `smtp.password_file`, `CRUCIBLE_SMTP_PASSWORD_FILE` |
| A sign-in provider's client secret | `auth_github.client_secret`, `CRUCIBLE_AUTH_GITHUB_CLIENT_SECRET` (and the same for `auth_google` and `auth_oidc`) | `auth_github.client_secret_file`, `CRUCIBLE_AUTH_GITHUB_CLIENT_SECRET_FILE` |

Files suit Docker and Kubernetes secrets mounted under `/run/secrets/`.

## The master key

`master_key` is 32 random bytes as 64 hexadecimal characters; `openssl rand -hex 32` makes one. A server with
ClickHouse or Postgres needs it. Crucible derives from it, with HKDF-SHA256:

- the passwords of its ClickHouse service users and of each project's SQL workspace user;
- the key that signs API and MCP cursors, so every server with the same master key opens the cursors of the others;
- the key that seals the secrets it keeps in Postgres (sign-in provider secrets, channel secrets);
- the key that finds an API key's row from its secret.

Keep it for the life of the deployment, and back it up with the data
([operations](operations.md#backups)). The ClickHouse service users are created once with passwords derived from
it, so a server started with another master key cannot read the same ClickHouse data, and the API keys and secrets
in Postgres stop working under another key.

## Settings

### Top level

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `master_key` | `CRUCIBLE_MASTER_KEY` | | none | The master key, 64 hexadecimal characters. Required with ClickHouse or Postgres |
| `master_key_file` | `CRUCIBLE_MASTER_KEY_FILE` | `--master-key-file` | none | A file that holds the master key |

### `[server]`

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `server.listen` | `CRUCIBLE_SERVER_LISTEN` | `--listen` | `0.0.0.0:8080` | The main listener: the UI, the REST API under `/api/v1`, the MCP server at `/mcp`, OTLP/HTTP ingest at `/v1/traces`, `/healthz` and `/readyz` |
| `server.ingest_listen` | `CRUCIBLE_SERVER_INGEST_LISTEN` | `--ingest-listen` | `0.0.0.0:4318` | The OTLP/HTTP ingest listener |
| `server.grpc_listen` | `CRUCIBLE_SERVER_GRPC_LISTEN` | `--grpc-listen` | `0.0.0.0:4317` | The OTLP/gRPC ingest listener (HTTP/2 without TLS) |
| `server.metrics_listen` | `CRUCIBLE_SERVER_METRICS_LISTEN` | `--metrics-listen` | `127.0.0.1:9464` | The Prometheus listener, `GET /metrics`. Loopback by default |
| `server.ui_dir` | `CRUCIBLE_SERVER_UI_DIR` | `--ui-dir` | none | Serve the UI from this directory (a built export such as `ui/out`) instead of the one compiled in. Read once at startup |
| `server.keys_file` | `CRUCIBLE_SERVER_KEYS_FILE` | `--keys-file` | none | A TOML file of project keys, for a server without Postgres ([without Postgres](README.md#without-postgres-keys-from-a-file)). Refused together with `postgres.url` |
| `server.public_url` | `CRUCIBLE_SERVER_PUBLIC_URL` | | none | The address people reach the server at, such as `https://crucible.example.com`. Sign-in through GitHub, Google or OpenID Connect needs it for the callback URL; an `https` address makes the session cookie `Secure`; cookie-authenticated writes must come from this origin. Without it, the origin of each request's `Host` is used |
| `server.allowed_origins` | `CRUCIBLE_SERVER_ALLOWED_ORIGINS` | | none | Comma-separated origins, besides the server's own, that may send cookie-authenticated writes |

### `[data]` and `[recent]`

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `data.dir` | `CRUCIBLE_DATA_DIR` | `--data-dir` | `data` | The directory that holds the `wal`, `quarantine` and `exports` directories. The image sets `/var/lib/crucible/data` |
| `data.wal_backend` | `CRUCIBLE_DATA_WAL_BACKEND` | `--wal-backend` | `auto` | The disk backend of the write-ahead log: `auto`, `portable` or `uring`. `uring` needs a binary built with the `wal-uring` feature on Linux; `auto` uses it when it is there and the kernel allows it, and `portable` otherwise. The image is built without it, so it runs `portable` and refuses `uring` |
| `recent.max_bytes` | `CRUCIBLE_RECENT_MAX_BYTES` | `--recent-max-bytes` | 10% of the memory limit, at most 1 GiB | The memory bound of the recent-span index, in bytes. The default is 10% of the container's memory limit (cgroup v2), or of the machine's memory without one, at most 1 GiB |

### `[cpu]`

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `cpu.permits` | `CRUCIBLE_CPU_PERMITS` | | 8 per CPU the process may use | How many blocking jobs run at once: a sixteenth each (at least one) for the shipper's normalizing and encoding of blocks, the rest for ingest's inflating, transcoding, scanning and compressing of requests. While ingest holds more requests than its share prepares at once, as in a burst, the shipper does its part on its own task instead. The CPUs counted are those of the process's affinity and its cgroup quota. A job beyond its share waits its turn without holding a thread, and 8 more threads are kept for disk reads, name lookups and password hashing |

### `[mcp]`, `[sql]` and `[metrics]`

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `mcp.calls_per_second` | `CRUCIBLE_MCP_CALLS_PER_SECOND` | | `50` | Tool calls one API key may make to the MCP server at `/mcp` per second, sustained. `0` turns the limit off |
| `mcp.burst` | `CRUCIBLE_MCP_BURST` | | `200` | Tool calls one key may make at once after a quiet spell |
| `sql.statements_per_hour` | `CRUCIBLE_SQL_STATEMENTS_PER_HOUR` | | `1800` | Statements each project's SQL workspace user may send to ClickHouse per hour. A run is three statements: two checks and the query. Applied to ClickHouse at every start ([SQL workspace](sql-workspace.md)) |
| `metrics.queries_per_project` | `CRUCIBLE_METRICS_QUERIES_PER_PROJECT` | | `4` | Metrics queries (`POST metrics/query`) one project runs at once on a server. A dashboard sends one per widget, so a dashboard of many widgets takes turns: the queries past the limit wait, oldest first, within their deadline (their time limit plus 2 seconds: 12 seconds for most widgets), and one that gets no turn by then answers 504 `query_timeout`. A query reads one ClickHouse statement at a time, so the default of 4 is a quarter of the 16 statements that reads made with API keys share on a server, and keeps one dashboard from sending ClickHouse all its queries together |

### `[clickhouse]`

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `clickhouse.url` | `CRUCIBLE_CLICKHOUSE_URL` | `--clickhouse-url` | none | The HTTP endpoint of ClickHouse, `http://host:8123` or `https://host:8443`. Without it the server only ingests: requests are made durable and wait in the write-ahead log, and the API answers 503 |
| `clickhouse.user` | `CRUCIBLE_CLICKHOUSE_USER` | `--clickhouse-user` | `crucible_admin` | The ClickHouse admin user you created. It runs the migrations and creates Crucible's service users, so it needs access management |
| `clickhouse.password` | `CRUCIBLE_CLICKHOUSE_PASSWORD` | | none | That user's password |
| `clickhouse.password_file` | `CRUCIBLE_CLICKHOUSE_PASSWORD_FILE` | `--clickhouse-password-file` | none | A file that holds it |
| `clickhouse.idle_timeout` | `CRUCIBLE_CLICKHOUSE_IDLE_TIMEOUT` | | 1 s below the server's `keep_alive_timeout` | Seconds an idle connection to ClickHouse is kept for the next statement. Set it below the idle timeout of a balancer between Crucible and ClickHouse that drops idle connections sooner than ClickHouse does |
| `clickhouse.cluster` | `CRUCIBLE_CLICKHOUSE_CLUSTER` | `--clickhouse-cluster` | none (one server) | The name, in the servers' `remote_servers`, of the replicated cluster that holds the replica `clickhouse.url` names: one shard with several replicas. Crucible then creates its tables and users on every replica and stores each insert on a majority of them ([clustered ClickHouse](clustered.md)). Letters, digits and `_`, not starting with a digit; it needs `clickhouse.url` |

### `[postgres]`

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `postgres.url` | `CRUCIBLE_POSTGRES_URL` | `--postgres-url` | none | The control plane, `postgres://user@host:5432/database`, Postgres 15 or newer; add `?sslmode=require` for TLS. A password in the URL works too. Without it the server has no sign-in and takes its keys from `server.keys_file` |
| `postgres.password` | `CRUCIBLE_POSTGRES_PASSWORD` | | none | The password, when the URL carries none |
| `postgres.password_file` | `CRUCIBLE_POSTGRES_PASSWORD_FILE` | `--postgres-password-file` | none | A file that holds it |
| `postgres.pool_size` | `CRUCIBLE_POSTGRES_POOL_SIZE` | | `8` | Connections of the main pool. A reserved pool of 2 connections for the key feed and leases is added |

### `[security]` and `[auth]`

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `security.require_https` | `CRUCIBLE_SECURITY_REQUIRE_HTTPS` | | `false` | Refuse sign-in unless `server.public_url` is an `https` address. Off by default, so sign-in works over plain HTTP on an internal network |
| `auth.pg_outage_mode` | `CRUCIBLE_AUTH_PG_OUTAGE_MODE` | | `fail_open` | API keys while Postgres is unreachable. `fail_open`: keys the server already knows keep working for ingest. `fail_closed`: every secret key is refused after 5 s without a successful poll of the key feed |
| `auth.fail_open_max` | `CRUCIBLE_AUTH_FAIL_OPEN_MAX` | | unlimited | Seconds after which a `fail_open` outage behaves as `fail_closed` |
| `auth.session_outage_grace` | `CRUCIBLE_AUTH_SESSION_OUTAGE_GRACE` | | `600` | Seconds that signed-in sessions keep working while Postgres is unreachable. `0` refuses them at once |

### `[auth_github]`, `[auth_google]` and `[auth_oidc]`

Sign-in providers declared in the configuration, at most one of each kind. They need Postgres. At every start the
server makes each provider's row in Postgres match its section: a new section adds the provider, a changed one
changes its row in place, and a section you remove turns its provider off. A new `auth_oidc.issuer` also unlinks the
accounts that signed in through the provider, because a person at one issuer is not the same person at another;
each person links again through their verified email at their next sign-in. The row keeps its id, so the callback URL
stays the same, and a provider you declare again comes back on with it. These providers are read-only under
**Settings > Sign-in** and in the API, and their client secrets stay in the configuration: Postgres never stores
them. Give every server of a deployment the same sections, because each one applies its own at start.

```toml
[auth_oidc]
issuer = "https://login.example.com"
client_id = "crucible"
client_secret_file = "/run/secrets/oidc"
display_name = "Continue with Example SSO"
allowed_domains = "example.com"
```

The sign-in page offers them once `server.public_url` is set, because their callback URLs are built from it: register
`<server.public_url>/api/v1/auth/oauth/<provider id>/callback` with the provider. **Settings > Sign-in** shows the
URL with the provider's id.

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `auth_github.client_id` | `CRUCIBLE_AUTH_GITHUB_CLIENT_ID` | | none | The client id of a GitHub OAuth app. Setting it adds GitHub sign-in |
| `auth_github.client_secret` | `CRUCIBLE_AUTH_GITHUB_CLIENT_SECRET` | | none | That app's client secret |
| `auth_github.client_secret_file` | `CRUCIBLE_AUTH_GITHUB_CLIENT_SECRET_FILE` | | none | A file that holds it |
| `auth_github.display_name` | `CRUCIBLE_AUTH_GITHUB_DISPLAY_NAME` | | `Continue with GitHub` | The text of the sign-in button |
| `auth_github.allowed_domains` | `CRUCIBLE_AUTH_GITHUB_ALLOWED_DOMAINS` | | any | Comma-separated email domains, such as `example.com`. Only a verified email at one of them signs in with GitHub, at every sign-in |
| `auth_google.client_id` | `CRUCIBLE_AUTH_GOOGLE_CLIENT_ID` | | none | The client id of a Google OAuth client. Setting it adds Google sign-in |
| `auth_google.client_secret` | `CRUCIBLE_AUTH_GOOGLE_CLIENT_SECRET` | | none | That client's secret |
| `auth_google.client_secret_file` | `CRUCIBLE_AUTH_GOOGLE_CLIENT_SECRET_FILE` | | none | A file that holds it |
| `auth_google.display_name` | `CRUCIBLE_AUTH_GOOGLE_DISPLAY_NAME` | | `Continue with Google` | The text of the sign-in button |
| `auth_google.allowed_domains` | `CRUCIBLE_AUTH_GOOGLE_ALLOWED_DOMAINS` | | any | Comma-separated email domains that may sign in with Google |
| `auth_oidc.issuer` | `CRUCIBLE_AUTH_OIDC_ISSUER` | | none | The issuer URL of an OpenID Connect provider. Crucible reads its settings from the issuer followed by `/.well-known/openid-configuration` |
| `auth_oidc.client_id` | `CRUCIBLE_AUTH_OIDC_CLIENT_ID` | | none | The client id Crucible has at that provider. Setting it adds its sign-in, and then the issuer is required |
| `auth_oidc.client_secret` | `CRUCIBLE_AUTH_OIDC_CLIENT_SECRET` | | none | The client secret Crucible has there |
| `auth_oidc.client_secret_file` | `CRUCIBLE_AUTH_OIDC_CLIENT_SECRET_FILE` | | none | A file that holds it |
| `auth_oidc.display_name` | `CRUCIBLE_AUTH_OIDC_DISPLAY_NAME` | | `Continue with single sign-on` | The text of the sign-in button |
| `auth_oidc.scopes` | `CRUCIBLE_AUTH_OIDC_SCOPES` | | none | Comma-separated scopes to ask for besides `openid`, `email` and `profile` |
| `auth_oidc.allowed_domains` | `CRUCIBLE_AUTH_OIDC_ALLOWED_DOMAINS` | | any | Comma-separated email domains that may sign in through the provider |

A section you remove while password sign-in is off and no other provider is offered stays on, because it is the last
way to sign in: the start leaves its row as it was and logs an error that names the section. The server cannot sign
anyone in through it, because its client secret was in that section, so put the section back and restart, or turn
password sign-in on with `crucible admin enable-password-login` (or under **Settings > Organization**) and restart,
which turns the row off ([troubleshooting](troubleshooting.md#signing-in)).

### `[egress]`

Outbound calls to webhook and Slack channels ([alerts and egress](alerts-and-egress.md)).

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `egress.mode` | `CRUCIBLE_EGRESS_MODE` | | `open` | `open` calls any `http` or `https` address, private networks included, except cloud metadata addresses, which are always refused. `strict` calls public `https` addresses only |
| `egress.allow_cidrs` | `CRUCIBLE_EGRESS_ALLOW_CIDRS` | | none | Comma-separated address ranges that `strict` lets through, over `https`, such as `10.20.0.0/16`. They never open a metadata address |

### `[smtp]`

The mail server of email channels. Without `smtp.host` and `smtp.from`, email channels cannot be created.

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `smtp.host` | `CRUCIBLE_SMTP_HOST` | | none | The mail server's host |
| `smtp.port` | `CRUCIBLE_SMTP_PORT` | | 587; 465 with `tls`; 25 with `none` | Its port |
| `smtp.tls` | `CRUCIBLE_SMTP_TLS` | | `starttls` | `starttls`, `tls`, or `none` for a relay on a network you trust |
| `smtp.username` | `CRUCIBLE_SMTP_USERNAME` | | none | The user name, when the mail server asks for one |
| `smtp.password` | `CRUCIBLE_SMTP_PASSWORD` | | none | The password |
| `smtp.password_file` | `CRUCIBLE_SMTP_PASSWORD_FILE` | | none | A file that holds it |
| `smtp.from` | `CRUCIBLE_SMTP_FROM` | | none | The sender address of notification email, such as `crucible@example.com` |

### `[object_store]` and `[media]`

Media on an S3-compatible object store ([media](media.md)). Media need Postgres too. Without
`object_store.endpoint`, media stay inline in payloads and the media routes answer `media_not_supported`.

| Key | Environment variable | Flag | Default | Meaning |
|---|---|---|---|---|
| `object_store.endpoint` | `CRUCIBLE_OBJECT_STORE_ENDPOINT` | | none | The endpoint the server reaches, such as `http://versitygw:7070`. Setting it turns media on |
| `object_store.public_endpoint` | `CRUCIBLE_OBJECT_STORE_PUBLIC_ENDPOINT` | | `object_store.endpoint` | The endpoint SDKs and browsers reach, which upload and download links name |
| `object_store.bucket` | `CRUCIBLE_OBJECT_STORE_BUCKET` | | none | The bucket, which must exist |
| `object_store.region` | `CRUCIBLE_OBJECT_STORE_REGION` | | `us-east-1` | The region the request signatures name |
| `object_store.access_key` | `CRUCIBLE_OBJECT_STORE_ACCESS_KEY` | | none | The access key id |
| `object_store.secret_key` | `CRUCIBLE_OBJECT_STORE_SECRET_KEY` | | none | The secret access key |
| `object_store.secret_key_file` | `CRUCIBLE_OBJECT_STORE_SECRET_KEY_FILE` | | none | A file that holds it |
| `object_store.url_style` | `CRUCIBLE_OBJECT_STORE_URL_STYLE` | | `path` | `path` (`host/bucket/key`) or `virtual_host` (`bucket.host/key`) |
| `object_store.prefix` | `CRUCIBLE_OBJECT_STORE_PREFIX` | | `media` | The key prefix of media objects |
| `media.max_bytes` | `CRUCIBLE_MEDIA_MAX_BYTES` | | `67108864` (64 MiB) | The largest media object, in bytes, at most 5 GiB |

## Docker Compose variables

The Compose files in `deploy/compose/` read these from `.env` beside them and pass them to the server:

| Variable | Default | Meaning |
|---|---|---|
| `CLICKHOUSE_PASSWORD` | none, required by `compose.yaml` and `compose.dev.yaml` | The password of `crucible_admin` in the bundled ClickHouse |
| `POSTGRES_PASSWORD` | none, required by `compose.yaml` | The password of `crucible` in the bundled Postgres |
| `CRUCIBLE_MASTER_KEY` | none, required | The master key |
| `CRUCIBLE_CLICKHOUSE_URL` | none, required by `compose.cloud.yaml` | Your ClickHouse, `https://<host>:8443` or `http://<host>:8123` |
| `CRUCIBLE_CLICKHOUSE_USER` | `crucible_admin` | The admin user in `compose.cloud.yaml` |
| `CRUCIBLE_CLICKHOUSE_PASSWORD` | none, required by `compose.cloud.yaml` | That user's password |
| `CRUCIBLE_POSTGRES_URL` | none, required by `compose.cloud.yaml` | Your Postgres, `postgres://<user>:<password>@<host>:5432/<database>`, with `?sslmode=require` for TLS |
| `CRUCIBLE_IMAGE` | `ghcr.io/elm-os/crucible:latest` | The Crucible image: a release tag such as `0.1` or `0.1.0`, or the tag of an image you built |
| `CRUCIBLE_BIND_ADDRESS` | `0.0.0.0` | The host address the ports are published on. `127.0.0.1` keeps them on this machine |
| `CRUCIBLE_HTTP_PORT` | `8080` | The host port of the main listener |
| `CRUCIBLE_OTLP_HTTP_PORT` | `4318` | The host port of OTLP/HTTP ingest |
| `MEDIA_ACCESS_KEY`, `MEDIA_SECRET_KEY` | none, required by `compose.media.yaml` | The credentials of the bundled S3 gateway ([media](media.md)) |
| `CRUCIBLE_MEDIA_PORT` | `7070` | The host port of the bundled S3 gateway |
| `CRUCIBLE_MEDIA_PUBLIC_URL` | `http://localhost:7070` | The gateway's address as SDKs and browsers reach it, which upload and download links name |

Any other server setting is one more line in the `environment` of the `crucible` service, for example:

```yaml
      CRUCIBLE_SERVER_PUBLIC_URL: https://crucible.example.com
```

A missing required variable stops Compose with the variable's name and where to set it.
