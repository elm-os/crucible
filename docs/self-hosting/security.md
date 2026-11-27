# Security

Crucible is self-hosted and single-organization. The protections that cost nothing are always on. The hardening
that adds friction is a setting you turn on, and the defaults work on an internal network over plain HTTP. Every
refusal says which setting changes it.

## Always on

- **Project isolation.** Every read and write is scoped to one project. The project comes only from the API key,
  or from a project the signed-in user may see; no request body, header or query parameter chooses it. A key used
  with another project's id gets 404, so it learns nothing about projects it cannot read.
- **SQL isolation in ClickHouse.** The SQL workspace runs each project's queries as that project's own ClickHouse
  user, behind row policies and a read-only settings profile, so isolation does not depend on Crucible's code
  ([SQL workspace](sql-workspace.md)).
- **Secrets at rest.** An API key's secret is stored as a salted SHA-256 hash and shown once. Session tokens are
  stored as hashes. Passwords are hashed with Argon2id. Sign-in provider secrets and channel secrets are sealed with
  a key derived from the master key. Keys and CSRF tokens are compared in constant time.
- **No secret on the command line.** No flag takes a secret's value; secrets come from the environment, the
  configuration file or files ([configuration](configuration.md#secrets)). `crucible config print` redacts them,
  and no refusal echoes a key.
- **Parameterized SQL.** Every telemetry read that Crucible renders binds its values as parameters.
- **Durability before acknowledgement.** Ingest acknowledges a request only after it is synced to disk.
- **Browser protections.** The session cookie is `HttpOnly` and `SameSite=Lax`. A cookie-authenticated request
  other than `GET` and `HEAD` must carry the session's CSRF token in `X-Crucible-CSRF` and come from the server's
  own origin. Responses carry a Content Security Policy (`default-src 'none'` on the API), `X-Content-Type-Options:
  nosniff`, `Referrer-Policy: same-origin` and same-origin opener and resource policies. Stored inputs and outputs
  are served as `text/plain`, so a browser never renders them.
- **Outbound calls.** Webhook and Slack deliveries never call a cloud metadata address, and never follow a
  redirect ([alerts and egress](alerts-and-egress.md)).
- **Metrics stay local.** The Prometheus listener binds to the loopback interface unless you change
  `server.metrics_listen`.

## Switches

| Setting | Default | What turning it on does |
|---|---|---|
| `server.public_url` set to an `https` address | not set | The session cookie becomes `__Host-crucible_session` with `Secure`, and cookie-authenticated writes must come from that origin |
| `security.require_https = true` | `false` | The server refuses to start unless `server.public_url` is an `https` address, so sign-in never happens over plain HTTP |
| `server.allowed_origins` | none | Lets other origins, such as a second hostname, send cookie-authenticated writes |
| `egress.mode = strict` | `open` | Channels may call public `https` addresses only; `egress.allow_cidrs` lets named ranges through |
| `auth.pg_outage_mode = fail_closed` | `fail_open` | During a Postgres outage, every secret key is refused after 5 s instead of the known keys working for ingest |
| `auth.fail_open_max` | unlimited | Caps how long a `fail_open` outage lasts before it behaves as `fail_closed` |
| `auth.session_outage_grace = 0` | `600` | During a Postgres outage, signed-in sessions are refused at once instead of for 10 minutes |
| `CRUCIBLE_BIND_ADDRESS=127.0.0.1` in Compose's `.env` | `0.0.0.0` | The ports are published on this machine only |

## Plain HTTP and TLS

The server speaks plain HTTP on every listener. Over plain HTTP, passwords, session cookies and API keys cross the
network unencrypted; the server logs one notice about it at startup. On a network you do not trust:

1. Publish the ports on `127.0.0.1` only, or put a TLS-terminating proxy in front of ports 8080, 4318 and 4317.
2. Set `server.public_url` to the proxy's `https` address.
3. Set `security.require_https = true`.

The UI's static pages hold no data, so anyone who reaches port 8080 can load them. Every page's data comes from the
API, which needs a session or an API key.

## API keys and roles

- A key has scopes: `ingest` (send telemetry), `read` (every read, exports and the MCP server), `write` (tags,
  flags, feedback and corrections), `delete` (deletion requests) and `sql` (SQL workspace runs). A new key has
  `ingest`, `read` and `write`, and a key made in **Settings**, **API keys** always has those three, so it can also
  send traces and write. Give each key only what it needs: an exporter needs `ingest` alone, and an agent that only
  reads, `read` ([below](#making-a-key-with-fewer-scopes)).
- A key belongs to one project and never manages keys, members, channels, alert rules or settings.
- With Postgres reachable, a revoked or changed key stops working on every server within 5 seconds, and on the
  server that revoked it at once.
- `crucible admin create-key --import` makes a key that may send spans older than the event-age limit, to import
  history.
- People have an organization role (`owner`, `admin`, `member`, `viewer`) and, per project, a project role
  (`admin`, `member`, `viewer`). [The authorization matrix](../architecture/authz-matrix.md) lists the permission
  that every route needs, and which role and key scope grant it.

### Making a key with fewer scopes

From the command line, `--scopes` takes a comma-separated list. An ingest-only key and a read-only key:

```sh
docker compose exec crucible crucible admin create-key --project default --scopes ingest --note exporter
docker compose exec crucible crucible admin create-key --project default --scopes read --note agent
```

Through the API, a project admin who is signed in sends `POST /api/v1/projects/{project_id}/api-keys` with the
scopes in the body, such as `{"note": "agent", "scopes": ["read"]}`, and the answer holds the secret key as
`secret_key` ([API](../api/README.md#authentication) shows how to sign in and send the CSRF token).

## The master key

The master key protects everything Crucible keeps in ClickHouse and Postgres
([configuration](configuration.md#the-master-key)). Give it to the server as a file (`--master-key-file`) or a
secret, keep it out of images and source control, and keep a copy with your backups: without it, restored data
cannot be read.
