# Contributing to Crucible

Crucible is MIT licensed, and contributions are made under the same license. This page covers the setup, the checks
a change must pass, and how a pull request is merged. The ground rules every change follows are in
[AGENTS.md](AGENTS.md): the naming rule, no AI attribution, determinism in product crates, and verification by tests
rather than review. Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md), and vulnerabilities are
reported privately as [SECURITY.md](SECURITY.md) describes, never in a public issue. Questions and ideas go to
GitHub Discussions and bugs to issues, as [Getting help](docs/getting-help.md) describes.

## Setup

[docs/development.md](docs/development.md) has the requirements and the dev container. In short:

- Docker, for the Rust workspace and every database the tests use. The Linux dev container is the canonical
  environment for Rust: it pins the toolchain from `rust-toolchain.toml` and carries cargo-nextest and cargo-deny.
  A Windows host needs no MSVC toolchain, because every Rust command runs in the container.
- Node.js 24 and pnpm 11 for the UI, the TypeScript SDK and the policy checker; Python 3.12 and uv for the Python
  SDK; Go comes from the `golang:1.27.4` image if it is not installed.
- Git with `core.autocrlf=false` for this repository (`.gitattributes` keeps every text file LF). On Windows, also
  set `core.longpaths=true`.

Build the dev container once, then install the JavaScript tools and the git hooks from the repository root:

```sh
docker build -t crucible-dev:1 -f deploy/dev/Dockerfile deploy/dev
pnpm install --frozen-lockfile
```

`pnpm install` installs two hooks (lefthook): `pre-commit` checks the staged files against the naming rule, and
`commit-msg` refuses AI attribution and naming-rule violations in the message.

The commands below run from the repository root. Rust commands run in the dev container; on Windows PowerShell,
write `"${PWD}"` where they say `"$PWD"`.

## The checks

These are the commands the required CI jobs run. A change is ready when they pass.

### Rust (`linux` and `windows` jobs)

```sh
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo fmt --all --check
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo clippy --workspace --all-targets --locked -- -D warnings
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo nextest run --workspace --locked --no-tests=pass
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo deny check --hide-inclusion-graph
```

- Clippy runs with the determinism bans of `clippy.toml`: product crates never read ambient time, randomness, the
  filesystem, the network or the process environment, never spawn tasks directly and never use the default-hasher
  `HashMap`; they go through `crucible-env`. `unwrap`, `expect`, `panic!` and printing are refused outside tests.
- The `linux` job also refuses an `allow` or `expect` of the determinism lints outside the files on its allowlist,
  and checks that the release binary carries no test-only features.
- The `windows` job runs `cargo build --workspace --locked` on Windows. Linux-only dependencies must be gated by
  target.

### The ban self-test (`bans` job)

```sh
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 python3 tools/ban-selftest/check.py bans
```

It plants every banned call in a scratch crate and fails unless clippy reports each one.

### Generated files (`drift` job)

The OpenAPI document and the UI's TypeScript types are generated from the API's code and committed. After changing
a route or a DTO, regenerate both and commit the result:

```sh
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo run --locked --quiet --package crucible -- openapi > specs/openapi/crucible-v1.json
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo xtask gen-ts
```

The job fails when either differs from what is committed, or when a generated type uses `bigint`. After a change to
route permissions, `cargo xtask authz-matrix` regenerates
[docs/architecture/authz-matrix.md](docs/architecture/authz-matrix.md), which a test compares.

### Kani proofs (`kani` job)

The `kani` job runs `cargo kani -p <crate>` for every crate with a `kani::proof`, when a pull request changes Rust
code. Kani 0.68.0 is in the formal-methods image, built on top of the dev container:

```sh
docker build -t crucible-dev-formal:1 -f deploy/dev/formal.Dockerfile deploy/dev
docker run --rm -v "$PWD":/work -v crucible-target-kani:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev-formal:1 cargo kani -p crucible-wal-codec
```

### The UI (`ui` job)

```sh
pnpm format:check
pnpm lint
pnpm --filter crucible-ui run typecheck
pnpm --filter crucible-ui test
pnpm --filter crucible-ui build
```

Biome formats and Oxlint lints. The Playwright suite that covers every UI surface in its loading, success, error and
empty states is described in [docs/testing/ui-surfaces.md](docs/testing/ui-surfaces.md); it runs in CI when `ui/`
changes.

### The naming rule and attribution (`policy` job)

```sh
node tools/policy/policy.mjs files
node tools/policy/policy.mjs selftest
```

The `policy` job also checks every commit message in the pull request, its title, its body and its branch name. To
check a title and body before opening a pull request:

```sh
printf '%s\n\n%s\n' "$TITLE" "$BODY" | node tools/policy/policy.mjs text
```

## Deterministic simulation

The whole system runs in a seeded simulation with a virtual clock and injected disk, network, database and crash
faults ([docs/testing/methodology.md](docs/testing/methodology.md),
[docs/testing/system-simulation.md](docs/testing/system-simulation.md)). Every pull request runs the regression
corpus and 200 fresh swarm configurations in the `swarm` jobs; the nightly lanes run thousands more.

```sh
# The corpus, then 20 fresh configurations with one seed each:
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo xtask sim run --configs 20 --seeds 1

# One run again, with what the oracle found:
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry \
  crucible-dev:1 cargo xtask sim replay --seed 0 --config 0xaeca437a47 --tier realistic
```

A failing run prints its seed, configuration and tier, which is all `sim replay` needs: one seed fixes the task
schedule, the random streams and every fault. The regression corpus is `sim/corpus/seeds.jsonl`; `sim run --record`
appends every failing run to it, so a bug once found is replayed on every later pull request. `cargo xtask help`
lists every option.

## Conformance against real databases

The in-memory ClickHouse and Postgres models that the simulation uses may be stricter than the real servers, never
more permissive. The conformance suites run the same operations against the models and against real servers, and
CI runs them when the store, schema, plan or control-plane crates change: ClickHouse 26.3, 26.8 and the latest
release, and Postgres 15 and 18. The media store adapter has its own suite against an S3-compatible gateway,
`crucible-s3-conformance`, which the `s3-conformance` workflow runs
([docs/testing/media-simulation.md](docs/testing/media-simulation.md)).

```sh
docker network create crucible-conformance
docker run -d --rm --name crucible-conformance-ch --network crucible-conformance \
  -e CLICKHOUSE_USER=crucible_admin -e CLICKHOUSE_PASSWORD=conformance-admin -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 \
  clickhouse/clickhouse-server:26.8
docker run -d --rm --name crucible-conformance-pg --network crucible-conformance \
  -e POSTGRES_USER=crucible -e POSTGRES_PASSWORD=conformance -e POSTGRES_DB=crucible postgres:18

docker run --rm --network crucible-conformance -v "$PWD":/work -v crucible-target:/target \
  -v crucible-cargo:/usr/local/cargo/registry crucible-dev:1 \
  cargo run --locked -p crucible-conformance -- --url http://crucible-conformance-ch:8123 \
    --user crucible_admin --password conformance-admin --label 26.8
docker run --rm --network crucible-conformance -v "$PWD":/work -v crucible-target:/target \
  -v crucible-cargo:/usr/local/cargo/registry crucible-dev:1 \
  cargo run --locked -p crucible-conformance --bin crucible-pg-conformance -- \
    --url postgres://crucible:conformance@crucible-conformance-pg:5432/crucible --label 18

docker stop crucible-conformance-ch crucible-conformance-pg
docker network rm crucible-conformance
```

The SQL workspace's isolation suite and the end-to-end test of the binary against real ClickHouse and Postgres are
described in [docs/testing/sqliso.md](docs/testing/sqliso.md) and
[docs/development.md](docs/development.md#the-end-to-end-test).

## The SDKs

Each SDK has its own CI workflow, which runs when its directory changes, and all four share the conformance suite
in `conformance/` ([docs/testing/sdk-conformance.md](docs/testing/sdk-conformance.md)).

```sh
# TypeScript
pnpm --filter @elm-os/crucible run typecheck
pnpm --filter @elm-os/crucible run build
pnpm --filter @elm-os/crucible test

# Python, from sdk/python
uv sync --locked
uv run --locked ruff check .
uv run --locked ruff format --check .
uv run --locked mypy
uv run --locked pytest

# Go
docker run --rm -v "$PWD/sdk/go":/src -w /src golang:1.27.4 sh -c 'go vet ./... && go test -race ./...'

# Rust: sdk/rust is its own Cargo workspace
docker run --rm -v "$PWD":/work -w /work/sdk/rust -v crucible-target-sdk:/target \
  -v crucible-cargo:/usr/local/cargo/registry crucible-dev:1 cargo test --workspace --all-features --locked
```

## Commits and pull requests

- One branch per change, from `main`. Commits follow [Conventional Commits](https://www.conventionalcommits.org/)
  with a lowercase, imperative subject, such as `fix(shipper): probe a batch once after an unknown outcome`.
- The naming rule: Crucible may name another product where it integrates with it, accepts its traffic, documents
  a migration or compares itself, and vendor names appear only in `crates/crucible-compat/`, `docs/integrations/`,
  `docs/migration/`, `docs/comparison/`, `THIRD_PARTY_NOTICES` and `policy/`. No text anywhere may say or imply
  that Crucible copied, cloned or remade another product. The phrase rules are in `policy/naming-rules.json`.
- No AI attribution: no AI co-author trailers and no "Generated with" lines in commits, pull requests or files.
  The `commit-msg` hook and the `policy` job refuse them.
- A change to records, the WAL, the insert protocol, the schema or the API changes
  [docs/architecture/contracts.md](docs/architecture/contracts.md) first, in the same pull request
  ([section 15](docs/architecture/contracts.md#15-change-control)).
- Docs and UI copy describe what exists on main, in plain sentences without em dashes.

Open the pull request against `main` and say in its body what changed and how it was verified: the commands you
ran and what they showed. The required checks are `policy`, `linux`, `windows`, `ui`, `bans`, `kani` and `drift`.
The `swarm` simulation jobs run on every pull request, and the SDK, conformance, SQL isolation, end-to-end, image
and proxy workflows run when the files they cover change. Pull requests are squash-merged once every check passes.

## Releasing

A release is a version bump and an annotated tag on `main`; the tag builds the binaries, the installers, the image
and the SDK packages. [docs/releasing.md](docs/releasing.md) has the steps and the checks after the tag.
