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
the main listener. Bodies are protobuf or JSON, optionally gzip-compressed, and a request is acknowledged only once
its WAL frame under `--data-dir` is durable. Requests authenticate with a project's secret key as
`Authorization: Bearer <key>`. Key management will move to the control plane; until then, list the keys in a TOML file
passed with `--keys-file` (without one, every ingest request answers 401):

```toml
[[project_key]]
project_id = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
secret = "csk_..."
```

A secret is `csk_` followed by 43 base62 characters (`0-9A-Za-z`) that encode 32 random bytes. One way to make one:

```sh
python3 -c "import secrets, string as s; a = s.digits + s.ascii_uppercase + s.ascii_lowercase; print('csk_0' + ''.join(secrets.choice(a) for _ in range(42)))"
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

Requests authenticate with a project's secret key from `--keys-file` as `Authorization: Bearer <key>`. Until a
telemetry store is configured, `crucible serve` checks the key, the project and the parameters of each request and
then answers 503 with the problem code `telemetry_store_unavailable`. The UI sends no credentials yet: browser
sessions will arrive with the control plane, so against a running server its API requests answer 401.

## Hooks

`pnpm install` at the repository root installs the git hooks (lefthook):

- `pre-commit` checks staged files against the naming rule;
- `commit-msg` rejects AI attribution and naming-rule violations in the message.

Run the checks by hand with `node tools/policy/policy.mjs files` and `node tools/policy/policy.mjs selftest`.

## Long paths on Windows

Some third-party repositories exceed the 260-character path limit. Keep vendor checkouts under a short root such as
`C:\src\`, and set `core.longpaths=true` on those clones.
