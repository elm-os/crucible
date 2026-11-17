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

## Hooks

`pnpm install` at the repository root installs the git hooks (lefthook):

- `pre-commit` checks staged files against the naming rule;
- `commit-msg` rejects AI attribution and naming-rule violations in the message.

Run the checks by hand with `node tools/policy/policy.mjs files` and `node tools/policy/policy.mjs selftest`.

## Long paths on Windows

Some third-party repositories exceed the 260-character path limit. Keep vendor checkouts under a short root such as
`C:\src\`, and set `core.longpaths=true` on those clones.
