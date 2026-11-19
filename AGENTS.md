# Crucible

Crucible is an open-source, self-hosted observability and analytics platform for AI agents and MCP servers. It is
one Rust binary (OTLP ingest, REST API, MCP server, MCP proxy, embedded Next.js UI) backed by ClickHouse for
telemetry and Postgres for the control plane. Read `docs/architecture/overview.md` before changing anything
structural, and `docs/architecture/contracts.md` before touching records, the WAL, the API or the database schemas.

## Ground rules (enforced by hooks and CI)

### Naming rule
Crucible may name other products when it integrates with them, accepts their traffic, documents a migration, or
compares itself in a comparison page. No text anywhere (code, comments, docs, tests, commit messages, PR titles and
bodies, branch names) may say or imply that Crucible copied, cloned or remade another product or its features.

- Vendor names may appear only in `crates/crucible-compat/`, `docs/integrations/`, `docs/migration/`,
  `docs/comparison/`, `THIRD_PARTY_NOTICES` and `policy/`.
- Provenance of code covered by an MIT notice is stated only in `THIRD_PARTY_NOTICES`. Never add provenance comments
  elsewhere, and never paste comments or prose from any other project.
- Comparison pages cite only a vendor's own published numbers. Do not run or publish benchmarks of hosted services.
- The phrase rules live in `policy/naming-rules.json`. Check with `node tools/policy/policy.mjs files` (and
  `selftest`). A line that must quote a flagged phrase carries an inline `naming-ok: <reason>` waiver.

### No AI attribution
Never add AI co-author trailers, "Generated with" lines or similar attribution to commits, PRs, tags, releases, code
or docs. The `commit-msg` hook and CI reject them. Generator banners from build tools (for example ts-rs) are fine.

### Determinism
Product crates never touch ambient time, randomness, task spawning, the filesystem, the network or process
environment directly. They take an `Env` (crate `crucible-env`: `Clock`, `Rng`, `Spawner`, `Disk`, `Net`,
`Metrics`) and, where they need storage, `Stores` (`TelemetryStore` now; `ControlStore`, `SqlWorkspaceStore`,
`ObjectStore` and `Mailer` later). `clippy.toml` bans the ambient APIs; only `crucible-env-prod`, `crucible-sim`
(the simulated Env), the binary crate, `xtask` and benches may allow them. Use `DetMap`/`DetSet` (fixed-seed hashing), `BTreeMap` or `IndexMap`, never the
default-hasher `HashMap`. `tokio::select!` must be `biased`.

### Verification
Correctness comes from empirical and formal methods, not from review passes:
- deterministic simulation testing with seeded fault models and a virtual clock (`crates/crucible-sim`);
- swarm testing (each run flips a coin per workload feature, fault and config flag);
- a Quint model of the ingest protocol (`specs/ingest.qnt`) and Kani proofs of pure kernels;
- unit, property (proptest), fuzz (cargo-fuzz), conformance (real ClickHouse and Postgres), Playwright and SDK
  conformance tests;
- performance gates (`cargo xtask perf gate`) on hot-path crates.

Do not spawn adversarial-review or QA agents. A change is done when its `[step] -> verify` checks pass.

### Security with good UX

Crucible is self-hosted. Keep the protections that cost users nothing always on (project isolation, hashed secrets,
constant-time comparisons, parameterized SQL, durability before acknowledgement). Hardening that adds friction is an
operator switch, never the default: the defaults must work on an internal network over plain HTTP, and every refusal
says which setting changes it (contracts 1.7).

### Legal
Some product behaviour is implemented from an MIT-licensed project under the notice in `THIRD_PARTY_NOTICES`. The
vendored reference copy lives outside this repository and excludes every directory that project licenses separately.
Never read, copy or reproduce material from those separately licensed directories; the features they cover are
designed independently from public documentation.

## Repository layout

| Path | Contents |
|---|---|
| `crates/` | Rust workspace crates (see `docs/architecture/overview.md` for the crate map) |
| `ui/` | Next.js static-export UI, embedded in the binary |
| `sdk/{typescript,python,go,rust}` | SDKs (agent helpers + MCP server wrappers) |
| `conformance/` | Shared SDK and MCP conformance transcripts and scrub rules |
| `specs/` | Quint specs and the committed OpenAPI spec |
| `fuzz/`, `sim/`, `perf/` | Fuzz targets, simulation corpus, performance rig |
| `xtask/` | Repository automation (`cargo xtask ...`) |
| `deploy/` | Docker images, Compose files, dev container |
| `docs/` | All documentation (keep docs here, not at the root) |
| `policy/`, `tools/policy/` | Naming and attribution rules and their checker |
| `tasks/` | `todo.md` (plan and progress) and `lessons.md` (rules learned from corrections) |

## Commands

The Linux dev container is the canonical environment for the Rust workspace (Windows hosts have no MSVC linker
here, and simulation, Kani and fuzzing need Linux). Build it once with
`docker build -t crucible-dev:1 -f deploy/dev/Dockerfile deploy/dev`, then:

```sh
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry crucible-dev:1 cargo nextest run --workspace
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry crucible-dev:1 cargo clippy --workspace --all-targets -- -D warnings
docker run --rm -v "$PWD":/work -v crucible-target:/target -v crucible-cargo:/usr/local/cargo/registry crucible-dev:1 cargo fmt --all --check
```

- JavaScript and TypeScript: `pnpm` (root workspace covers `ui` and `sdk/typescript`).
- Python: `uv` inside `sdk/python`.
- Go: the `golang:1.27` image, or a local Go toolchain if present.
- Policy: `node tools/policy/policy.mjs files` and `node tools/policy/policy.mjs selftest`.

## Web UI

This is NOT the Next.js you know: version 16.4 has breaking changes. Read the relevant guide in
`node_modules/next/dist/docs/` before writing any Next.js code, and heed deprecation notices. The UI is a static
export (`output: 'export'`): no dynamic route segments, cookies, rewrites, proxy or server actions; entity ids
travel in search params.

- Every interactive behaviour uses UI SDK (`@ui-sdk/core`, `@ui-sdk/react`, exact version pinned). Load the
  `ui-sdk` skill before building components.
- Follow the `ux-guidelines` skill: every screen and independently fetched section designs its loading, success,
  error and empty states; every action gives feedback; no raw backend text reaches a user.
- Visual identity: paper-toned monochrome, light only, tokens in `ui/src/app/globals.css`; no semantic colour coding;
  accent hues only as chart data encoding; status shown by icon plus label.
- Lint with Oxlint, format with Biome. TypeScript 7.

## Writing style for docs and UI copy

- No em dashes.
- Describe Crucible as the released product it is: document what exists on main, in the present tense, and leave
  out what is not built. Never write "preview", "under development", "planned" or "coming soon". A caveat an
  operator needs, such as one about upgrades between versions, is stated as a plain operational fact.
- Plain, specific sentences; every claim checkable against the code or a measurement.

## Ways of working

Any instruction that references `CLAUDE.md` means this file.

- **Think before coding.** State assumptions. If something is unclear, write down the interpretation you chose.
- **Plan before building.** Non-trivial work goes into `tasks/todo.md` as `[Step] -> verify: [check]` items;
  mark items done as you go.
- **API docs first.** Before implementing against any API, read its current docs or source at the pinned version.
- **Simplicity first.** Minimum code that solves the problem; no speculative features or abstractions.
- **Surgical changes.** Touch only what the task needs; match existing style; remove what your change orphaned.
- **Demand elegance.** If a fix feels hacky, fix the root cause instead.
- **Own bugs end to end.** Chase failing tests and CI to resolution.
- **Verify before done.** Run the checks the task names; never mark a task complete without proof.
- **Learn from corrections.** Capture each correction as a rule in `tasks/lessons.md`.

## Code graph and vendor source

- The code graph lives at `graphify-out/graph.json` (gitignored). Rebuild with `graphify update .`; query with
  `graphify query "<question>" --graph graphify-out/graph.json`, `graphify path "A" "B"`, `graphify explain "X"`.
- Never run `graphify extract` without `--code-only`, `graphify cluster-only` or `graphify label` without
  `--no-label`, or any `graphify install` command.
- For third-party source use `npx -y opensrc@0.7.3 path <spec>`, pinned to a tag for monorepos.

## Git

- Conventional commits with a lowercase subject; one branch per change; squash-merge PRs on green CI.
- Never commit secrets, generated `target/` output or `node_modules/`.
