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
- `crucible serve --ui-dir ui/out` serves a built export from a directory, which suits UI work.
- The `embed-ui` feature compiles `ui/out` into the binary:

```sh
pnpm --filter crucible-ui build                         # writes ui/out
cargo xtask ui compress                                 # writes .br and .gz variants beside the text files
cargo build --release -p crucible --features embed-ui
```

Run `cargo xtask ui compress` after every UI build, because the build replaces `ui/out`; without the variants the
server sends every file uncompressed. `embed-ui` fails to compile without `ui/out`, so it is off by default and CI
builds without it.

## Hooks

`pnpm install` at the repository root installs the git hooks (lefthook):

- `pre-commit` checks staged files against the naming rule;
- `commit-msg` rejects AI attribution and naming-rule violations in the message.

Run the checks by hand with `node tools/policy/policy.mjs files` and `node tools/policy/policy.mjs selftest`.

## Long paths on Windows

Some third-party repositories exceed the 260-character path limit. Keep vendor checkouts under a short root such as
`C:\src\`, and set `core.longpaths=true` on those clones.
