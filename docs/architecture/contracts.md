# Contracts v2

Contract version: **v2**. Status: frozen. This page is the single binding contract that implementers code against.
The map of the system is [`overview.md`](overview.md); the reasons for the load-bearing choices are in
[`decisions.md`](decisions.md); the invariants that check this contract are in
[`../testing/invariants.md`](../testing/invariants.md).

Language: "must" and "must not" are binding. "(default; revisit)" marks a value chosen here because no design fixed
it; it is binding until a contract change replaces it. "Pinned by conformance" marks a statement about ClickHouse or
Postgres behaviour that a named conformance row must establish before code relies on it. Features are described in
planned tense where they are not yet shipped.

Sections: 1 conventions, 2 env traits, 3 records, 4 WAL, 5 insert protocol, 6 ClickHouse schema, 7 ClickHouse
identities and access, 8 read side, 9 MCP, 10 compat dialect registry, 11 control plane, 12 API, 13 retention and
deletion, 14 media, 15 change control.

## 1. Conventions

### 1.1 Identifiers

| Kind | Internal form | Wire form | Rules |
|---|---|---|---|
| OTel `trace_id` | 16 bytes | 32 lowercase hex characters | All-zero is rejected at ingest (partial success) |
| OTel `span_id`, `parent_span_id` | 8 bytes | 16 lowercase hex characters | All-zero `span_id` is rejected; all-zero or absent parent means "no parent" |
| Crucible ids (org, project, user, API key, session row, dashboard, widget, alert rule, channel, deletion request, export job, fact, comment, media object, saved query, SQL run, request id, `wal_epoch`) | UUIDv7, 16 bytes | Canonical hyphenated lowercase string | Minted only through `Env::new_id()` (an explicit `IdGen` keeps the monotonic state; ids draw from the `Rng` fork `crucible-env/new_id`); never by a database default |
| Nil UUID (`00000000-0000-0000-0000-000000000000`) | 16 zero bytes | not exposed | Reserved for marker rows (section 5.5). It must never be a project id and `ProjectScope` must refuse it |
| API keys | see section 11.3 | `cpk_...`, `csk_...` | |

UUIDv7 layout: 48-bit Unix milliseconds from `Clock::wall_now`, version and variant bits, and the remaining 74 bits
from `Rng`. One generator instance must return strictly increasing ids: when the wall clock does not advance it
increments the previous value's random field by one (default; revisit).

Uppercase hex on input is accepted and lowered. Any other id form is `validation_failed`.

### 1.2 Time

| Topic | Rule |
|---|---|
| Internal type | `Timestamp` = `i64` microseconds since the Unix epoch, UTC. `Duration` fields are `u64` microseconds unless the name ends in another unit |
| Wire type | RFC 3339 UTC string with exactly 6 fractional digits and a `Z` suffix, for example `2026-11-17T09:30:00.000000Z`. Inputs with other offsets or precisions are accepted and normalized |
| Nanosecond inputs | Truncated toward zero to microseconds |
| Valid range | `0 <= t < 4102444800000000` (year 2100). Span times outside the range are clamped and the span is flagged `clock_skew` |
| Clock skew | A `start_time` more than 24 h ahead of `received_at` is accepted and flagged `clock_skew = 1` |
| Event time and arrival time | Partitioning, retention and list windows use `start_time` (facts: `target_time`). `received_at` is for freshness, cursors' `as_of` and deletion cut-offs only |
| Maximum event age | A span whose `start_time` is older than the project's retention horizon, or older than `ingest.max_event_age` (default 400 days), is rejected through OTLP partial success. An import key (section 11.3) bypasses the age check when `ingest.admin_import = true` |
| Time windows | `TimeWindow { from, to }` is half-open `[from, to)`, both required, `from < to` |

### 1.3 Wire rules

| Rule | Detail |
|---|---|
| No `bigint` | Generated TypeScript must not contain `bigint`. Every `i64`/`u64` DTO field uses a wire newtype in `crucible-api-types` that serializes as a JSON number and carries a ts-rs `number` override. CI fails on any `bigint` in `ui/src/api/generated/` |
| Safe integers | Counts, sizes, token totals and `duration_us` are JSON numbers and must stay at or below 2^53 - 1; the server saturates at that value and sets `saturated: true` on the enclosing object (default; revisit) |
| Cost | Stored as `Int64` nano-USD (section 3.4). On the wire a cost is a JSON number in USD named `*_usd`, computed as `nano / 1e9` rounded to the nearest IEEE 754 double. It is exact to 1 nano-USD for magnitudes below 2^53 nano-USD (about 9.0 million USD) |
| Ids and times | Strings, per 1.1 and 1.2 |
| Enums | Lowercase snake_case strings with the variant names in section 1.6 |
| Bytes | Hashes are lowercase hex strings. Payload bodies are returned as raw bytes with a content type, never base64 inside JSON |
| Unknown fields | Request bodies with unknown fields are rejected with `validation_failed`. Responses may gain fields in `/api/v1` (additive only) |
| One struct, three derives | Every DTO derives serde, the utoipa schema trait and the ts-rs trait from one struct in `crucible-api-types`. OTLP bodies, compat bodies and internal records are excluded from ts-rs |

### 1.4 ProjectScope

- `ProjectScope { project_id }` is the only value that authorizes reading or writing project data. The type lives in
  `crucible-records`, a leaf crate, so that `crucible-plan` and the `Env` store traits can name it without a
  dependency cycle. Its only constructor, `ProjectScope::mint`, is a disallowed method in `clippy.toml`, and the CI
  allow-attribute guard permits the allow in exactly one file, `crates/crucible-auth/src/scope.rs`. No other crate
  may build one. Tests outside `crucible-auth` obtain scopes from `crucible_auth::test_support` (feature
  `test-support`, which product builds never enable).
- Every function in `crucible-plan`, `crucible-query`, `crucible-recent`, `crucible-lifecycle`, `crucible-sqlws`,
  `crucible-media` and the export writers that touches project data must take a `ProjectScope`.
- `project_id` comes only from the authenticated credential or from a path id that the permission extractor has
  checked (section 11.6). No request body, header or query parameter may set it. Routing headers on compat paths are
  ignored for routing.
- Trace ids are client-chosen and may collide across projects, so every lookup by trace id is project-scoped.
- Cursors, export downloads, payload links, media links and cache keys are bound to the project id.
- Cross-project joins and aggregates do not exist in v0.
- The nil UUID is refused by the constructor.

### 1.5 Crate names

Crates are named `crucible-<area>` in kebab-case; the binary crate is `crucible`. Internal crates are
`publish = false`. `crucible-api-types` and the Rust SDK `crucible-sdk` (in `sdk/rust`) are published.

| Area | Crates |
|---|---|
| Environment | `crucible-env`, `crucible-env-prod` |
| Shared types | `crucible-records` (records, enums, ids, time, table ids, histogram and classifier kernels) |
| Ingest and WAL | `crucible-otlp`, `crucible-admission`, `crucible-ingest`, `crucible-wal-codec`, `crucible-wal` |
| Normalize and ship | `crucible-normalize`, `crucible-compat`, `crucible-schema`, `crucible-chfmt`, `crucible-shipper`, `crucible-store-clickhouse` |
| Read | `crucible-plan`, `crucible-recent`, `crucible-query`, `crucible-lifecycle` |
| Platform | `crucible-config`, `crucible-auth`, `crucible-control`, `crucible-control-pg`, `crucible-api`, `crucible-api-types`, `crucible-cursor`, `crucible-jobs`, `crucible-alerts`, `crucible-notify`, `crucible-egress`, `crucible-sqlws`, `crucible-media`, `crucible-ui-assets` |
| MCP | `crucible-proxy`, `crucible-mcp-server` |
| SDK | `crucible-sdk` and its conformance runner `crucible-sdk-conformance` (`publish = false`), in `sdk/rust`: their own Cargo workspace and lockfile, outside the root workspace |
| Verification | `crucible-sim`, `crucible-conformance`, `crucible-loadgen`, `crucible-fake-collector` |
| Binary | `crucible` |

Dependency rules: `crucible-records`, `crucible-wal-codec`, `crucible-otlp`, `crucible-admission`, `crucible-plan`,
`crucible-cursor`, `crucible-schema`, `crucible-chfmt` and `crucible-compat` are pure (no `Env`, no IO).
`clickhouse::Client` is allowed only in `crucible-store-clickhouse`; the Postgres client only in
`crucible-control-pg`. Names, attribute keys, paths and fixtures that belong to another product live only in
`crucible-compat`.

Internal dependency edges. A crate may depend only on the internal crates listed for it; a new edge is a contract
change (section 15). The graph is acyclic: pure crates never depend on `crucible-env`, and `crucible-env` depends
only on pure crates. Dev-dependencies on `crucible-auth` (for `test_support`) are allowed everywhere; the minting
path deals only in `crucible-records` types, so the dev-dependency cycle that cargo permits cannot mix types.

| Crate | Internal dependencies |
|---|---|
| `crucible-records`, `crucible-chfmt` | none |
| `crucible-wal-codec`, `crucible-otlp`, `crucible-admission`, `crucible-compat`, `crucible-schema`, `crucible-api-types` | `crucible-records` |
| `crucible-plan` | `crucible-records`, `crucible-schema` |
| `crucible-normalize` | `crucible-records`, `crucible-otlp`, `crucible-compat` |
| `crucible-env` | `crucible-records`, `crucible-plan`, `crucible-schema`, `crucible-chfmt`, `crucible-control` (the `ControlStore` trait lives in `crucible-control`; `Stores` holds it) |
| `crucible-env-prod` | `crucible-env`, `crucible-records` |
| `crucible-auth` | `crucible-records`, `crucible-env`, `crucible-control` (identity repositories) |
| `crucible-wal` | `crucible-env`, `crucible-records`, `crucible-wal-codec` |
| `crucible-recent` | `crucible-env`, `crucible-records`, `crucible-plan` |
| `crucible-query` | `crucible-env`, `crucible-records`, `crucible-plan`, `crucible-recent`, `crucible-schema` |
| `crucible-shipper` | `crucible-env`, `crucible-records`, `crucible-wal`, `crucible-wal-codec`, `crucible-schema`, `crucible-chfmt`, `crucible-normalize`, `crucible-plan` (to build `BatchProbe`), `crucible-recent` (the shipper feeds the recent index through one adapter shared by the binary and the simulation) |
| `crucible-store-clickhouse` | `crucible-env`, `crucible-records`, `crucible-schema`, `crucible-chfmt`, `crucible-plan` |
| `crucible-ingest` | `crucible-env`, `crucible-records`, `crucible-otlp`, `crucible-admission`, `crucible-wal`, `crucible-wal-codec`, `crucible-auth` |
| `crucible-ui-assets` | `crucible-env` |
| `crucible-api` | `crucible-env`, `crucible-records`, `crucible-plan`, `crucible-query`, `crucible-recent`, `crucible-auth`, `crucible-api-types`, `crucible-cursor`, `crucible-control` (identity and authorization), `crucible-sqlws` |
| `crucible-cursor` | `crucible-records` |
| `crucible-config` | none (the only crate that reads the process environment) |
| `crucible-control` | `crucible-records` (`crucible-env` depends on it for the `ControlStore` trait, so it never depends on `crucible-env`) |
| `crucible-control-pg` | `crucible-env`, `crucible-records`, `crucible-control` |
| `crucible-fake-collector` | `crucible-env`, `crucible-env-prod`, `crucible-records`, `crucible-otlp`, `crucible-normalize` (it runs every export through the real normalizer) |
| `crucible-conformance` | `crucible-env`, `crucible-env-prod`, `crucible-records`, `crucible-schema`, `crucible-chfmt`, `crucible-plan`, `crucible-store-clickhouse`, `crucible-auth`, `crucible-sim` (the ChModel and PgModel differentials), `crucible-control`, `crucible-control-pg` |
| `crucible-loadgen` | `crucible-env`, `crucible-env-prod`, `crucible-records` |
| `crucible-jobs` | `crucible-env`, `crucible-records`, `crucible-control` |
| `crucible-sqlws` | `crucible-env`, `crucible-records`, `crucible-schema`, `crucible-control` |
| `crucible-proxy` | `crucible-env`, `crucible-records` |
| `crucible-mcp-server` | `crucible-env`, `crucible-records`, `crucible-plan`, `crucible-query`, `crucible-schema`, `crucible-auth`, `crucible-cursor`, `crucible-api`, `crucible-api-types` |
| `crucible-sdk` | none (it depends on no internal crate) |
| `crucible-sdk-conformance` | `crucible-sdk` |
| `crucible-sim`, `crucible` (binary) | any crate above |
| `xtask` | `crucible-sim` (`cargo xtask sim`), `crucible-api-types` (`cargo xtask gen-ts`), `crucible-api` (`cargo xtask authz-matrix`) |

Dev-dependency edges are allowed where a test needs another crate's test support (for example `crucible-query` on
`crucible-recent` and `crucible-chfmt`, `crucible-recent` on `crucible-auth`). Test-only features (`test-support`,
`test-mutants`) only add constructors or hooks that stay inert unless a test harness configures them; release builds
of the binary never enable them. Crates of later milestones (control plane, proxy, MCP server and the rest of section
1.5's map) add their rows here before their first code lands.

### 1.6 Frozen enums

Numeric values are the stored `Enum8` values and the internal codec values. They must never be renumbered; new
variants append.

| Enum | Variants (value) |
|---|---|
| `SpanKind` | unspecified (0), internal (1), server (2), client (3), producer (4), consumer (5) |
| `StatusCode` | unset (0), ok (1), error (2) |
| `ObservationType` | span (0), generation (1), event (2), embedding (3), agent (4), tool (5), chain (6), retriever (7), evaluator (8), guardrail (9) |
| `Level` | debug (0), default (1), warning (2), error (3) |
| `IoShape` | raw (0), genai_messages (1), indexed_messages (2) |
| `Truncation` | none (0), cap_exceeded (1) |
| `CostSource` | none (0), provided (1), calculated (2) |
| `TracePublic` | unset (0), private (1), public (2) |
| `McpSide` | none (0), client (1), server (2) |
| `SourceFormat` | otel (0), crucible_sdk (1), crucible_proxy (2), compat (3) |
| `CaptureSource` | sdk (1), proxy_stdio (2), proxy_http (3), otlp (4) |
| `Era` | legacy (1), modern (2) |
| `Transport` | unknown (0), stdio (1), streamable_http (2), sse (3) |
| `McpTargetKind` | none (0), tool (1), prompt (2), resource (3) |
| `Direction` | client_to_server (1), server_to_client (2) |
| `ResultType` | none (0), complete (1), input_required (2), task (3) |
| `CancelSource` | none (0), notification (1), stream_close (2) |
| `McpStatus` | ok (0), tool_error (1), protocol_error (2), transport_error (3), cancelled (4) |
| `ErrorClass` | none (0), protocol (1), validation (2), not_found (3), tool_execution (4), auth (5), rate_limit (6), timeout (7), transport (8), cancelled (9), internal (10) |
| `ErrorSubclass` | none (0), auth (1), rate_limit (2), upstream (3), business (4) |
| `GroupKeyKind` | legacy_session (1), handle (2), trace (3), process (4) |
| `IdentitySource` | none (0), identify_hook (1), token_claims (2), token_verified (3) |
| `FactTargetKind` | trace (1), span (2), session (3) |
| `AuthorKind` | user (1), api_key (2), system (3) |
| `HeadKind` | root (1), provisional (2) |
| `ScoreDataType` | numeric (1), categorical (2), boolean (3), text (4) |
| `ScoreSource` | feedback (1), api (2), annotation (3), eval (4) |
| `TagOp` | add (1), remove (2) |
| `DeletionKind` | trace (1), user (2), session (3), project (4) |
| `DeletionState` | pending, logged, applied, verified, completed, failed (Postgres text) |

`SourceFormat` and `IoShape` carry no product names: a span mapped by a dialect other than 0 has
`source_format = compat` and the dialect id in `compat_dialect` (section 10).

### 1.7 Security posture

Crucible is self-hosted and single-organization. Protections that cost users nothing are always on: project isolation,
hashed secrets, constant-time comparisons, parameterized SQL, durability before acknowledgement. Hardening that adds
friction is an operator switch, never the default: the default works on an internal network over plain HTTP, and every
refusal says which setting changes it.

## 2. Env traits

Product crates take an `Env`, and the `Stores` they need, and never touch ambient time, randomness, task spawning,
the filesystem, the network or the process environment. Both are cloneable structs of `Arc<dyn Trait>` handles,
built once in `main` (production, from `crucible-env-prod` and the store adapters) or by the simulation harness
(`crucible-sim`). The same compiled product code runs in both. The only cargo features that change product
behaviour are `wal-uring` (an extra `Disk` backend) and `embed-ui` (the `AssetSource`).

Construction has two phases, because a store runs over the primitives:

1. The core `Env`: `Clock`, `Rng`, `Spawner`, `Disk`, `Net`, `Metrics` and the id generator.
2. `Stores`, each store built over that `Env`: it connects through `env.net`, takes deadlines from `env.clock` and
   mints query ids with `env.new_id()`. `Stores` holds `telemetry: Arc<dyn TelemetryStore>`; `ControlStore` and the
   later stores join it as their traits land. In simulation the stores are in-memory models over the same `Env`.

Components then receive the `Env` and the `Stores`. Stores are not fields of `Env`.

Retry, backoff, batching and the action taken on each error class sit above these traits, so that simulation
exercises them. A store only maps its backend's failures to the typed errors below (for ClickHouse, the code table
of section 5.6).

Signatures below are Rust-like sketches; `async` methods return futures that must be cancel-safe unless stated.

### 2.1 Clock

| Method | Semantics |
|---|---|
| `now() -> Instant` | Monotonic nanoseconds. Never decreases, including across wall-clock steps |
| `wall_now() -> Timestamp` | UTC microseconds. May step forward or backward |
| `sleep(d: Duration)`, `sleep_until(t: Instant)` | Async; wakes no earlier than requested |
| `timeout(d, fut) -> Result<T, Elapsed>` | The inner future is dropped on expiry |
| `interval(period, missed: Skip or Burst) -> Interval` | First tick after one period |

Simulation must model: a paused virtual clock that advances only when every task is idle, a fixed epoch, a per-host
wall-clock offset (skew up to 5 min in the realistic tier) and seeded wall-clock steps. The monotonic clock never
goes backwards.

### 2.2 Rng

| Method | Semantics |
|---|---|
| `next_u64() -> u64`, `fill_bytes(&mut [u8])` | Cryptographically strong stream (ChaCha20) |
| `fork(label: &str) -> Rng` | Child stream that is a function of the root seed and the label path only, never of how many values the parent has produced |

Production seeds the root from OS entropy. Simulation seeds it from the run seed; every subsystem and every fault
type forks its own labelled child, so adding one consumer never perturbs another's stream. Secrets (API keys,
session tokens, CSRF tokens, signing keys, nonces) must come from `Rng`.

### 2.3 Spawner

| Method | Semantics |
|---|---|
| `spawn(name, fut) -> JoinHandle<T>` | Named task in the host's shutdown scope |
| `spawn_blocking(name, f) -> JoinHandle<T>` | CPU-bound or blocking work (Argon2id, compression, encoding) |
| `JoinHandle::{join, abort}` | `join` returns `Err(Cancelled)` after abort or scope shutdown |
| `shutdown() -> ShutdownSignal` | Resolves when the scope is asked to stop |

Simulation must model: a single thread; blocking closures run inline; a simulated crash aborts every task in the
host's scope at an await point or crash barrier and drops the `App`. Product code must keep no state in statics, since
a crash cannot clear them.

### 2.4 Disk

Paths are relative to a named root (`wal`, `quarantine`, `exports`, `ui`).

| Method | Semantics |
|---|---|
| `create_dir_all(path)`, `list(dir) -> Vec<Entry { name, len }>` | `list` is sorted by name |
| `open(path, mode: Read or Write or CreateNew) -> File` | `CreateNew` fails if the entry exists |
| `preallocate(file, len)` | Reserves space; reads of unwritten ranges return zeros |
| `append(file, offset, bufs) -> Result<()>` | Vectored write at an explicit offset. On error an unknown prefix may have been written |
| `read_at(file, offset, len) -> Bytes` | Short only at end of file |
| `sync_data(file)` | Bytes written before the call are durable only after `Ok`. After an `Err` the file must be treated as failed: the caller must not retry the sync and assume durability |
| `truncate(file, len)` | Durable after the next `sync_data` |
| `sync_dir(dir)` | Create, rename and remove are durable only after this returns `Ok` |
| `rename(from, to)`, `remove(path)` | |
| `free_space(root) -> u64` | Bytes available |

Production backends: `portable` (one named writer thread per root, standard file APIs) and, with `wal-uring`, an
io_uring backend on Linux (single-issuer ring, O_DIRECT with aligned buffers, write then datasync). `wal.backend =
auto | portable | uring`; `auto` probes at startup and falls back to `portable` on ENOSYS, EPERM or a failed
O_DIRECT probe, emitting a metric with the reason.

`SimDisk` must model: a pending-versus-durable split per file; loss of the un-synced tail on crash; torn writes at
4,096-byte block granularity on crash; short writes; ENOSPC from a capacity budget; EIO on write, read and sync;
fsync latency from a seeded distribution; an fsync lie (reports success, loses the data at the next crash; stress
tier and labelled runs only); bit flips on read; and loss of a directory entry when `sync_dir` was not called.

### 2.5 Net

| Method | Semantics |
|---|---|
| `listen(addr) -> Listener`, `Listener::accept() -> (Stream, peer)` | TCP |
| `connect(addr, deadline) -> Stream` | TCP to a literal socket address; never resolves names |
| `resolve(host) -> Vec<IpAddr>` | DNS |
| `tls_connect(stream, server_name, roots) -> Stream`, `tls_accept(stream, config) -> Stream` | rustls in production |
| `http(profile) -> HttpClient` | Outbound HTTP/1.1 and HTTP/2 over the methods above; `send(request, deadline) -> Response` with streaming bodies; no automatic redirects |

Every outbound connection in product code (ClickHouse, Postgres, object storage, OAuth and OIDC, webhooks, SMTP
through `Mailer`) must go through `Net` or a trait in this section.

Simulation must model: loss, delay, reorder, connection reset mid-request and mid-response, partitions between named
hosts, slow-loris peers, half-open connections and scripted DNS answers (including an answer that changes between
two resolutions).

### 2.6 TelemetryStore

The only way product code reaches ClickHouse, apart from `SqlWorkspaceStore`. Argument types: `TableId`,
`PartitionId`, `BatchToken`, `ProjectId` and `Uuid` from `crucible-records`; `EncodedBlock` from `crucible-chfmt`;
`QueryPlan`, `BatchProbe` and `DeletePlan` from `crucible-plan`; `MigrationStep` from `crucible-schema`. The other
types are defined with the trait in `crucible-env`.

| Method | Identity (section 7) | Semantics |
|---|---|---|
| `insert_block(block: EncodedBlock, settings: InsertSettings) -> Result<(), InsertError>` | `crucible_ingest` | One Native block, section 5. The table and the partition are those of `settings.token()`, so they cannot disagree with the token. A dropped call is an unknown outcome |
| `execute(plan: QueryPlan) -> Result<RowStream, QueryError>` | by query class (section 8.4) | Typed plans only. Rows follow `QueryPlan::output()` (section 8.1) |
| `probe(probe: BatchProbe) -> Result<bool, StoreError>` | `crucible_ingest` | The existence probe of section 5.5: the one read without a project predicate; it returns only the boolean |
| `delete(plan: DeletePlan) -> Result<MutationId, StoreError>` | `crucible_maint` | Lightweight `DELETE ... IN PARTITION` with a typed predicate; always carries a project predicate or the marker predicate. The rows are hidden from reads on return |
| `drop_partition(table, partition) -> Result<(), StoreError>` | `crucible_maint` | Dropping a partition that does not exist succeeds |
| `apply_deleted_mask(table, partition) -> Result<MutationId, StoreError>` | `crucible_maint` | |
| `mutation_status(id: &MutationId) -> Result<MutationStatus, StoreError>` | `crucible_maint` | `Pending`, `Done` or `Failed { code, reason }`. A mutation the store no longer lists is `Done` |
| `partition_projects(table, partition) -> Result<Vec<ProjectId>, StoreError>` | `crucible_maint` | Distinct projects with visible rows in a partition, ascending, excluding the nil marker project. Markers never keep a partition alive: a partition drop removes them too |
| `masked_parts(table, partition) -> Result<u64, StoreError>` | `crucible_maint` | Parts that still carry a delete mask |
| `apply_migration(step: &MigrationStep) -> Result<(), StoreError>` | `crucible_admin` | Schema or access step; idempotent |
| `detect() -> Result<StoreFacts, StoreError>` | `crucible_maint` | Section 5.8 |
| `dedup_selftest() -> Result<(), StoreError>` | `crucible_ingest` | Section 5.8; anything but exactly one row is `StoreError::SelfTest` |
| `kill(query_id: Uuid) -> Result<(), StoreError>` | `crucible_maint` | `KILL QUERY` by id; killing a query that has ended succeeds |
| `health() -> StoreHealth` | `crucible_maint` | `Healthy` or `Unavailable { reason }`; never fails |

No method takes SQL text and returns rows, and no method reads across projects: rows leave the store only through
`execute`, whose plan carries a `ProjectScope`.

| Type | Definition |
|---|---|
| `InsertSettings` | Built by `InsertSettings::new(token: BatchToken, flavour: StoreFlavour)`. `pairs()` is the exact name and value list of section 5.4, in that order; `insert_quorum = auto` is present for the clustered flavour only |
| `InsertError` | One variant per class of section 5.6: `Retry { reason, message }` with `reason` one of `NotSent`, `TooManyQueries` (202), `TooManyParts` (252), `Memory` (241), `NoSpace` (243), `ReadOnly` (242); `UnknownOutcome { code?, message }`; `FatalConfig { code, message }`; `Poison { code, message }`. A failure that fits no class is `UnknownOutcome`, because probing is always safe. Two poison failures are identical when their codes are equal; the shipper counts them |
| `QueryError` | `Timeout`, `MemoryExceeded`, `TooBroad`, `Unavailable`, `Internal`, mapping one-to-one to the problem codes `query_timeout`, `query_memory_exceeded`, `query_too_broad`, `telemetry_store_unavailable`, `internal_error` (section 12.3) through `problem_code()` |
| `StoreError` | `Unavailable` (also a timeout; for a statement that changes data the outcome is unknown and the idempotent call is repeated), `Rejected { code, message }`, `SelfTest`, `Internal`. `problem_code()` gives `telemetry_store_unavailable` for the first and `internal_error` for the rest |
| `MutationId` | The table plus the store's own name for the mutation; opaque to callers |
| `StoreFacts` | `version` (major, minor; the minimum is 26.3), `flavour` (`SingleNode`, `Clustered`, `Cloud`), `engines` (the engine family, `MergeTree`, `Replicated` or `Shared`, of every Crucible table that exists), `missing_settings` (settings Crucible sends that the server does not know; empty when all are present), `text_search_functions`, `text_index`, and `ddl_initial_user_and_roles` (the server setting `distributed_ddl_use_initial_user_and_roles`; clustered flavour only) |
| `RowStream` | A boxed stream of `Result<RowBlock, QueryError>` that also exposes `query_id()`. Every block holds 1 to 65,536 rows; the result is the concatenation of the blocks and an empty result has no block; block boundaries carry no meaning and may differ between stores. The first error is terminal. Dropping the stream cancels the query (section 8.5) |

`ChModel` (simulation) must model:
- tables loaded from the production `TableDef`s in `crucible-schema`; parts per partition;
- single-partition insert enforcement (a block that spans partitions is rejected);
- a finite deduplication log per table with the configured count window and, for replicated variants, the age
  window; window expiry as a fault;
- the marker-row existence probe and the `batch_seq` probe, answered through `probe(BatchProbe)`;
- `SimpleAggregateFunction` merges for `min`, `max`, `sum`, `sumMap` and `groupUniqArrayArray`, applied on a seeded
  merge schedule, with reads that aggregate correctly whether or not a merge has happened;
- lightweight-delete masking (rows hidden at once), `APPLY DELETED MASK` and `DROP PARTITION`;
- a typed-plan interpreter that computes exact answers;
- outcome faults: timeout before apply, applied but ack lost, 5xx, too many parts, memory and concurrency rejects,
  minutes-long outage, and a poison batch.

The model may be stricter than the real system and must never be more permissive.

### 2.7 ControlStore

Repository-shaped access to Postgres. One repository per aggregate in section 11.1, each with plain async methods
(`get`, `list`, `insert`, `update_if_version`, `delete`), plus:

| Method | Semantics |
|---|---|
| `transaction(f) -> Result<T, TxError>` | Runs `f` in one repeatable-read transaction. `TxError` distinguishes `SerializationFailure` (retryable), `UniqueViolation(constraint)`, `ForeignKeyViolation(constraint)`, `CommitUnknown` (connection lost during commit), `Fenced` (a lease's fencing token was superseded), `Invalid` (the request broke a model rule) and `Unavailable` |
| `key_feed().poll(after_seq, overlap) -> Vec<KeyChange>` | Section 11.8 |
| `key_feed().snapshot() -> KeySnapshot` | Full reconcile |
| `leases().acquire(name, holder, ttl) -> Option<Lease { fencing_token, expires_at }>`, `renew(lease)`, `release(lease)` | Section 11.7. Expiry uses the database clock |
| `claims().claim(queue, limit, lease_for) -> Vec<Claim>`, `complete(claim)`, `fail(claim, retry_at)` | Row claims with skip-locked semantics, a `lease_until` and an attempt counter |
| `db_now() -> Timestamp` | Database clock |
| `migrations().{applied, apply}` | Forward-only ledger for Postgres and for ClickHouse schema and access steps |

Rules: ids and timestamps are produced in Rust from `Clock` and `Rng`, never by column defaults (lease expiry is
the one exception). Nothing may depend on session state (no LISTEN dependency, no session advisory locks), so
transaction-mode poolers work. Statement timeout and idle-in-transaction timeout are 10 s. Two pools: a main pool
and a reserved pool of 2 connections for the key feed, leases and session lookups.

`PgModel` (simulation) must model: snapshot transactions that can abort with a serialization failure; unique
constraints; commit-unknown on connection loss; pool exhaustion; failover (brief unavailability, then a
stale-free reconnect); lease rows; claims; key-feed sequence numbers that can commit out of order; and a database
clock with skew against node clocks.

Settled in code: the store sits in `Stores.control` as an `Option`, so `crucible serve` without Postgres keeps
working as an ingest and telemetry node. The pool is `deadpool` with a custom manager over `connect_raw` on `Net`
(`deadpool-postgres` spawns on the ambient runtime, which the `Env` rule forbids). Migrations are `refinery`,
forward-only, numbered `V<n>__<name>.sql` in `crates/crucible-control-pg/migrations`.

### 2.8 ChildProcess

Used only by `crucible-proxy`.

| Method | Semantics |
|---|---|
| `spawn(spec: { program, args, env, cwd, stderr: Inherit or Pipe }) -> Child` | Unix: the child gets a parent-death signal where the OS supports it. Windows: suspended start, Job Object with kill-on-close, resume; command resolution honours `PATHEXT` |
| `Child::{stdin, stdout, stderr}` | Byte streams |
| `Child::wait() -> ExitStatus`, `Child::kill()`, `Child::signal(sig)` | |

Simulation must model a scripted MCP server task with seeded chunk splitting, stalls, crashes and exit codes.

### 2.9 SqlWorkspaceStore

The one path that sends user-written SQL to ClickHouse (section 7.4). No method accepts a user name; the adapter
computes the identity from the scope.

| Method | Semantics |
|---|---|
| `validate(scope, sql, params) -> Result<Estimate, SqlError>` | `EXPLAIN AST` and `EXPLAIN ESTIMATE` as the project identity |
| `run(scope, sql, params, limits) -> Result<GridStream, SqlError>` | Sends the text verbatim as the whole statement, with `query_id`, output format and `param_*` values only |
| `export(scope, sql, params, format, limits) -> ByteStream` | CSV, JSON lines or Parquet |
| `cancel(scope, query_id)` | Drops the connection and issues `KILL QUERY` through `crucible_maint` |
| `provision(project_id, generation, retention) -> Result<()>` | Policy, then user, then grant (section 7.4). Runs as `crucible_admin` |
| `deprovision(project_id)` | Revoke, drop user, drop policies |
| `probe() -> ProbeReport` | Runtime self-probe (section 7.5) |

Simulation uses a scripted stub: rows, each error class, slow, hang and disconnect. Isolation itself is verified
only against real ClickHouse by the `sqliso` suite.

### 2.10 ObjectStore

| Method | Semantics |
|---|---|
| `presign_put(key, content_type, sha256, len, ttl) -> Url` | Content type and checksum are signed headers; uses the public endpoint |
| `presign_get(key, disposition, ttl) -> Url` | Uses the public endpoint |
| `head(key) -> Option<ObjectMeta { len, sha256?, created_at }>` | |
| `put_small(key, bytes)`, `get_stream(key) -> ByteStream` | Internal endpoint |
| `delete(key)`, `delete_batch(keys)`, `list(prefix, after) -> Page<ObjectMeta>` | |

`SimObjectStore` must model: uploads that never arrive, eventually consistent listing, throttling, and a stored
object whose hash or length differs from what was signed.

### 2.11 Mailer

`send(message: { to, subject, text, html?, message_id, date }) -> Result<(), MailError { transient: bool }>`.
`message_id` and `date` come from `Rng` and `Clock`. Simulation models an outbox with seeded transient and
permanent failures.

### 2.12 Metrics

`counter(name, labels).add(n)`, `gauge(name, labels).set(v)`, `histogram(name, labels).record(v)`. Names and label
keys are `&'static str` from one registry in `crucible-env`. Production renders Prometheus text on
`127.0.0.1:9464/metrics`; the recorder must not read the clock. Simulation uses a deterministic in-memory recorder
that invariants can read.

### 2.13 AssetSource

`get(path) -> Option<Asset { bytes, etag, content_type, encodings: { br?, gzip? } }>` and
`manifest() -> AssetManifest { routes, csp_hashes }`. Sources: embedded (`embed-ui`), a directory (`--ui-dir`,
through `Disk`), or empty. The serving code is identical for all three.

### 2.14 Settled in code

Decisions made while implementing the traits above; they bind every backend.

| Topic | Rule |
|---|---|
| Store connections (#76) | Startup detection reads the server's `keep_alive_timeout` (`system.server_settings`); the pool keeps an idle connection for that value minus min(1 s, half of it) (default; revisit), rebuilt once after detection; 2 s when it cannot be read; `clickhouse.idle_timeout` overrides it, for a balancer in front of the store. A read whose connection could not be opened is sent once more; inserts are unchanged. Metric `crucible_clickhouse_connections_opened_total`, counted at `Net::connect` |
| Memory budgets (#76) | At startup the node adds up its byte budgets (the recent index, the shipper and ingest) and warns above 90% (default; revisit) of the memory the process may use, naming each budget and the setting that changes it |
| Id state | `new_id` needs state for strict monotonicity, and statics are forbidden, so `Env` owns one `IdGen`; the free form is `new_id(&IdGen, &dyn Clock, &dyn Rng)`. Backward wall-clock steps and an exhausted random field keep ids increasing |
| `ChildProcess` | Not a field of `Env`: only the proxy uses it, and takes it as an argument. Linux delivers the parent-death signal through a re-exec trampoline (a hidden `crucible proxy exec-child`), so the workspace stays free of `unsafe`; macOS has no parent-death signal. The simulation of a scripted MCP server (seeded chunk splits, stalls, crashes) lives in `crucible-proxy`'s tests; exit codes are covered by the scripted and real-process tests, because `ProxyCore` takes no exit status |
| Join | `JoinHandle::join(&mut self)` is cancel-safe; a task that panics surfaces as `Cancelled`. HTTP server per-connection tasks are spawned by the server library outside the `Spawner` scope; the accept loops are inside it and graceful shutdown waits for open connections |
| File operations | Per-file operations (`append`, `read_at`, `sync_data`, `truncate`, `preallocate`) are methods on `File`. `OpenMode::Write` creates a missing file and never truncates |
| Unknown sync outcome | Dropping a `sync_data` future fails the file, because its outcome is unknown. Mutating disk operations run to completion even when their future is dropped; they are not cancel-safe |
| Platform durability | Linux uses `fdatasync` and directory `fsync`; macOS uses `F_FULLFSYNC`; on Windows `sync_data` is `FlushFileBuffers` and `sync_dir` is not a durability barrier. I1 (no acked span lost across a crash) is guaranteed on Linux and macOS; Windows is a development platform |
| Expired deadlines | `connect` with a deadline already past fails with `DeadlineElapsed` without connecting |
| Metric handles | Calls take a typed metric definition, constructible only in `crucible-env`'s registry, plus label values. `Gauge::set` takes `i64`; histograms use fixed integer buckets, so recorded values are deterministic |

## 3. Records

Records live in `crucible-records`. The normalizer (`crucible-normalize`, with dialect tables from
`crucible-compat`) is the only producer of `SpanRecord`, `McpOpRecord` and `McpToolInventoryRecord`.

### 3.1 SpanRecord

Row identity is `(project_id, trace_id, span_id)`. The record has two tiers with that key: the **core tier** (narrow
columns and previews; lists and aggregates) and the **payload tier** (full values; point reads and search).
String caps are in bytes of UTF-8; an over-long string is cut at the longest prefix that fits and ends on a
character boundary.

**Core tier**

| Group | Field | Type | Cap and semantics |
|---|---|---|---|
| Scope | `project_id` | UUID | From the credential only |
| OTel | `trace_id` | 16 bytes | |
| | `span_id` | 8 bytes | |
| | `parent_span_id` | 8 bytes, optional | Stored as all-zero when absent |
| | `is_root` | bool | No parent, or the dialect's root flag |
| | `is_app_root` | bool | Dialect application-root flag |
| | `kind` | `SpanKind` | |
| | `name` | string | 1,024 bytes |
| | `start_time`, `end_time` | Timestamp | `end_time` is clamped to at least `start_time` |
| | `duration_us` | u64 | `end_time - start_time` |
| | `completion_start_time` | Timestamp, optional | Time of first output token |
| | `status_code` | `StatusCode` | |
| | `status_message` | string | 8 KiB |
| Provenance | `service_name`, `service_version` | string | 256 bytes each; from the resource |
| | `scope_name`, `scope_version` | string | 256 bytes each |
| | `telemetry_sdk_name`, `telemetry_sdk_language`, `telemetry_sdk_version` | string | 128 bytes each |
| | `sdk_name`, `sdk_version` | string | 128 bytes each; from request headers or resource |
| Taxonomy | `observation_type` | `ObservationType` | Chosen by the priority rule in section 10.4; default `span` |
| | `level` | `Level` | Explicit level, else `error` when `status_code = error`, else `default` |
| | `environment` | string | 128 bytes; default `default` |
| | `release`, `version` | string | 256 bytes each |
| Trace-level | `trace_name`, `user_id`, `session_id` | string | 512 bytes each; rule in 3.3 |
| | `trace_tags` | list of string | 64 items, 256 bytes each |
| | `trace_public` | `TracePublic` | Ingest-time value; later edits are facts |
| Annotations | `tags` | list of string | 64 items, 256 bytes each |
| Model | `model_requested`, `model_response`, `provider` | string | 256 bytes each |
| | `finish_reasons` | list of string | 16 items, 64 bytes each |
| Usage and cost | `usage` | ordered map string to u64 | Calculated usage: normalized keys (`input`, `output`, `total`, `cache_read`, `cache_write`, `reasoning`, and other provided keys), computed only from provided usage. 32 keys |
| | `cost` | list of i64 nano-USD aligned with `usage` keys | 0 where a key has no price |
| | `input_tokens`, `output_tokens`, `total_tokens` | u64 | Scalars for aggregates |
| | `total_cost_nusd` | i64 nano-USD | Section 3.4 |
| | `cost_source` | `CostSource` | Provided wins over calculated |
| | `price_model_id`, `price_tier_id` | UUID, optional | The price row used |
| | `usage_aggregate` | bool | Set by normalizer stage B on a span with an observed descendant that carries usage or cost (or is itself marked), seen in the same request or earlier in the LSN stream (the usage descendant set of section 5.3). The span stores and shows its own usage; token and cost sums leave it out (the sums rule below) |
| Prompt | `prompt_name`, `prompt_version` | string | 256 and 64 bytes; reference only in v0 |
| Tools | `tool_names` | list of string | 128 items, 256 bytes each |
| | `tool_call_count` | u32 | |
| MCP link | `mcp_side` | `McpSide` | When not `none`, an `McpOpRecord` with this key exists |
| I/O summary | `io_shape` | `IoShape` | Tells exports how to re-emit |
| | `input_preview`, `output_preview` | string | 1,024 bytes on a character boundary |
| | `input_bytes`, `output_bytes` | u64 | Original sizes before any truncation |
| | `input_truncation`, `output_truncation` | `Truncation` | |
| | `input_sha256`, `output_sha256` | 32 bytes, optional | SHA-256 of the original bytes; set only when truncated |
| Source | `source_format` | `SourceFormat` | |
| | `compat_dialect` | u8 | Section 10 |
| | `normalizer_version` | u32 | |
| | `clock_skew` | bool | Section 1.2 |
| Ingestion | `wal_epoch` | UUID | |
| | `lsn` | u64 | LSN of the frame that carried the span |
| | `batch_seq` | u64 | Section 5.1 |
| | `received_at` | Timestamp | From the frame header |
| | `api_key_id` | UUID | |
| | `row_digest` | u64 | xxh3-64 of the normalized row excluding the ingestion group |
| | `event_bytes` | u32 | Size of the span in the canonical request, saturating |

**Payload tier**

| Field | Type | Cap and semantics |
|---|---|---|
| key | `project_id`, `trace_id`, `span_id`, `start_time` | Identical values to the core tier |
| `user_id`, `session_id` | string | Denormalized so deletes and retention never join |
| `input`, `output` | bytes (JSON or text) | Verbatim source structure; 8 MiB each |
| `system_instructions` | JSON | 8 MiB |
| `metadata` | JSON object | User metadata. Also flattened to `metadata_keys`, `metadata_values` (values as JSON text for non-strings) and `metadata_kv` (`key=value`, each item cut at 1,024 bytes); 256 keys |
| `trace_metadata` | JSON object | Rule in 3.3 |
| `attributes` | JSON object | Residual span attributes not consumed by mapping; 8 MiB |
| `resource_attributes` | JSON object | Residual resource attributes; 8 MiB |
| `events` | JSON list of `{time, name, attributes}` | 128 events |
| `links` | JSON list of `{trace_id, span_id, attributes}` | 32 links |
| `tool_definitions` | JSON | 8 MiB |
| `tool_calls` | JSON list of `{id, name, arguments}` | 8 MiB |
| `model_parameters` | JSON object | 64 KiB |
| `usage_provided` | ordered map string to u64 | Provided usage under normalized keys (section 10.5); never estimated |
| `cost_provided` | ordered map string to i64 nano-USD | Decimal USD inputs are converted per 3.4 |
| `trace_state` | string | 4 KiB |
| `truncated_fields` | list of string | Names of payload fields cut by either cap |
| ingestion | `wal_epoch`, `lsn`, `batch_seq`, `received_at` | |

**Payload caps and the truncation rule**

- Per-field cap: 8 MiB (`ingest.max_field_bytes`). Per-span cap: 16 MiB over the sum of stored payload-tier fields
  (`ingest.max_span_bytes`). Both are configurable per deployment; a project snapshot pins them in `NormCtx`.
- A span is never rejected for size. A field above the per-field cap is cut to the longest prefix at or below the
  cap that ends on a UTF-8 character boundary.
- If the span still exceeds the per-span cap, fields are cut further in this fixed order until the sum fits:
  `events`, `links`, `resource_attributes`, `attributes`, `tool_definitions`, `tool_calls`, `trace_metadata`,
  `metadata`, `system_instructions`, `output`, `input`. Each field in turn is cut by as much of the remaining excess
  as it holds (possibly to empty) before the next is touched.
- For `input` and `output`: the original size, `Truncation = cap_exceeded` and the SHA-256 of the original bytes are
  kept in the core tier. For every cut field its name is added to `truncated_fields`.
- A cut JSON field is stored as its raw text prefix and is no longer valid JSON. Readers must treat any field named
  in `truncated_fields` (or with `*_truncation = cap_exceeded`) as opaque text.
- Previews are cut from the original value before truncation, after media tokens are replaced (section 10.5).

**Other rules**

- There is no server-side token estimation. `usage` is computed only from provided usage.
- **Sums rule.** Trace, session, user and metrics token and cost sums count every span whose `usage_aggregate` is
  false, whatever its observation type. A span with `usage_aggregate = true` repeats usage that its descendants
  report, so the descendants, which carry the model used for pricing, are what is counted; a hand-instrumented span
  of any type that carries usage or a provided cost and has no usage-bearing descendant counts. The rule applies to
  token and cost sums only: span, error and other counts include every span, and every span still stores and shows
  its own usage and cost. Known limit: a usage-bearing descendant that arrives after its ancestor, or whose frame
  the WAL has collected (the descendant set holds only the retained prefix, section 5.3), leaves the ancestor
  counted as well (a double count); the I7 oracle models the same rule.
- A client retry that re-sends a span produces new LSNs for one span key. The normalizer's duplicate window (section
  5.3) drops a repeat whose `row_digest` equals the remembered one; a repeat with a different digest is kept. Trace
  detail de-duplicates by `span_id` in the Rust tier (greatest `lsn` wins). Aggregates accept the rare duplicate
  outside the window, which holds only what the retained prefix of the WAL holds (section 5.3): a repeat whose
  earlier sighting was collected is kept, in every table alike.

### 3.2 Normalizer interface

`normalize(frame, &NormCtx) -> NormalizedFrame` where `NormCtx` pins `normalizer_version`, `price_table_version`,
the project config snapshot (caps, retention, salt, capture settings) and the tombstone snapshot.

| Stage | Properties | Work |
|---|---|---|
| A | Pure per frame; parallel; no clock, randomness or IO | Decode, dialect mapping (section 10), observation type, I/O extraction, caps, cost, `row_digest` |
| B | Sequential in LSN order; deterministic given the LSN stream | Duplicate-window filter, first-seen set, tombstone drops, recent-index insert, append to per-(table, partition) column builders |

The same frame and the same `NormCtx` must give byte-identical records.

### 3.3 Trace-level attributes

Trace-level attributes are `trace_name`, `user_id`, `session_id`, `trace_tags`, `trace_metadata`, `trace_public`,
`environment`, `release` and `version`.

- They are **never backfilled**. A stored span row is never rewritten when a later span supplies a value.
- Stage A forward-fills within one request only: among spans of one `trace_id` in one request, a span with an empty
  value takes the value of the request's root span for that trace, else of the earliest span in wire order that has
  one.
- The trace-level value shown for a trace is a **rank-prefixed pick** over all of its spans: the root span's
  non-empty value, else the non-empty value with the greatest `(end_time, span_id)`.
- Encoding of the pick (so it is a mergeable byte-wise `max`): 17 rank bytes followed by the value. Rank bytes are
  `is_root` (1 byte, 0 or 1), `end_time` as 8 bytes big-endian (non-negative by 1.2), then `span_id` (8 bytes). A
  pick is emitted only when the value is non-empty. The reader strips the first 17 bytes.
- `trace_stats` carries picks for `trace_name`, `user_id` and `session_id`. Trace lists filter on head values
  (section 6.4) and display pick values. Span lists show per-span values.
- SDKs must set session and user ids at span start so that every span carries them.

### 3.4 Cost

- All costs are fixed-point `i64` nano-USD (1 USD = 1,000,000,000). Sums are exact integer sums.
- A provided decimal USD value is converted with round-half-to-even at 1 nano-USD and saturates at `i64::MAX`.
  Negative provided costs are clamped to 0.
- `cost_source = provided` when the client sent any cost; then `total_cost_nusd` is the provided total, else the sum
  of provided parts. Otherwise cost is calculated from the price table and `cost_source = calculated`; with no
  matching price row it is `none` and 0.
- Price table: versioned and immutable per `price_table_version`. Each model row has a match regex and a start
  date; each tier has a priority and conditions (numeric thresholds on usage keys, or set membership on model
  parameters). Selection: project override before bundled; greatest start date at or before the span's
  `start_time`; then the first tier by priority whose conditions hold. Per-key cost is
  `round_half_even(usage[key] * price_pusd_per_unit[key] / 1000)`: prices are stored in pico-USD per unit (`i64`),
  so fractional nano-USD prices are exact; the product is computed in 128-bit arithmetic and saturated to `i64`.
- The version used is recorded in `BatchSeal` (section 4.4), so a replay computes identical costs.

### 3.5 McpOpRecord

One row per MCP operation in both protocol eras, keyed `(project_id, trace_id, span_id)`: the key of its span. A
span yields an `McpOpRecord` if and only if it carries `mcp.method.name` (section 9.1). Agent cost and latency come
from spans only. MCP analytics come from MCP rows only and always filter on `side`; the two sides are never unioned
or summed.

| Group | Field | Type | Semantics |
|---|---|---|---|
| Key | `project_id`, `trace_id`, `span_id`, `parent_span_id` | | |
| Correlation | `transport_link_trace_id`, `transport_link_span_id` | 16 and 8 bytes, optional | First span link |
| | `server_name`, `server_version` | string, 256 and 128 bytes | |
| | `deploy_id` | string, 128 bytes | Section 9.7 |
| | `instance_id` | string, 128 bytes | |
| | `side` | `McpSide` (client or server) | |
| | `capture_source` | `CaptureSource` | |
| Protocol | `era` | `Era` | |
| | `protocol_version` | string, 32 bytes | |
| | `transport` | `Transport` | |
| | `method` | string, 128 bytes | |
| | `target_kind` | `McpTargetKind` | From the method |
| | `target_name` | string, 512 bytes | Tool or prompt name; resource URIs as templates |
| | `jsonrpc_id` | string, 128 bytes | String form |
| | `direction` | `Direction` | |
| | `is_notification`, `is_batch` | bool | |
| | `result_type` | `ResultType` | |
| | `mrtr_round` | u16 | 0 when not part of a multi-round exchange |
| | `mrtr_state_in`, `mrtr_state_out` | 16 bytes, optional | Section 9.6 |
| | `task_id`, `subscription_id` | string, 128 bytes | |
| Timing | `start_time` | Timestamp | |
| | `duration_us` | u64 | |
| | `progress_count` | u32, optional | Absent means the capture source could not observe progress |
| | `cancel_source` | `CancelSource` | |
| | `timed_out` | bool | |
| Outcome | `status` | `McpStatus` | Section 9.2 |
| | `error_class` | `ErrorClass` | |
| | `error_subclass` | `ErrorSubclass` | Set only by SDK annotation |
| | `jsonrpc_code` | i32, optional | |
| | `http_status` | u16, optional | |
| | `caller_fault` | bool | Section 9.2 |
| | `error_type` | string, 128 bytes | The semconv `error.type` string |
| | `error_message` | string, 2 KiB | Scrubbed in the SDK or proxy |
| Identity | `client_name`, `client_version` | string, 256 and 128 bytes | Always self-reported; the API labels them so |
| | `client_capabilities_hash` | u64 | |
| | `oauth_client_id` | string, 256 bytes | |
| | `user_subject_hash` | 16 bytes, optional | Section 9.3 |
| | `user_issuer` | string, 512 bytes | The token issuer that the hash was computed with; kept so delete-by-user can recompute hashes |
| | `user_id` | string, 512 bytes | Only from the `identify` hook |
| | `tenant` | string, 256 bytes | |
| | `identity_source` | `IdentitySource` | |
| | `session_key` | string, 32 hex characters, optional | Legacy era only; section 9.4 |
| | `handle_id` | string, 256 bytes | Section 9.8 |
| | `group_key`, `group_key_kind` | string, 256 bytes; `GroupKeyKind` | Section 9.4 |
| | `ip_prefix` | string, 64 bytes | /24 or /48 prefix, truncated by the sender |
| | `user_agent` | string, 512 bytes | |
| Sizes | `request_bytes`, `response_bytes` | u64 | Uncompressed JSON-RPC message; 0 when unknown |
| | `content_blocks` | ordered map string to u32 | Count per content type; 16 keys |
| Content (opt-in) | `content_captured` | bool | |
| | `content_sample_rate` | f32 | |
| | `content_truncation` | `Truncation` | |
| | `args`, `result` | JSON | Content tier (`mcp_content`); 16 KiB each by default in the SDK, hard cap 8 MiB at the normalizer |
| Ingestion | `wal_epoch`, `lsn`, `batch_seq`, `received_at`, `normalizer_version` | | |

Never stored, in any record or residual attribute: Authorization values, `requestState`, cookies, `Mcp-Param-*`
values, baggage, raw IP addresses and raw user subjects (the subject is hashed in stage B; section 9.3).

### 3.6 Tool inventory record

`McpToolInventoryRecord`, fed by `tools/list` and `server/discover` results and by the SDK's snapshot of registered
tools at start:

| Field | Type |
|---|---|
| `project_id` | UUID |
| `server_name` | string |
| `deploy_id` | string |
| `tool_name` | string, 512 bytes |
| `schema_hash` | u64 (xxh3-64 of the canonical JSON of the tool's input and output schemas) |
| `capture_source` | `CaptureSource` |
| `first_seen`, `last_seen` | Timestamp (the observing span's `start_time`) |

Unused tools are inventory entries with no calls in the window. Inventory rows are capped per frame (section
5.1).

### 3.7 Deploy record

`McpDeployRecord { project_id, server_name, deploy_id, server_version, capture_source, first_seen, last_seen }` is
emitted by stage B once per distinct `(server_name, deploy_id, server_version, capture_source)` per batch, from MCP
rows with `side = server`. It backs `mcp/deploys` and `mcp/compare` (section 9.9).

### 3.8 Facts

Facts resolve by one comparator, `crucible_records::fact_order`: `version` first, then `fact_id` in RFC 9562 byte
order (time order for UUIDv7). No SQL ordering of UUIDs ever picks a winner, because ClickHouse compares UUIDs by
their second half first: plans fetch candidate facts, and the Rust fold, `ChModel` and the differential check use
the same comparator. A fact write larger than one frame is split across frames; if it fails after some frames are
durable, a retry writes new facts that win, so writes are at-least-once with latest-wins resolution.

Facts are later-written, append-only, versioned rows. They are never updated or deleted in place; a retraction is a
new row. Facts written through the API go through the WAL (`FactBatch`) and are acknowledged after the fsync.

**Envelope (every fact)**

| Field | Type | Semantics |
|---|---|---|
| `project_id` | UUID | |
| `fact_id` | UUIDv7 | Server-generated |
| `target_kind` | `FactTargetKind` | |
| `trace_id`, `span_id`, `session_id` | per target kind | Unused ones are zero or empty |
| `target_time` | Timestamp | `start_time` of the target (trace: its earliest known start; session: its first seen); aligns pruning and retention with spans |
| `key` | string, 256 bytes | What latest-wins resolves over |
| `version` | u64 | Server microsecond timestamp at acceptance; ties broken by greater `fact_id` |
| `retracted` | bool | |
| `author_kind`, `author_id` | `AuthorKind`, UUID | |
| `created_at` | Timestamp | |
| `wal_epoch`, `lsn`, `batch_seq`, `received_at` | | |

**Bodies**

| Fact | Table | `key` | Body fields |
|---|---|---|---|
| Score (v0 exposes it as feedback) | `fact_scores` | score name, plus `#` and the client `score_id` when given | `data_type` `ScoreDataType`, `value_num` f64, `value_str` string 1 KiB, `comment` string 4 KiB, `source` `ScoreSource`, `metadata` JSON 16 KiB, `config_id` UUID optional |
| Tag edit | `fact_tags` | the tag | `op` `TagOp` |
| Flag edit | `fact_flags` | `bookmarked` or `public` | `value` bool |
| Correction | `fact_corrections` | `output` | `corrected` JSON 8 MiB, `note` string 4 KiB |
| Comment | Postgres `comments` | comment id | body, mentions, anchor path, `edited_from` (section 11.1) |

**Resolution**

- Latest-wins per `(target, key)`: the row with the greatest `(version, fact_id)`; if it is retracted the key has no
  value. Resolution happens in the Rust tier for the current page only. No query uses `FINAL`.
- Decoration: fetch the page of traces or spans, then one query per fact table restricted to the page's ids and
  time window.
- Filtering by a fact (bookmarked, tag, score threshold): resolve matching target ids in the window first, then
  semi-join into the list (section 8.1 `SemiJoin`).
- Effective tags = ingest tags, plus tags whose latest edit is `add`, minus tags whose latest edit is `remove`.

### 3.9 Trace head and trace stats rows

**Trace head** (`trace_heads`, plain rows; the root-anchored driver of the trace list):

| Field | Semantics |
|---|---|
| `project_id`, `trace_id`, `trace_hash` | |
| `head_kind` | `root`: emitted for every span with `is_root`. `provisional`: emitted by stage B the first time a trace is seen without a root (first-seen set, defined over the LSN stream so replay is stable) |
| `span_id` | The head span |
| `start_time`, `end_time` | Of the head span. Immutable; the list sort key |
| `name`, `level`, `status_code` | Of the head span |
| `user_id`, `session_id`, `tags`, `environment`, `release`, `version` | Head span values after the request-local forward-fill of 3.3 |
| `input_preview`, `output_preview` | Head span previews |
| ingestion | `wal_epoch`, `lsn`, `batch_seq`, `received_at` |

**Trace stats** (`trace_stats`, mergeable partial rows; any number per trace; also the trace-id to time lookup):

| Field | Merge | Semantics |
|---|---|---|
| `project_id`, `trace_id`, `row_day` | key | `row_day` is the UTC day of the contributing spans' `start_time`; also the partition anchor |
| `min_start`, `max_end` | min, max | Displayed trace start and end |
| `has_root` | max | 1 once a root span is seen |
| `min_head_start` | min | Earliest head `start_time` |
| `max_level` | max | |
| `span_count`, `error_count`, `generation_count`, `tool_count`, `mcp_count` | sum | |
| `total_cost_nusd`, `input_tokens`, `output_tokens`, `total_tokens` | sum | Under the sums rule of 3.1: every span with `usage_aggregate = false` contributes |
| `environments` | groupUniqArrayArray, at most 16 | |
| `trace_name_pick`, `user_id_pick`, `session_id_pick` | max | Rank-prefixed picks (3.3) |

Readers must aggregate `trace_stats` rows per trace at read time. A provisional head is shown only when the trace's
merged `has_root = 0` and the head's `start_time` equals `min_head_start`. Trace list order uses the head
`start_time`; the displayed start is `min_start`. Filters on per-trace aggregates (cost, error anywhere) are a
semi-join into `trace_stats` in class `analytics_raw`.

### 3.10 Rollup rows

All written by the shipper from the rows of one batch; none by ClickHouse. Each rollup row is a partial that merges
by its key (section 6). The cost and token sums of `session_rollup`, `user_rollup` and `span_metrics` follow the
sums rule of 3.1; their counts and histograms include every span.

| Set | Key | Measures | Freshness target |
|---|---|---|---|
| `session_rollup` | project, day, session_id | `trace_count` (incremented on root spans only), `span_count`, `error_count`, first and last seen, cost and token sums, `user_ids` (at most 8) | 10 s |
| `user_rollup` | project, day, user_id | `trace_count` (root spans only), `span_count`, `error_count`, cost and token sums, first and last seen. A user's distinct session count is computed at read time (default; revisit) | 60 s |
| `span_metrics` | project, minute, environment, observation_type, model_response, level, release | `count`, `error_count`, `dur_hist`, `ttft_hist`, cost and token sums. No user dimension | 2 s |
| `mcp_tool_rollup` | project, side, minute, server_name, deploy_id, capture_source, era, method, target_name, client_name, error_class, caller_fault | `count`, `dur_hist`, `semconv_hist`, request and response byte sums, `last_seen` | 5 s |
| `mcp_deploys` | project, server_name, deploy_id, server_version, capture_source | first and last seen | 5 s |
| `mcp_tool_inventory` | project, server_name, deploy_id, tool_name, schema_hash | first and last seen | 5 s |

### 3.11 Settled in code

| Topic | Rule |
|---|---|
| Derived columns | `duration_us`, `start_minute` and `trace_hash` are computed by methods from their inputs, never stored separately, so they cannot disagree |
| One copy per value | `SpanPayload` holds only payload-tier fields; the key, `user_id`, `session_id` and ingestion columns live once in `SpanCore` |
| Usage and cost | One aligned list of `{ key, units, cost }` entries |
| MCP content sizes | `McpOpRecord` carries `args_bytes` and `result_bytes`: sizes as received, before the content cap |
| Per-span cap | Sums the 13 body fields. A field with its own cap is cut at the smaller of that cap and the per-field cap and is listed in `truncated_fields` |
| Missing caps | `ip_prefix` 64 bytes; tool inventory `server_name` 256 bytes and `deploy_id` 128 bytes (default; revisit) |
| Explicit error class | An explicitly annotated class is kept only when the classifier table pairs it with the observed status; a subclass is kept only with `tool_execution` |
| Time parsing | RFC 3339 inputs: a leap second `:60` carries into the next minute; lowercase `t` and `z` are accepted; a space separator is rejected |
| Provided cost | A NaN provided cost becomes 0 |
| No-head sentinel | `NO_HEAD_START` is `2299-12-31T23:59:59.999999Z`, the largest `DateTime64(6)` value |

## 4. WAL

Codec: `crucible-wal-codec` (pure). Log: `crucible-wal` (on `Disk`). All integers are little-endian. One WAL
directory belongs to one node and has one `wal_epoch`.

### 4.1 Directory layout

| Entry | Contents |
|---|---|
| `EPOCH` | The `wal_epoch` (UUIDv7, 16 bytes) and a CRC32C. Created once with `CreateNew`, `sync_data`, `sync_dir`. A directory with segments and no valid `EPOCH` is a hard error |
| `seg-<segment_no, 20 decimal digits>.wal` | Segments, preallocated to 128 MiB (`wal.segment_bytes`) |
| `CHECKPOINT` | Advisory hint: last checkpoint `{batch_seq, last_lsn, segment_no}` and a CRC32C. WAL records are authoritative; a missing or invalid hint only lengthens recovery |

Quarantined data lives under the separate `quarantine` root (section 5.6).

### 4.2 Segment header (64 bytes)

| Offset | Field | Size | Notes |
|---|---|---|---|
| 0 | magic | 4 | ASCII `CWS1` |
| 4 | `format_version` | 2 | 1 |
| 6 | reserved | 2 | zero |
| 8 | `wal_epoch` | 16 | Must equal `EPOCH` |
| 24 | `segment_no` | 8 | Starts at 1, increases by 1 |
| 32 | `first_lsn` | 8 | LSN of the first frame written to this segment |
| 40 | `created_at` | 8 | i64 microseconds |
| 48 | reserved | 12 | zero |
| 60 | `header_crc32c` | 4 | Over bytes 0 to 59 |

Frames follow immediately, back to back with no alignment padding. Four zero bytes where a frame magic is expected
mean "end of written data in this segment".

### 4.3 Frame header (36 bytes)

| Offset | Field | Size | Notes |
|---|---|---|---|
| 0 | magic | 4 | ASCII `CWF1` |
| 4 | `header_version` | 1 | 1 |
| 5 | `record_type` | 1 | Section 4.4 |
| 6 | `flags` | 2 | bit 0: payload is compressed; bit 1: continuation (reserved, must be 0); all other bits must be 0 |
| 8 | `payload_len` | 4 | Stored payload bytes; at most `MAX_PAYLOAD` = 67,108,864 (64 MiB) |
| 12 | `lsn` | 8 | Starts at 1 in an epoch. Within the valid log (section 4.6) it increases by exactly 1 per frame, across segments. Every frame of every type consumes one LSN |
| 20 | `received_at` | 8 | i64 microseconds, `Clock::wall_now` read by the committer when it assigns the LSN |
| 28 | `payload_crc32c` | 4 | Over the stored payload bytes |
| 32 | `header_crc32c` | 4 | Over bytes 0 to 31 |

Rules:
- Compression (flag bit 0): the stored payload is a `u32` uncompressed length followed by one LZ4 block. The
  uncompressed length must be at most `MAX_PAYLOAD`. A writer must store the payload uncompressed when compression
  does not make it smaller. `wal.compression = lz4` (default) or `none`.
- An unknown `record_type`, a `header_version` above 1, or a reserved flag bit returns a typed error
  (`UnknownRecordType`, `UnsupportedVersion`, `ReservedFlag`), never a panic. Recovery treats these as fatal, not as
  a torn tail, when both CRCs are valid.
- A frame never spans segments. When a frame does not fit in the remaining space, the writer rotates.
- The request cap that follows: the canonical OTLP body of one request is at most 64 MiB minus a 64 KiB envelope
  reserve (67,043,328 bytes), so an `IngestRequest` payload always fits `MAX_PAYLOAD` uncompressed.

### 4.4 Record types and payload layouts

Payload codec: fixed-width little-endian integers; `uuid` is 16 bytes in RFC 9562 byte order; `bytes` is a `u32`
length then the bytes; `str` is `bytes` holding UTF-8; `list<T>` is a `u32` count then the items. Every payload
starts with `payload_version: u8 = 1`. Decoders must reject trailing bytes and unknown payload versions with a typed
error.

| Code | Type | Payload after `payload_version` |
|---|---|---|
| 1 | `IngestRequest` | Envelope, then `otlp: bytes` (the canonical OTLP protobuf `ExportTraceServiceRequest`; JSON bodies are transcoded before append). Envelope: `project_id: uuid`, `api_key_id: uuid`, `compat_dialect: u8` (request-level hint), `transport: u8` (1 HTTP, 2 gRPC), `content_origin: u8` (1 protobuf, 2 JSON), `flags: u8` (bit 0 admin import), `sdk_name: str` (128 bytes), `sdk_version: str` (64 bytes), `idempotency_key: bytes` (128 bytes; empty when absent), `span_count: u32`, `rejected: bytes` (bitmap over span ordinals in wire order; bit set means the span was rejected before the ack and must be skipped). The envelope, together with the payload version and the length prefix of `otlp`, is at most 64 KiB. It never holds secrets or Authorization values |
| 2 | `NativeBatch` | Reserved. No v0 writer emits it (the proxy ships OTLP). Decoders must accept the type code and skip the frame. Its layout will be frozen by a contract change before first use |
| 3 | `FactBatch` | `project_id: uuid`, `author_kind: u8`, `author_id: uuid`, `facts: list<Fact>` (at most 1,000 facts and at most 16 MiB of uncompressed payload per frame; the API splits a larger write into several frames and rejects a single fact above the cap with `payload_too_large`). `Fact`: `table: u8` (1 score, 2 tag, 3 flag, 4 correction), `fact_id: uuid`, `target_kind: u8`, `trace_id: [u8; 16]`, `span_id: [u8; 8]`, `session_id: str`, `target_time: i64`, `key: str`, `version: u64`, `retracted: u8`, `created_at: i64`, `body: bytes` (the body fields of section 3.8 in declaration order, in this codec) |
| 4 | `Tombstone` | `deletion_request_id: uuid`, `project_id: uuid`, `kind: u8` (`DeletionKind`), `requested_at: i64`, `deletion_seq: u64` (deployment-wide, from Postgres), `selector`: for trace `list<[u8; 16]>` (at most 10,000), for user or session `str`, for project nothing |
| 5 | `BatchSeal` | `batch_seq: u64` (equals `first_lsn`), `first_lsn: u64`, `last_lsn: u64`, `frame_count: u32`, `normalizer_version: u32`, `price_table_version: u32`, `deletion_seq: u64` (highest tombstone sequence applied while normalizing the range) |
| 6 | `Checkpoint` | `batch_seq: u64`, `last_lsn: u64` (the low watermark: every frame with an LSN at or below it is resolved), `table_bitmap: u64` (bit `table_id` set for every table; all ones for known tables in v0; kept for format stability) |
| 7 | `Quarantine` | `batch_seq: u64`, `first_lsn: u64`, `last_lsn: u64`, `pairs: list<{ table_id: u16, partition_id: str }>`, `error_code: i32`, `attempts: u16`, `quarantine_ref: str` (directory name under the quarantine root), `reason: str` (512 bytes) |
| 8 | `Noop` | Arbitrary bytes, ignored. Used for padding by backends that need aligned writes |

Code 0 is invalid. Codes above 8 are unknown.

### 4.5 Group commit, ack rule and durability modes

- Handlers prepare frames off the committer: build the payload, compress, compute `payload_crc32c`.
- One committer task per WAL: it assigns LSNs, fills `received_at` and `header_crc32c`, drains the queue up to 8 MiB
  per group (a larger single frame forms its own group), calls `Disk::append` once and `Disk::sync_data` once, then
  completes every waiter in the group.
- **Ack rule.** A request (OTLP export, fact write, tombstone) is acknowledged only after `sync_data` for the group
  that contains its frame returned `Ok`.
- Rotation: `sync_data` the old segment, create and preallocate the next, write its header, `sync_data`, then
  `sync_dir`.
- Append or sync failure: every waiter in the group fails (the client gets 503 and may retry); the committer
  rewinds its next LSN to the last durable LSN plus 1, abandons the segment at its last durable frame and rotates.
  The new segment's `first_lsn` is that next LSN. If rotation also fails, the WAL enters `failed`, readiness goes
  false and ingest answers 503 until restart.

| Mode (`wal.durability`) | Behaviour | Status |
|---|---|---|
| `group` | As above | Default. The only mode under which I1 is claimed |
| `interval(ms)` | Ack after append; sync on a timer | Unsafe; benchmarks only. Startup log, `/readyz` detail and the health DTO say `durability: unsafe` |
| `none` | Ack after append; never sync | Unsafe; benchmarks only; flagged the same way |

### 4.6 Recovery

Recovery must be idempotent (a crash during recovery followed by another recovery gives an identical state) and
bounded by the retained WAL size.

1. Read `EPOCH`. List segments; validate each header (magic, CRC, epoch) and that `segment_no` values are
   contiguous.
2. Read the `CHECKPOINT` hint to choose a starting segment; without a valid hint start at the oldest segment.
3. Scan frames forward. For each segment the valid prefix ends at the first position that is zero magic, a short
   read, a bad header CRC or a bad payload CRC.
   - In the last segment this is a **torn tail**: truncate (zero) the segment from that position, `sync_data`.
   - The next segment's `first_lsn` is authoritative. Frames in an earlier segment whose LSN is at or above the
     next segment's `first_lsn` are superseded (they are what a failed group left behind before the committer
     rotated, section 4.5) and must be ignored. An earlier segment is accepted when its valid prefix reaches at
     least the next segment's `first_lsn` minus 1.
   - An **LSN gap** is a hard error: an earlier segment whose valid prefix ends below the next segment's
     `first_lsn` minus 1, a missing `segment_no`, or an LSN inside a segment that is not its predecessor plus 1.
     The node refuses to start ingest and reports `wal_gap`; nothing is truncated.
4. The next LSN is the last valid LSN plus 1. From here the WAL accepts appends, so ingest readiness does not wait
   for the remaining steps.
5. Collect: the highest `Checkpoint` (the watermark), every `BatchSeal` whose `last_lsn` is above the watermark
   (sealed-but-unchecked batches), `Quarantine` frames, and tombstones. A seal stays unchecked until a `Checkpoint`
   covers it. A `Quarantine` frame resolves only the pairs it lists; every other pair of that batch is still
   probed and shipped in step 8.
6. Rebuild the duplicate window, the first-seen set and the usage descendant set by replaying the retained prefix
   in LSN order. The rebuilt state equals the state the previous process held, because stage B never remembers
   more than the retained prefix holds (section 5.3), so a re-normalized batch admits exactly the spans its first
   attempt admitted.
7. Rebuild the recent-span index from every frame above the watermark plus frames at or below it that are younger
   than the visibility grace (section 8.6).
8. Re-ship each sealed-but-unchecked batch with its recorded boundaries: re-normalize the range with the seal's
   `price_table_version`, probe every (table, partition) pair, insert the missing ones with identical tokens, then
   append the `Checkpoint`.
9. Normalize the unsealed tail and seal it with fresh boundaries.

### 4.7 Space, GC and replay retention

| Setting | Default | Rule |
|---|---|---|
| `wal.max_bytes` | 16 GiB (default; revisit) | Total segment bytes. Above 90% (`wal.unshipped` high-water), or free disk below `wal.min_free_bytes` (1 GiB), ingest answers 503 with `Retry-After` |
| `wal.replay_retention` | 15 min | Checkpointed segments are kept this long for replay after a normalizer fix. Hard cap 24 h. At most 25% of `wal.max_bytes`; evicted first under pressure |
| Visibility grace | 2 s single node, 10 s clustered or Cloud | GC must not remove a segment that the recent-span index still references |
| Quarantine retention | 24 h | Unless an operator pins a quarantine (a documented exception to the deletion SLA) |

A segment is removable when all of these hold: every frame in it has an LSN at or below the checkpoint watermark; it
is outside replay retention (by age, or by the byte budget); no recent-index entry references it; no frame above
the watermark relies on a sighting in it (`Admission::relies_on`, section 5.3); no batch that recovery found is
still waiting for its frames; it is not the current segment. Removal is `remove` then `sync_dir`. After each
collection stage B forgets every key last sighted below the first retained LSN. If a collection fails, a removal
may or may not be durable, so no frame is admitted until a later collection succeeds; ingest keeps acknowledging
into the WAL meanwhile. The frames an unchecked frame relies on stay on disk during a store outage, so the
high-water 503 can come sooner, by at most the replay retention budget. The WAL must never be truncated or removed above the
watermark, and never because an insert outcome is unknown.

Replay of retained, already checkpointed segments (operator command) consults the deletion requests in Postgres
and drops matching spans, so no raw copy outlives the deletion SLA.

### 4.8 Admission before the ack

`crucible-ingest` is the only pre-ack code. The scanner (`crucible-otlp`) validates the body once, without
allocation.

| Cap | Default |
|---|---|
| Canonical body per request | 67,043,328 bytes (section 4.3); gzip is inflated under this cap |
| Spans per request | 50,000 |
| `ByteBudget`s (each with a metric) | `ingest.inflight_bytes`, `wal.unshipped_bytes`, `normalize.cache_bytes`, `shipper.encoded_bytes`, `recent.bytes` |

| Condition | HTTP | gRPC |
|---|---|---|
| Accepted; some spans rejected (missing or zero id, too old) | 200 with `partial_success.rejected_spans` and a message | OK with partial success |
| Malformed body; over caps | 400; 413 | INVALID_ARGUMENT |
| In-flight byte budget or per-project token bucket exceeded | 429 with `Retry-After` 1 to 5 s (jitter from `Rng`) | RESOURCE_EXHAUSTED with RetryInfo |
| WAL over high-water, disk below floor, sync failing | 503 with `Retry-After` 5 to 30 s (scaled to the drain rate) | UNAVAILABLE with RetryInfo |
| Key unknown while Postgres is unreachable (section 11.8) | 503 with `Retry-After` | UNAVAILABLE |

No accepted request is ever dropped silently. Un-normalized data beyond `normalize.cache_bytes` lives only on disk
and is read back from the WAL in LSN order.

### 4.9 Kani obligations

Frame codec (`crucible-wal-codec`; no allocation in the header path, no unchecked arithmetic):

1. Decoding an arbitrary 36-byte array never panics.
2. Encode then decode is the identity for every valid header.
3. A decoded `payload_len` never exceeds `MAX_PAYLOAD`, and no offset or segment arithmetic overflows.
4. Any single-bit flip in a header fails the CRC check (for a bounded model of the checksum routine).
5. The frame iterator always advances or returns an error.

Other proved kernels, each owned by the named crate:

| Kernel | Crate | Property |
|---|---|---|
| Batch token | `crucible-records` | Injective over `(wal_epoch, first_lsn, last_lsn, table_id, partition_id)` |
| Partition routing and retention boundary | `crucible-schema` | No overflow over the valid time range; monotone in time |
| Histogram bucket function | `crucible-records` | Total; monotone; `lower(bucket(v)) <= v < lower(bucket(v)) + width(bucket(v))` |
| Backpressure accounting | `crucible-admission` | Reserve and release never underflow or exceed the cap |
| Cursor codec | `crucible-cursor` | Round-trip; no panic on arbitrary input |
| MCP error classifier | `crucible-records` | Total over (era, side, JSON-RPC code, `isError`, HTTP status, cancel source, timeout, transport failure, unhandled exception, annotations) |
| Rate limiter arithmetic | `crucible-auth` | No overflow |

Payload codecs and checksum strength over whole payloads are covered by property tests and simulation, not Kani.

### 4.10 Encoding details settled in code

| Topic | Rule |
|---|---|
| `EPOCH` | `wal_epoch` (16 bytes) then CRC32C (4 bytes); 20 bytes, no magic |
| `CHECKPOINT` | `batch_seq`, `last_lsn`, `segment_no` (each `u64`) then CRC32C; 28 bytes, no magic |
| Opaque payloads | `Noop` and `NativeBatch` payloads are opaque bytes with no `payload_version` byte; every other record type starts with `payload_version` |
| Optional UUID | A presence byte (0 or 1) followed by the UUID when present (for example a score's `config_id`) |
| Batch ranges | `BatchSeal` and `Quarantine` carry `first_lsn` and `last_lsn`; decoders reject `batch_seq != first_lsn` and `first_lsn > last_lsn` |
| Value domains | Unknown flag bits, enum values outside their frozen set and bool bytes other than 0 and 1 are `InvalidValue`; header version 0 is `UnsupportedVersion` |
| Bitmaps | The `rejected` bitmap is read least-significant bit first |
| Caps | Field and list caps are enforced on encode and on decode |
| Quarantine pairs | `table_id` is a `u16` that must name a known table; `partition_id` must parse as a partition id |
| Classification order | The header CRC is checked first, so every other header error implies a valid header CRC. `payload_len` above `MAX_PAYLOAD` with a valid header CRC is `Corrupt`. A non-zero wrong magic with both CRCs valid is `Corrupt`. Fewer than four remaining bytes that are all zero are a clean end; otherwise a short header is a torn tail |
| Recovery hardening | Each step is forced by the 2.4 fault model and proven necessary by a test that fails without it: the last segment is cut and re-extended at every recovery (stale frames can follow a clean end; readable zeros may not be durable); a torn tail or unreadable newest header is read twice before truncating or removing; a newest segment whose header was never completed and that holds nothing else is removed; `EPOCH`, the last segment's header and frames and the directory are rewritten and synced before appends resume (after a process stop, readable is not durable) |
| Failed group | A failed append or `sync_data` fails the whole group and rewinds the LSN; the segment is abandoned and the log rotates; the WAL is `failed` until restart only if rotation also fails. The failed handle is never synced again |
| Control records | `append` refuses a `BatchSeal` or `Checkpoint` whose range is above the durable LSN; recovery treats one as fatal |
| `CHECKPOINT` hint | Rewritten by WAL GC, off the committer, not when the checkpoint frame becomes durable; `segment_no` names the segment that holds the checkpoint frame |
| Kani obligation 4 | Discharged by four bounded proofs (every flip of a fixed header through the real encode and decode; every flip of the checksum field for any header; linearity of one CRC step; no single-bit difference cancels over 32 bytes) plus property tests on random headers and segments; the bitwise CRC model is tied to the production routine by a property test |

## 5. Insert protocol

Owner: `crucible-shipper` above `TelemetryStore`. Encoding: `crucible-chfmt`. There are no materialized views:
every table, base and derived, is written by this protocol.

### 5.1 Batch identity

- A batch is a contiguous LSN range `[first_lsn, last_lsn]`. Ranges of consecutive batches are adjacent: every LSN
  belongs to exactly one batch. Control frames inside a range contribute no rows.
- `batch_seq = first_lsn`. Batch identity is `(wal_epoch, first_lsn, last_lsn)`. It is computed only from WAL
  positions and is durable in a `BatchSeal` frame (synced) before any insert for the batch is sent.
- A batch seals when any of these is reached: 500 ms linger; 131,072 span rows; 128 MiB of raw frame bytes. Under
  pressure the limits widen by AIMD on insert latency up to 2 s and 262,144 rows. A batch must also seal before
  adding a frame would push any (table, partition) builder past 262,144 rows; frames are never split across
  batches.
- One frame can never exceed the block limit by itself: a request holds at most 50,000 spans, each span yields at
  most one row per table, a `FactBatch` holds at most 1,000 facts, and the normalizer caps tool-inventory rows at
  65,536 per frame (`MCP_INVENTORY_MAX_ROWS_PER_FRAME`; the first in wire order are kept and the excess is counted
  in a metric) (default; revisit).

### 5.2 Routing, tokens and blocks

- Each row routes to exactly one (table, partition). `partition_id` is the ClickHouse partition id string of the
  table's partition expression:

| Partition expression | `partition_id` |
|---|---|
| `toDate(t)` | `YYYYMMDD` |
| `toMonday(toDate(t))` (the `week` provisioning option, replacing `toDate`; `toMonday` of a `DateTime64` wraps in early 1970) | `YYYYMMDD` of that Monday; the days before the first Monday of 1970 route to `19700101` |
| `toYYYYMM(t)` | `YYYYMM` |
| none | `all` |

- Partition ids have four-digit years. Routing uses `crucible_schema::route` only (never a calendar helper, which
  differs in the first week of 1970).
- Routing reads the time column verbatim from the raw frame (UTC). It is frozen across normalizer versions:
  changing a routing rule requires a new `wal_epoch`. A row can therefore never move between pairs across
  attempts.
- **Token**: `{wal_epoch}:{first_lsn}-{last_lsn}:{table_id}:{partition_id}` where `wal_epoch` is the hyphenated
  lowercase UUID, the LSNs and `table_id` are unpadded decimals, for example
  `0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b:1048577-1051000:1:20261117`.
- **One block per pair**: for each (batch, table, partition) with at least one row the shipper sends exactly one
  INSERT that carries exactly one Native block of at most 262,144 rows (including a marker row), LZ4-compressed in
  ClickHouse-framed chunks of 1 MiB (`clickhouse.insert_compression = lz4 | zstd`). A pair lands wholesale from
  exactly one attempt, or not at all.
- Statement: `INSERT INTO crucible.<table> (<every column, in TableDef order>) FORMAT Native`. Type strings in the
  block header come from the `TableDef`. LowCardinality columns are sent as plain `String`.
- Rows in a block are sorted by the table's ORDER BY key, ties broken by `lsn` then span ordinal. Rollup rows with
  equal keys are combined by the shipper before encoding.
- The compressed bytes are kept in memory until the insert is confirmed, so a retry inside one process is
  byte-identical. After a restart the block is rebuilt from the WAL and may differ in bytes; the token, not the
  content, decides deduplication (conformance C4).

### 5.3 Replay determinism

| Input that could change between attempts | How it is pinned |
|---|---|
| Batch boundaries | `BatchSeal`, durable before any send |
| Normalizer version | Recorded in the seal. If the binary changed, the new version is used; safe because routing is frozen and pairs land wholesale |
| Price table | `price_table_version` in the seal records the table the first attempt used; versions are immutable. As with the normalizer version, a replay after a binary change prices with the current bundled table; pairs land wholesale, so costs never mix inside a pair. A batch whose pairs straddle a crash and an upgrade can carry two table versions across its tables, which I7 states as an exception |
| Duplicate window and first-seen set | Defined over the LSN stream (not wall time) and rebuilt from the retained prefix. Duplicate window: keyed by xxh3-64 of `(project_id, trace_id, span_id)`, value `row_digest`, bounded to 4,000,000 keys. Every sighting of a key, a dropped repeat included, moves it to the newest position; the key last sighted longest ago is evicted first, and every key last sighted below the first retained LSN is forgotten after each collection, so the live state always equals a rebuild from the retained prefix. An admission records the oldest earlier LSN its decisions used (`relies_on`), and the WAL keeps that frame until the deciding frame is checkpointed (section 4.7) |
| Usage descendant set | Per trace, the span ids that have a descendant carrying usage or cost (or already marked): every such span adds its parent id, and within one frame all spans are added before any is marked. Defined over the LSN stream and bounded to 4,000,000 entries. Each id carries a support: the LSN of the frame whose span carries the usage. A mark passing through later frames keeps that support, and an id with several marks keeps the highest. Like the duplicate window, the set is held to the retained prefix: it forgets ids whose support is below the first retained LSN after each collection, evicts the lowest support first (then the lowest key), and a kept span flagged on an earlier frame's support lowers `relies_on` to that LSN. So a descendant whose frame was collected no longer marks its ancestor (the known limit of 3.1) |
| Tombstones | A replay may drop more rows, never fewer. The deletion executor acts only after the node's checkpoint passes the tombstone (section 13.3) |
| Partition routing | Section 5.2 |
| Marker and ingestion columns | Computed from the seal and the frames (`received_at` of the batch's last frame for `marker_at`) |

Mixed-version attempts can therefore neither lose nor duplicate rows.

### 5.4 Per-insert settings

Always sent explicitly on every insert, never left to server or profile defaults:

| Setting | Value |
|---|---|
| `async_insert` | 0 |
| `insert_deduplicate` | 1 |
| `insert_deduplication_token` | the token |
| `max_insert_threads` | 1 |
| `max_partitions_per_insert_block` | 1 |
| `throw_on_max_partitions_per_insert_block` | 1 (a routing bug becomes a loud error) |
| `input_format_native_allow_types_conversion` | 1 |
| `max_execution_time` | 60 |
| `insert_quorum` | `auto`, clustered variant only |

Parallelism: at most 2 batches in flight per node; within a batch all pairs run concurrently on a pool of 8
connections. No order is required between tables.

### 5.5 Existence probe

The probe (`BatchProbe`, which is not a plan `Node`; `TelemetryStore::probe`, identity `crucible_ingest`) must run
before every retry whose previous outcome is unknown, and for every pair on recovery, requeue and drain. It is the
one read rendered without a project predicate; it returns only a boolean and can be built only inside
`crucible-shipper` (`crates/crucible-shipper/src/probe.rs`).

**Once in doubt, always probe.** After any attempt of a pair has had an unknown outcome, every later send of that
pair must be preceded by a probe that reads absent, even when a later attempt was definitively rejected: a
rejection after an unknown outcome returns the pair to unknown, because the earlier attempt may still land. The
ingest model found the counterexample where skipping this probe lands a block twice after a window expiry
(`docs/testing/formal-model.md`).

**Probe results expire.** A probe that reads absent authorizes a resend only for `PROBE_VALID_FOR` (default 60 s;
always well under half of the shortest deduplication window in section 6.2). Past it, probe again before sending.

| Table kind | Probe | Present when |
|---|---|---|
| Plain | `count()` restricted to `_partition_id = {partition}`, `wal_epoch = {epoch}`, `batch_seq = {batch_seq}`; pruned by the minmax skip index on `batch_seq` | count > 0 |
| Aggregating | Primary-key lookup of the block's **marker row**: `project_id` = nil UUID, `marker_epoch = {epoch}`, `marker_batch_seq = {batch_seq}`, within `_partition_id = {partition}` | count > 0 |

Marker rows: every block for an aggregating table carries exactly one marker row. Its `project_id` is the nil UUID,
its dimension columns hold type defaults, its time column holds the first instant of the partition, and
`marker_epoch`, `marker_batch_seq` and `marker_at` are set. `marker_epoch` and `marker_batch_seq` are the last
ORDER BY columns of every aggregating table (zero on data rows), so a marker never merges with another row. Markers
commit atomically with their block. They are invisible to every scoped read because the nil UUID can never be a
`ProjectScope`. `crucible-lifecycle` purges markers whose `marker_at` is older than 7 days, except markers of
batches named by an open quarantine.

The token covers the remaining race, where the probe reads "absent" while an earlier attempt is still committing.
That is why every table sets a deduplication window (section 6.2), and why the window must outlast both the longest
in-flight insert and the probe-to-resend gap.

Every service identity must hold a `USING 1` row policy before any workspace policy exists (section 7.3);
otherwise a probe could read "absent" for a landed block and re-insert it.

### 5.6 Error classes and quarantine

ClickHouse error codes are parsed from the response (`Code: N` and the exception-code header). The code table is
data in `crucible-store-clickhouse` and its stability across versions is pinned by conformance C12.

| Class | Signals | Action |
|---|---|---|
| Retry with backoff | Network error before the request was sent; 202 (too many queries); 252 with the too-many-parts message (also widens batches); 241 (memory; also halves concurrency); 243 (no space); 242 (read-only) | Same token, same bytes. Full jitter, 100 ms to 30 s, from `Clock` and `Rng` |
| Unknown outcome | Timeout, reset mid-request, 159, 319, 999, process crash | Probe; insert only if absent |
| Fatal configuration | 516, 497, 60, 16 | Stop the shipper for that table, raise an operator alert, keep the WAL. Ingest continues until `wal.unshipped_bytes` hits its cap |
| Poison data | 53, 27, 117, the Native-format data errors 33, 271, 432, 376 and 43, and similar data errors for one pair after 3 identical failures; 252 with the multi-partition message at once (a routing bug) | Quarantine |
| Read guardrails (SELECT only) | 158, 307, 396, 191, 159, 241 | Never retried; mapped to `query_too_broad`, `query_timeout`, `query_memory_exceeded` |

**Quarantine.** The frames of the batch are copied to `quarantine/<quarantine_ref>/` with a manifest (identity,
failing pairs, error), synced; a `Quarantine` frame is appended and synced; the failing pairs then count as
resolved for the watermark; an operator alert fires. Other pairs of the batch land normally. Data is never dropped.
`crucible wal requeue <quarantine_ref>` re-ships the named pairs with the original identity and tokens, probing
first. A quarantined batch counts as durable for invariant I1. A pair with an attempt in doubt may be quarantined
only after a probe reads absent; otherwise a quarantined pair could also be present.

**Frame quarantine.** A durable frame that the current binary cannot decode or normalize must not stop shipping.
The shipper copies the frame's bytes to `quarantine/frame-<wal_epoch>-<lsn>/` with a manifest (identity, decoder
or normalizer version, error), syncs it (`CreateNew`, so a repeat after a crash finds the copy and moves on), raises
an operator alert and a metric, and treats the frame as producing no rows. No WAL record is needed: the decision is
recomputed deterministically on replay with the same binary, and a newer binary that can normalize the frame ships
it normally. The frame then counts as durable for I1 through its quarantine copy, and `crucible wal requeue`
re-ingests it.

### 5.7 Checkpoint

`Checkpoint { batch_seq, last_lsn }` is appended only when that batch and every earlier batch are resolved: each of
their pairs is confirmed present (insert acknowledged, or probe present) or quarantined. It is a low watermark, so
checkpoints are monotonic. Segments wholly at or below `last_lsn` become eligible for GC (section 4.7). The
`CHECKPOINT` hint file is rewritten after the frame is durable.

### 5.8 Startup detection and self-test

Before a node accepts ingest it must, through `TelemetryStore::detect` and `dedup_selftest`:

- read `version()`, the flavour (single node, clustered, Cloud) and the engine family of each table after migration;
- confirm that every setting Crucible sends exists (`system.settings`, `system.merge_tree_settings`) and that the
  text-search functions exist;
- insert one block twice with one token into `dedup_selftest` and read back exactly one row;
- confirm that each service identity sees a known row through its row policy (the "service identities see all rows
  after access migrations" conformance row, also run at startup). This runs inside `detect`: a canary row is
  inserted as `crucible_ingest` and read back as every service identity, and a blind identity fails `detect` with
  `StoreError::SelfTest` naming it, which refuses ingest like a failed deduplication self-test.

Ingest is refused, loudly, when the version is below 26.3, the text index is unavailable, a required setting is
missing, or either self-test fails. All behaviour-affecting settings are sent per query, so a Cloud service's
`compatibility` value cannot change behaviour.

### 5.9 Crash barriers

A crash barrier is a labelled call that is a no-op in production and a forced crash point in simulation.

| Barrier label | Crash here leaves | Action on restart |
|---|---|---|
| before `wal.synced` | Frame not durable, not acked; tail torn or lost | Tail truncated; the client retries |
| `wal.synced` (before `ack.sent`) | Durable, not acked | Normalized and shipped; a client retry is absorbed by the duplicate window |
| `ack.sent` (before `seal.appended`) | Acked, unsealed tail | Normalize and seal with fresh boundaries; nothing was sent |
| `seal.appended` (before any `insert.sent`) | Sealed, unchecked, nothing landed | Probe (absent), insert |
| `insert.sent(table, partition)` / lost ack | Sealed, unchecked, some pairs landed | Probe each pair; insert the missing ones with identical tokens |
| `insert.confirmed` for all pairs (before `checkpoint.appended`) | Sealed, unchecked, all landed | Probes all present; append the checkpoint |
| `checkpoint.appended` (before `gc.removed`) | Checkpointed | GC resumes |
| `quarantine.copied` (before the `Quarantine` frame) | Copy exists, batch unresolved | Retry the insert; quarantine again if it still fails (the copy is overwritten) |
| any point during recovery | As above | Recovery is idempotent |

## 6. ClickHouse schema as data

Owner: `crucible-schema`. The schema is a set of typed `TableDef` values (columns, codecs, partition expression,
key lists, skip indexes, settings, deduplication window, retention binding, SQL-workspace column list).
`render(variant, options)` emits DDL for three variants. Golden files pin the rendered SQL, `ChModel` loads the
same `TableDef`s, and conformance compares normalized `SHOW CREATE TABLE` output with the descriptor on every
target.

**No materialized views, no projections and no table TTL exist in v0.** Rows are immutable. No query uses `FINAL`,
no insert is asynchronous and no lightweight update is used.

### 6.1 Conventions

| Topic | Rule |
|---|---|
| Databases | `crucible` (base tables) and `crucible_sql` (curated views for the SQL workspace) |
| First key column | `project_id UUID` on every table except `dedup_selftest` |
| Ids | `trace_id FixedString(16)`, `span_id FixedString(8)`, raw bytes. Absent ids are all-zero, not Nullable |
| Times | `DateTime64(6, 'UTC')` with `CODEC(Delta, ZSTD(1))`; written `DT64` below |
| Computed in Rust | `start_minute DateTime('UTC')` (start of the minute of `start_time`) and `trace_hash UInt32` (low 32 bits of xxh3-64 of `trace_id`) are real columns filled by the shipper |
| Enums | `Enum8` with the names and values of section 1.6 |
| Codecs | Default `ZSTD(1)`; payload strings `ZSTD(3)`; counters (`UInt32`, `UInt64`, `Int64` sums) `T64, ZSTD(1)` |
| `LowCardinality(String)` (written `LC`) | Only for bounded names: environment, provider, models, service and SDK names, scope name, method, server, tool and client names, usage keys, finish reasons. User-controlled high-cardinality strings (name, release, user, session) are plain `String` |
| Optional values | Empty string, zero or all-zero bytes mean "absent", except where a column is declared `Nullable` |
| Ingestion columns on every plain table | `wal_epoch UUID`, `batch_seq UInt64`, `lsn UInt64`, `received_at DT64` |
| Marker columns on every aggregating table | `marker_epoch UUID`, `marker_batch_seq UInt64` (last ORDER BY columns), `marker_at SimpleAggregateFunction(max, DateTime('UTC'))` |
| Aggregating tables | Every non-key column is a `SimpleAggregateFunction` (written `SAF(f, T)`), whose wire image is its underlying type, so rows are encodable from Rust. Every dimension is in ORDER BY (the floor version has no setting that allows dimensions outside the sorting key) |
| Histograms | `SAF(sumMap, Tuple(Array(UInt16), Array(UInt64)))`: bucket indexes and counts. The exact accepted type form is pinned by conformance (row "sumMap in SimpleAggregateFunction") |
| Bloom indexes | `bloom_filter(0.01)` (default; revisit) |
| Text indexes | `TYPE text(tokenizer = ..., preprocessor = lower(col))`, built on insert. Switching to build-at-merge is a config option only if a benchmark shows the need |

### 6.2 Engines and deduplication window per variant

| Variant | Plain tables | Aggregating tables | Deduplication settings in DDL |
|---|---|---|---|
| Single node | `MergeTree` | `AggregatingMergeTree` | `non_replicated_deduplication_window = 20000` (count only; there is no age window) |
| Clustered (1 shard, N replicas; no sharding in v0) | `ReplicatedMergeTree` | `ReplicatedAggregatingMergeTree`; every DDL `ON CLUSTER` | `replicated_deduplication_window = 20000`, `replicated_deduplication_window_seconds = 7200` |
| Cloud | written as `MergeTree` | written as `AggregatingMergeTree`; the service converts both to its shared engines; no `ON CLUSTER` | The replicated settings above. Whether the shared engines honour them is pinned by conformance (row "shared-engine dedup settings") and checked by the startup self-test |

Unknown outcomes are always probed (section 5.5), so the window only has to cover two races: an insert still in
flight when the probe runs, and the gap between a probe that reads absent and the resend (bounded by
`PROBE_VALID_FOR`). The count window of 20,000 inserts per table and the age window of 7,200 s are both far larger
than either bound at the insert rates in section 5.1. On clustered and Cloud targets every DDL sets `alter_sync` and
uses `wait_end_of_query`.

### 6.3 Tables

`table_id` is the stable number used in tokens and in the checkpoint bitmap. PRIMARY KEY is the bold prefix of
ORDER BY. Kind P is plain, A is aggregating.

| id | Table | Kind | PARTITION BY | ORDER BY (**PRIMARY KEY**) | Skip indexes | Table settings | Retention binding |
|---|---|---|---|---|---|---|---|
| 1 | `span_core` | P | `toDate(start_time)` | **project_id, start_minute, trace_hash**, start_time, span_id | bloom(trace_id) granularity 1; bloom(user_id), bloom(session_id) granularity 2; set(100) on observation_type and on level, granularity 4; text(name, `ngrams(3)`, lower); minmax(batch_seq); minmax(received_at) | `index_granularity = 8192` | `retention_days` |
| 2 | `span_payload` | P | `toDate(start_time)` | **project_id, start_minute, trace_hash**, span_id | text(input), text(output) with `splitByNonAlpha` and lower; text(metadata_kv) with `array`; bloom(trace_id) granularity 1; minmax(batch_seq) | `index_granularity = 1024`, `index_granularity_bytes = 16777216` (both to be benchmarked) | `payload_retention_days` |
| 3 | `mcp_ops` | P | `toDate(start_time)` | **project_id, side, start_minute, trace_hash**, span_id | bloom(trace_id), bloom(group_key), bloom(user_subject_hash), bloom(user_id); set(100) on error_class; minmax(batch_seq) | 8192 | `retention_days` |
| 4 | `mcp_content` | P | `toDate(start_time)` | **project_id, side, start_minute, trace_hash**, span_id | text(args), text(result) with `splitByNonAlpha` and lower; minmax(batch_seq) | as `span_payload` | `mcp_content_retention_days` |
| 5 | `mcp_tool_inventory` | A | none | **project_id, server_name, deploy_id, tool_name, schema_hash**, marker_epoch, marker_batch_seq | none | 8192 | `retention_days` on `last_seen` |
| 6 | `fact_scores` | P | `toYYYYMM(target_time)` | **project_id, key, target_day, trace_id**, span_id, session_id, version, fact_id | bloom(trace_id) granularity 1; bloom(session_id); minmax(batch_seq) | 8192 | `retention_days` on `target_time` |
| 7 | `fact_tags` | P | as 6 | as 6 | as 6 | 8192 | as 6 |
| 8 | `fact_flags` | P | as 6 | as 6 | as 6 | 8192 | as 6 |
| 9 | `fact_corrections` | P | as 6 | as 6 | as 6 | 8192 | `payload_retention_days` on `target_time` |
| 10 | `trace_heads` | P | `toDate(start_time)` | **project_id, start_time, trace_hash**, trace_id | bloom(trace_id), bloom(user_id), bloom(session_id); text(name, `ngrams(3)`, lower); text(tags, `array`); set(100) on environment; minmax(batch_seq); minmax(received_at) | 8192 | `retention_days` |
| 11 | `trace_stats` | A | `row_day` | **project_id, trace_id**, row_day, marker_epoch, marker_batch_seq | none | 8192 | `retention_days` on `row_day` |
| 12 | `session_rollup` | A | `toYYYYMM(day)` | **project_id, day, session_id**, marker_epoch, marker_batch_seq | bloom(session_id) | 8192 | `retention_days` on `day` |
| 13 | `user_rollup` | A | `toYYYYMM(day)` | **project_id, day, user_id**, marker_epoch, marker_batch_seq | bloom(user_id) | 8192 | `retention_days` on `day` |
| 14 | `span_metrics` | A | `toYYYYMM(minute)` | **project_id, minute**, environment, observation_type, model_response, level, release, marker_epoch, marker_batch_seq | none | 8192 | `retention_days` on `minute` |
| 15 | `mcp_tool_rollup` | A | `toYYYYMM(minute)` | **project_id, side, minute**, server_name, deploy_id, capture_source, era, method, target_name, client_name, error_class, caller_fault, marker_epoch, marker_batch_seq | none | 8192 | `retention_days` on `minute` |
| 16 | `mcp_deploys` | A | none | **project_id, server_name, deploy_id**, server_version, capture_source, marker_epoch, marker_batch_seq | none | 8192 | `retention_days` on `last_seen` |
| 17 | `dedup_selftest` | P | none | **id** | none | 8192 | rows older than 1 day purged |
| 18 | `sql_probe` | P | none | **project_id**, id | none | 8192 | none (access object; section 7.5; never written by the shipper) |

Partition granularity is a provisioning parameter chosen before the first migration: `day` (default) or `week`
(recommended on Cloud when retention exceeds one year, because of the service's limit on total parts). With `week`,
every `toDate(x)` partition expression and `row_day` become `toMonday(...)` of that value. It cannot be changed
without a new table and a backfill.

### 6.4 Columns

Columns are listed in `TableDef` order. Unless a codec is named, the convention in 6.1 applies.

**`span_core`**

| Columns | Type |
|---|---|
| `project_id` | UUID |
| `start_minute` | DateTime('UTC') |
| `trace_hash` | UInt32 |
| `start_time`, `end_time` | DT64 |
| `span_id`, `parent_span_id` | FixedString(8) |
| `trace_id` | FixedString(16) |
| `is_root`, `is_app_root`, `clock_skew` | UInt8 |
| `kind`, `status_code`, `observation_type`, `level`, `trace_public`, `cost_source`, `mcp_side`, `io_shape`, `input_truncation`, `output_truncation`, `source_format` | Enum8 |
| `name`, `status_message`, `service_version`, `scope_version`, `telemetry_sdk_version`, `sdk_version`, `release`, `version`, `trace_name`, `user_id`, `session_id`, `prompt_name`, `prompt_version` | String |
| `service_name`, `scope_name`, `telemetry_sdk_name`, `telemetry_sdk_language`, `sdk_name`, `environment`, `model_requested`, `model_response`, `provider` | LC |
| `duration_us`, `input_tokens`, `output_tokens`, `total_tokens`, `input_bytes`, `output_bytes` | UInt64 |
| `completion_start_time` | Nullable(DateTime64(6, 'UTC')) |
| `trace_tags`, `tags`, `tool_names` | Array(String) |
| `finish_reasons`, `usage_keys` | Array(LC) |
| `usage_values` | Array(UInt64) |
| `cost_values` | Array(Int64), aligned with `usage_keys` |
| `total_cost_nusd` | Int64 |
| `price_model_id`, `price_tier_id`, `api_key_id` | UUID |
| `tool_call_count`, `normalizer_version`, `event_bytes` | UInt32 |
| `compat_dialect` | UInt8 |
| `input_preview`, `output_preview` | String |
| `input_sha256`, `output_sha256` | FixedString(32) |
| `row_digest` | UInt64 |
| `usage_aggregate` | UInt8 (added by the step in 6.6) |
| `wal_epoch`, `batch_seq`, `lsn`, `received_at` | ingestion columns |

**`span_payload`**

| Columns | Type |
|---|---|
| `project_id`, `start_minute`, `trace_hash`, `span_id`, `trace_id`, `start_time` | as `span_core` |
| `user_id`, `session_id` | String |
| `input`, `output`, `system_instructions`, `attributes`, `resource_attributes`, `events`, `links`, `tool_definitions`, `tool_calls`, `model_parameters`, `trace_metadata` | String CODEC(ZSTD(3)) |
| `metadata_keys`, `metadata_values`, `metadata_kv` | Array(String) |
| `usage_provided_keys`, `cost_provided_keys` | Array(LC) |
| `usage_provided_values` | Array(UInt64) |
| `cost_provided_values` | Array(Int64) |
| `trace_state` | String |
| `truncated_fields` | Array(LC) |
| `wal_epoch`, `batch_seq`, `lsn`, `received_at` | ingestion columns |

**`mcp_ops`**

| Columns | Type |
|---|---|
| `project_id` | UUID |
| `side`, `capture_source`, `era`, `transport`, `target_kind`, `direction`, `result_type`, `cancel_source`, `status`, `error_class`, `error_subclass`, `identity_source`, `group_key_kind`, `content_truncation` | Enum8 |
| `start_minute`, `trace_hash`, `start_time` | as `span_core` |
| `span_id`, `parent_span_id`, `transport_link_span_id` | FixedString(8) |
| `trace_id`, `transport_link_trace_id`, `user_subject_hash`, `mrtr_state_in`, `mrtr_state_out` | FixedString(16) |
| `server_name`, `method`, `target_name`, `client_name` | LC |
| `server_version`, `deploy_id`, `instance_id`, `protocol_version`, `jsonrpc_id`, `task_id`, `subscription_id`, `error_type`, `error_message`, `client_version`, `oauth_client_id`, `user_id`, `user_issuer`, `tenant`, `session_key`, `handle_id`, `group_key`, `ip_prefix`, `user_agent` | String |
| `is_notification`, `is_batch`, `timed_out`, `caller_fault`, `content_captured` | UInt8 |
| `mrtr_round` | UInt16 |
| `duration_us`, `request_bytes`, `response_bytes`, `client_capabilities_hash` | UInt64 |
| `progress_count` | Nullable(UInt32) |
| `jsonrpc_code` | Nullable(Int32) |
| `http_status` | Nullable(UInt16) |
| `content_block_types` | Array(LC) |
| `content_block_counts` | Array(UInt32) |
| `content_sample_rate` | Float32 |
| `normalizer_version` | UInt32 |
| `wal_epoch`, `batch_seq`, `lsn`, `received_at` | ingestion columns |

**`mcp_content`**: `project_id`, `side`, `start_minute`, `trace_hash`, `span_id`, `trace_id`, `start_time`,
`user_id String`, `user_subject_hash FixedString(16)`, `target_name LC`, `args String CODEC(ZSTD(3))`,
`result String CODEC(ZSTD(3))`, `args_bytes UInt64`, `result_bytes UInt64`, `content_truncation Enum8`, ingestion
columns. A row exists only when content was captured.

**`fact_scores`, `fact_tags`, `fact_flags`, `fact_corrections`** share: `project_id UUID`, `key String`,
`target_day Date`, `trace_id FixedString(16)`, `span_id FixedString(8)`, `session_id String`, `version UInt64`,
`fact_id UUID`, `target_kind Enum8`, `target_time DT64`, `retracted UInt8`, `author_kind Enum8`, `author_id UUID`,
`created_at DT64`, ingestion columns. Body columns:

| Table | Body columns |
|---|---|
| `fact_scores` | `data_type Enum8`, `value_num Float64`, `value_str String`, `comment String`, `source Enum8`, `metadata String`, `config_id UUID` |
| `fact_tags` | `op Enum8` |
| `fact_flags` | `value UInt8` |
| `fact_corrections` | `corrected String CODEC(ZSTD(3))`, `note String` |

**`trace_heads`**: `project_id UUID`, `start_time DT64`, `trace_hash UInt32`, `trace_id FixedString(16)`,
`span_id FixedString(8)`, `head_kind Enum8`, `end_time DT64`, `name String`, `level Enum8`, `status_code Enum8`,
`user_id String`, `session_id String`, `tags Array(String)`, `environment LC`, `release String`, `version String`,
`input_preview String`, `output_preview String`, ingestion columns.

**`trace_stats`**: key `project_id UUID`, `trace_id FixedString(16)`, `row_day Date`, marker columns. Measures:
`min_start SAF(min, DateTime64(6, 'UTC'))`, `max_end SAF(max, DateTime64(6, 'UTC'))`, `has_root SAF(max, UInt8)`,
`min_head_start SAF(min, DateTime64(6, 'UTC'))`, `max_level SAF(max, UInt8)`, `span_count`, `error_count`,
`generation_count`, `tool_count`, `mcp_count`, `input_tokens`, `output_tokens`, `total_tokens` each
`SAF(sum, UInt64)`, `total_cost_nusd SAF(sum, Int64)`,
`environments SAF(groupUniqArrayArray(16), Array(String))`, and `trace_name_pick`, `user_id_pick`,
`session_id_pick` each `SAF(max, String)`. `min_head_start` holds the maximum representable time on rows with no
head.

**`session_rollup`**: key `project_id`, `day Date`, `session_id String`, marker columns. Measures:
`first_seen SAF(min, DateTime64(6, 'UTC'))`, `last_seen SAF(max, DateTime64(6, 'UTC'))`, `trace_count`,
`span_count`, `error_count`, `input_tokens`, `output_tokens`, `total_tokens` each `SAF(sum, UInt64)`,
`total_cost_nusd SAF(sum, Int64)`, `user_ids SAF(groupUniqArrayArray(8), Array(String))`.

**`user_rollup`**: key `project_id`, `day Date`, `user_id String`, marker columns. Measures: `first_seen`,
`last_seen`, `trace_count`, `span_count`, `error_count`, token sums and `total_cost_nusd` as in `session_rollup`.

**`span_metrics`**: key `project_id`, `minute DateTime('UTC')`, `environment LC`, `observation_type Enum8`,
`model_response LC`, `level Enum8`, `release String`, marker columns. Measures: `count`, `error_count`,
`input_tokens`, `output_tokens`, `total_tokens` each `SAF(sum, UInt64)`, `total_cost_nusd SAF(sum, Int64)`,
`dur_hist` (histogram), `ttft_hist` (histogram of `completion_start_time - start_time`, generations only).

**`mcp_tool_rollup`**: key `project_id`, `side Enum8`, `minute DateTime('UTC')`, `server_name LC`,
`deploy_id String`, `capture_source Enum8`, `era Enum8`, `method LC`, `target_name LC`, `client_name LC`,
`error_class Enum8`, `caller_fault UInt8`, marker columns. Measures: `count SAF(sum, UInt64)`,
`request_bytes SAF(sum, UInt64)`, `response_bytes SAF(sum, UInt64)`, `dur_hist` (histogram), `semconv_hist`
(`SAF(sumMap, Tuple(Array(UInt8), Array(UInt64)))`: exact counts per fixed bucket of the MCP operation-duration
metric; the boundary list is the constant `MCP_SEMCONV_DURATION_BOUNDS` in `crucible-records`, frozen at semantic
conventions v1.40.0: inclusive upper bounds of 10, 20, 50, 100, 200 and 500 ms and 1, 2, 5, 10, 30, 60, 120 and
300 s, giving 15 buckets with the last above 300 s), and
`last_seen SAF(max, DateTime64(6, 'UTC'))`.

**`mcp_deploys`**: key `project_id`, `server_name LC`, `deploy_id String`, `server_version String`,
`capture_source Enum8`, marker columns. Measures: `first_seen SAF(min, DateTime64(6, 'UTC'))`,
`last_seen SAF(max, DateTime64(6, 'UTC'))`.

**`mcp_tool_inventory`**: key `project_id`, `server_name LC`, `deploy_id String`, `tool_name LC`,
`schema_hash UInt64`, marker columns. Measures: `first_seen`, `last_seen` as above,
`capture_sources SAF(groupUniqArrayArray(4), Array(UInt8))`.

**`dedup_selftest`**: `id UUID`, `wal_epoch UUID`, `batch_seq UInt64`, `created_at DT64`.

**`sql_probe`**: `project_id UUID`, `id UInt32`, `canary String`, `start_time DT64`.

On tables with no partition expression the marker row's key columns hold type defaults; there is no time column to
set.

### 6.5 Deterministic latency histogram

Latency percentiles use one integer log-linear histogram, defined in `crucible-records` and used identically by the
shipper, the query post-operator and `ChModel`. It has 16 sub-buckets per octave.

For a value `v` (a `u64` count of microseconds):

```text
bucket(v):
  if v < 16:            return v
  m   = index of the most significant set bit of v      (4 <= m <= 63)
  sub = (v >> (m - 4)) & 15
  return (m - 3) * 16 + sub

lower(b), width(b):
  if b < 16:            return (b, 1)
  m   = b / 16 + 3
  sub = b % 16
  return ((16 + sub) << (m - 4), 1 << (m - 4))

report(b) = lower(b) + width(b) / 2          (integer division)
```

| Property | Value |
|---|---|
| Bucket index range | 0 to 975 for the full `u64` domain; stored as `UInt16`. Durations are not capped |
| Storage | Sparse map from bucket index to count; merged with `sumMap`, so a merge is integer addition and the result does not depend on merge order, part layout or server version |
| Quantile `q` (given in parts per million, for example 990000) | `rank = max(1, ceil(q * total / 1000000))` in integer arithmetic; walk buckets in ascending index until the cumulative count reaches `rank`; return `report(b)` of that bucket |
| Error | Values below 32 are exact. Otherwise the reported value is within `1/32` (3.125%) of every value in the bucket; summaries elsewhere quote this as 3.1%. Tests and conformance row C14 must use 3.125% |
| Distinct counts | `uniqCombined64` at read time (deterministic); its error is stated by C14 |

### 6.6 Migrations and provisioning

- The ledger `schema_migrations` lives in Postgres. Steps are forward-only and idempotent and are applied by
  `crucible migrate` or by the lease holder at startup, as `crucible_admin`.
- There are two kinds of step: schema steps (this section) and access steps (section 7). Both render through one
  flavour switch `clickhouse.flavor = auto | single | cluster | cloud`.
- A change to a sort key or partition expression means a new table and a backfill job, never a dual write.
- A schema step that creates a base table must, in that same step, create the table's service-identity row policy
  (section 7.3).
- Schema step `0004_span_usage_aggregate` adds the `UInt8` column `usage_aggregate` (section 3.1) to `span_core`,
  after `row_digest`. Like every step it is forward-only and idempotent; rows written before the step read 0. The
  shipper's rollup functions, the system simulation's I7 oracle and the fixtures' goldens apply the sums rule of
  3.1 from the change that starts writing the column.

Ledger (settled in code): with Postgres configured, `crucible migrate` (and `serve --migrate`) runs every pending
Postgres migration, then every pending ClickHouse schema and access step in `crucible_schema::LEDGER_ORDER`, under the
`migrations` lease. Each ClickHouse step is recorded after it applied, in a transaction fenced by the lease token
(steps are idempotent, so a crash between apply and record only repeats the step). A step that outlives the lease is
not recorded and the run reports that it must be repeated. A ledger row whose checksum differs from the step (for
example after a master-key change) makes the step reapply, with a warning. Without Postgres, `--migrate` applies the
ClickHouse steps on every start.

### 6.7 Settled in code

Found against ClickHouse 26.3, 26.8, 26.10 and a one-shard, two-replica Keeper cluster.

| Topic | Rule |
|---|---|
| Step order | Migration ids `0001_schema_base`, then `0002_access_service` (access step A1 with every service policy), then `0003_access_workspace` (A2). A later table step creates its own service policies with `crucible_schema::service_policy` |
| Clustered DDL | `ON CLUSTER` statements carry `distributed_ddl_entry_format_version = 8` or higher, and every host must set `distributed_ddl_use_initial_user_and_roles = 1` (text-index preprocessors need it); startup detection (5.8) checks the server setting on the clustered variant |
| Replication path | `'/clickhouse/tables/{shard}/{database}/{table}', '{replica}'` |
| Profile ceilings | Constraints use `MIN 1`, because 0 means unlimited for these settings |
| Codecs | `T64` on scalar `UInt32`, `UInt64` and `Int64` columns except hash-valued ones; arrays and `start_minute` use `ZSTD(1)` |
| Column order | Fact body columns precede the ingestion columns; marker columns follow the key columns |
| Skip-index granularity | Unspecified granularities are 1; text indexes use `GRANULARITY 100000000` |
| Defaults | No column defaults |
| Marker-row enums | Enums without a 0 value (`capture_source`, `era`) take their first variant in marker rows |
| Native block headers | The TableDef type, except `LowCardinality(String)` is sent as `String`; `SimpleAggregateFunction` wrappers are kept, so the server converts nothing |
| Insert compression | ZSTD level 1 when ZSTD is configured. A `compress=1` response may come back LZ4 or ZSTD (26.10 answers ZSTD), and readers accept both |
| Empty blocks | Never sent; a zero-row block is a header only and creates no part |

## 7. ClickHouse identities and access

### 7.1 The final identity set

This is the one scheme. Earlier drafts' names `crucible_read` and a maintenance-capable `crucible_admin` do not
exist.

| Identity | Created by | Used by | Grants | Settings profile |
|---|---|---|---|---|
| `crucible_admin` | The operator (supplied in config) | `crucible migrate`, schema and access steps, the SQL identity reconciler | All privileges on `crucible.*` and `crucible_sql.*` with grant option; access management (users, roles, row policies, quotas, settings profiles) | none imposed |
| `crucible_ingest` | Access step A1 | `insert_block`, `BatchProbe`, `dedup_selftest` | `INSERT` on every `crucible` table except `sql_probe`; column-level `SELECT` on the probe columns only (`_partition_id`, `project_id`, `wal_epoch`, `batch_seq`, `marker_epoch`, `marker_batch_seq`, and `id` on `dedup_selftest`) | `crucible_ingest_profile`: `max_execution_time` at most 60 |
| `crucible_read_ui` | A1 | Typed plans for session-authenticated requests, alert evaluation, exports and jobs. Classes `point`, `list`, `search`, `analytics`, `analytics_raw`, `alert`, `export` | `SELECT` on `crucible.*` | `crucible_read_ui_profile`: `readonly = 2`; maximum constraints equal to the largest class values in section 8.4 |
| `crucible_read_agent` | A1 | Typed plans for API-key principals and the MCP server. Class `agent` | `SELECT` on `crucible.*` | `crucible_read_agent_profile`: `readonly = 2`; maximum constraints equal to the `agent` class; `max_concurrent_queries_for_user = 16` (default; revisit). Quota `crucible_agent_quota`: per hour 100,000 queries and 7,200 s of execution time (default; revisit) |
| `crucible_maint` | A1 | `crucible-lifecycle` (deletes, partition drops, mask application, marker purge), `detect`, `kill`, health | `ALTER DELETE` on `crucible.*` (it covers mutations, partition drops and `APPLY DELETED MASK`) and per-table `ALTER UPDATE(_row_exists)` (a lightweight `DELETE` needs it); `SELECT` on `crucible.*`; `KILL QUERY`; `SELECT` on `system.mutations`, `system.parts`, `system.settings`, `system.server_settings`, `system.merge_tree_settings`, `system.functions`, `system.processes`, `system.tables` (`detect` reads engine families and the clustered server setting) | `crucible_maint_profile` |
| `crucible_sql_<project hex>` | The reconciler (section 7.4) | `SqlWorkspaceStore` for one project | Role `crucible_sql_reader` | `crucible_sql_profile`, quota `crucible_sql_quota` |

`<project hex>` is the project id as 32 lowercase hex characters without hyphens.

The five identities in the first five rows are the **service identities**. `KILL QUERY` and all runtime
maintenance belong to `crucible_maint`; DDL and access management belong to `crucible_admin` only.

Credentials: the operator supplies `crucible_admin`. Passwords for the other service identities and for workspace
identities are computed by HKDF-SHA256 from the master key with the identity name and a generation counter, so
nothing is stored and rotation is a generation bump (`crucible admin rotate-keys`). An optional
`clickhouse.read_url` points the two read identities and the workspace at separate compute.

Fallback: `clickhouse.identity_mode = split` (default) or `single`. In `single` (for operators whose admin user
lacks access management) every service role connects with the operator-supplied credentials, no access steps run,
and the SQL workspace is disabled with reason `access_management_unavailable`. (default; revisit)

### 7.2 Row-policy semantics that the scheme relies on

Once any row policy exists on a table, what an identity without a matching permissive policy reads depends on the
server switch `users_without_row_policies_can_read_rows`: every row under the permissive setting, no row under the
strict one. Crucible must behave identically under both settings. Two rules follow.

### 7.3 Rule 1: service policies first

**Every service identity must hold a `USING 1` row policy on every base table before any workspace policy is
created on that table.** Under the strict setting a missing policy would make the ingest existence probe read
"absent" and re-insert a landed block, or make the UI read an empty project.

- Access step A1 creates the four generated service identities and their profiles, and, for every base table `T`
  in `crucible`, one policy:
  `CREATE ROW POLICY IF NOT EXISTS crucible_service_<T> ON crucible.<T> AS PERMISSIVE FOR SELECT USING 1 TO
  crucible_admin, crucible_ingest, crucible_read_ui, crucible_read_agent, crucible_maint`.
  One policy per table is rendered from the table list; no wildcard policy syntax is assumed.
- Access step A2 (workspace role, profile, quota, static policies, views) must be ordered after A1 in the ledger,
  and its first action is to verify that A1's policies exist for every table.
- Any later schema step that creates a base table creates that table's `crucible_service_<T>` policy in the same
  step, before any workspace policy on it.
- The reconciler must check that `crucible_service_<T>` exists before creating a per-project policy on `T`.
- Conformance row "service identities see all rows after access migrations" (26.3, 26.8, latest, Cloud; both
  values of the server switch) and the startup check in section 5.8 verify the rule.

### 7.4 Rule 2: workspace identities

The SQL workspace is the one exception to the typed-plan rule. Isolation between projects is carried only by
ClickHouse access control; if every Crucible check were bypassed, another project's rows must still be unreadable.

**Role and grants.** Role `crucible_sql_reader` gets column-level `SELECT` on the curated column list of each base
table (the `TableDef` marks SQL-readable columns; ingestion and marker columns, `api_key_id` and `row_digest` are
excluded) and `SELECT` on database `crucible_sql`. It gets nothing else: no source or table-function grants, no
dictionary access, no temporary tables, no `SYSTEM`, no `KILL`, no access to `dedup_selftest`.

**Curated views (`crucible_sql`).** `spans`, `span_payloads`, `traces`, `sessions`, `users`, `mcp_operations`,
`mcp_contents`, `scores`, `tags`, `flags`, `corrections`, `span_metrics`, `mcp_tool_metrics`, `mcp_tools`,
`mcp_deploys`. Views run with invoker rights, so the caller's base-table policies apply. Views over aggregating
tables finalize the partial rows (group by key) and expose ids as hex strings. v0 ships base-table grants; the
known residual (whole-table row and byte totals visible through system metadata) is documented. The grant set
switches to views only once conformance script W12 is green on 26.3, 26.8, latest and Cloud.

**Two policy layers, each in two forms.** Both layers sit on base tables.

| Layer | Created by | Addressed to | Predicate |
|---|---|---|---|
| Static, role-wide | Access step A2, per SQL-readable table | `crucible_sql_reader` | `project_id = <the 32 hex characters after the prefix "crucible_sql_" in currentUser(), parsed with toUUIDOrZero> AND project_id != <nil UUID>` |
| Per project | The reconciler, per SQL-readable table | `crucible_sql_<project hex>` | `project_id = '<uuid>' AND <table time column> >= now() - toIntervalDay(<retention>)`; the time term is omitted when that retention is unset |

Each predicate is created twice: once `AS PERMISSIVE` and once `AS RESTRICTIVE`. With both forms present the
effective filter is the conjunction of both layers whichever way the server switch is set: the permissive forms
grant only the project's rows, and the restrictive forms subtract everything else, including rows past the
retention cut-off. A user in the role with a malformed name, or with no per-project policy, matches no row.

The retention term uses the table's retention binding (section 6.3) and time column (`start_time`, `target_time`,
`minute`, `day`, `row_day` or `last_seen`). The reconciler rewrites per-project policies when retention changes,
so retention hides rows from SQL immediately.

**Provisioning order** (binding): the user without its role, then per-project policies (both forms, every table),
then the role grant. ClickHouse refuses a policy for a user that does not exist (code 511), so the user comes
first; it can read nothing until the grant. Deprovisioning is the reverse. Identities are provisioned lazily on first workspace use and dropped after
30 idle days. State lives in Postgres `ch_sql_identities` (`project_id`, `generation`, `state`, `last_probe`).
The reconciler runs every 5 minutes and on project creation, under the leader lease.

**Settings profile `crucible_sql_profile`.** `readonly = 1`, and every value below is also declared `CONST`.
Values are deployment defaults, tunable through config before provisioning.

| Group | Settings |
|---|---|
| Mode | `readonly = 1`, `allow_ddl = 0`, `allow_introspection_functions = 0`, `serialize_query_plan = 0`, `use_query_cache = 0` |
| Time and memory | `max_execution_time = 30`, `max_memory_usage` = 2 GiB, `max_memory_usage_for_user` = 4 GiB, no external sort or group-by spill |
| Read volume | `max_rows_to_read = 1000000000`, `max_bytes_to_read` = 50 GiB, `max_columns_to_read = 200`, overflow mode throw |
| Result | `max_result_rows = 10001`, `max_result_bytes` = 32 MiB, overflow mode break |
| Complexity | `max_ast_depth = 200`, `max_ast_elements = 10000`, `max_expanded_ast_elements = 100000`, `max_subquery_depth = 10`, `max_query_size` = 128 KiB, join, set and group-by row and byte caps, sleep functions disabled |
| Concurrency | `max_threads = 4`, `max_concurrent_queries_for_user = 2` |
| HTTP | `cancel_http_readonly_queries_on_client_close = 1`, 64-bit integers quoted in JSON output, ISO date output |
| Window backstops | `force_index_by_date = 1` and `max_partitions_to_read`, enabled only if conformance script W13 shows they work through the views |

**Quota `crucible_sql_quota`**, keyed by user name: per hour 1,800 queries (the operator setting
`sql.statements_per_hour`, `CRUCIBLE_SQL_STATEMENTS_PER_HOUR`; one API run is about three statements because the
validation `EXPLAIN`s count, so about 600 runs), 300 errors, 20,000,000,000 read rows and 1,800 s of execution time.
The quota is applied at every start, beside the profile; a refusal on the `queries` limit names the setting. The probe
identities are outside the quota.

**The time window is a guardrail, not isolation.** The API requires `from` and `to` and passes them as the query
parameters `{from}` and `{to}`; a lexical check requires the text to use both (`sql_time_window_required`). If the
window is bypassed, the damage is one project scanning its own data until a profile limit or the quota stops it.

**Validation and execution** (`crucible-sqlws`): a small Rust lexer for user experience only (size cap, single
statement, starts with `SELECT` or `WITH`, window parameters present, friendly rejection of `SETTINGS`, `FORMAT`
and `INTO OUTFILE`); then `EXPLAIN AST` and `EXPLAIN ESTIMATE` as the project identity; then the text is sent
verbatim as the whole statement with only `query_id`, the output format and `param_*` values as URL parameters.
Results stream as a typed grid: at most 10,000 rows, 16 MiB, cells cut at 64 KiB with a marker. Every run writes a
`sql_runs` row. Cancel drops the connection and `crucible_maint` issues `KILL QUERY`. Application-side limits: 1
concurrent run per user, 4 per project, 30 runs per minute per user.

### 7.5 Verification of the access scheme

- `sql_probe` holds canary rows for two reserved probe project ids. Two workspace identities for those ids are
  provisioned by A2 exactly like project identities.
- **Runtime self-probe**: at startup, every 5 minutes and after a ClickHouse version change, about 25 statements
  run as a probe identity (cross-project canary read, a system log table, a URL table function, `SET`, `INSERT`, a
  settings override). Any unexpected success disables the workspace deployment-wide with
  `sql_workspace_unavailable` and a reason in health and in the session DTO.
- **`sqliso` conformance suite** (`crucible-conformance`; real ClickHouse only): layer A direct attacks W1 to W15
  as the project identity; layer B grammar fuzzing with canary oracles (5,000 queries per PR, 500,000 nightly);
  layer C through the API; layer D mutants (policy dropped, a source granted, `readonly = 2`, a system log table
  granted, profile missing, the server switch flipped) that must each fail the suite.

## 8. Read side

### 8.1 Typed query-plan IR

Owner: `crucible-plan`. Every telemetry read of project data is a `QueryPlan`; `crucible-store-clickhouse` renders
it to SQL and `ChModel` interprets it. The SQL workspace (section 7.4) and the existence probe below are the only
exceptions.

```text
QueryPlan { scope: ProjectScope, window: TimeWindow, class: QueryClass, root: Node }
```

A plan cannot be constructed without a scope and a window.

| Node | Meaning |
|---|---|
| `Scan` | Table, columns, `Pred`, optional key order (ascending or descending), limit, keyset cursor, optional arrival bounds (`received_at <= as_of`, `received_at > since`) |
| `PointLookup` | Table, key list, columns |
| `Aggregate` | Input table, `Pred`, group dimensions, measures from a closed set (`count`, `sum`, `min`, `max`, `uniq`, histogram merge, pick merge, latest-by-version), having, order, limit |
| `SemiJoin` | A left `Scan` restricted to ids produced by a right node, with `max_ids` (default 10,000; exceeding it is `query_too_broad`) |
| `TextSearch` | Payload table, fields, token query (`all`, `any`, `phrase`), `Pred`, limit |
| `TimeSeries` | An `Aggregate` bucketed by a granularity, with gap fill. A zero-filled bucket carries the series' dimension values, 0 for `count` and `sum`, and null for `min`, `max`, `uniq`, histogram and pick measures |
| `FieldRange` | Point lookup of one payload field with a byte range |

`Pred` is a closed AST: `And`, `Or`, `Not`, comparisons (`Eq`, `Ne`, `Lt`, `Le`, `Gt`, `Ge`) between a column from
the table's column enum and a typed literal, `In`, `StartsWith`, array `HasAny`, `MetadataEq(key, value)` and
`TextMatch` on `name`. **`Pred` has no project variant.** The renderer adds `project_id = {scope}` to every table
reference itself, and adds `project_id != nil` nowhere because a scope can never be nil.

`BatchProbe { table, partition, epoch, batch_seq }` (section 5.5) is deliberately **not a `Node`**: it is the one
read without a project predicate, so it is a separate type that no `QueryPlan` can contain, and it runs through
`TelemetryStore::probe`, which returns only a boolean. Its constructor `BatchProbe::for_shipper` is a disallowed
method in `clippy.toml`, and the CI allow-attribute guard permits the allow in exactly one file,
`crates/crucible-shipper/src/probe.rs`. Tests of stores obtain probes from `crucible_plan::test_support` (feature
`test-support`, which product builds never enable).

`DeletePlan { scope or marker purge, table, partition, pred }` is the separate typed form used by
`crucible-lifecycle`.

**Output schema.** `QueryPlan::output()` returns the ordered output columns (label and type) that every row of the
plan follows. It is the single definition of a result's shape: the adapter and `ChModel` must both return rows that
pass `OutputSchema::check_row`, and a store reports a result that does not as `internal_error`.

| Root | Output columns |
|---|---|
| `Scan`, `PointLookup` | The node's columns, in the order given |
| `Aggregate` | The group dimensions, then one column per measure, in the order given |
| `SemiJoin` | The columns of the left scan |
| `TextSearch` | The table's key columns (its `ORDER BY` columns after `project_id`) |
| `TimeSeries` | `bucket` (the first instant of the bucket, `DateTime('UTC')`), then the aggregate's output |
| `FieldRange` | The field |

- Output types are result types, never storage wrappers: `SimpleAggregateFunction(f, T)` is `T` and
  `LowCardinality(String)` is `String`. `count` and `uniq` are `UInt64`; `sum` is `UInt64`, `Int64` or `Float64` by
  the column's signedness; the other measures have their column's result type; a histogram merge is the
  `(keys, counts)` tuple with keys ascending and unique.
- Each datum has one representation as a `Value`: `UInt`, `Int`, `Float`, `String`, `Bytes`, `Uuid`, `Date`,
  `Time`, `Enum` (the stored value), `Array`, `Tuple2`, and `Null` only under `Nullable`. A `String` is
  `Value::String` when its bytes are valid UTF-8 and `Value::Bytes` otherwise (a `FieldRange` may cut a character);
  a `FixedString` is always `Value::Bytes`.
- Columns are identified by position; labels are for diagnostics.
- A node with an order returns rows in that order; rows that tie on it, and the rows of a node without an order,
  come in no defined order.
- A grouped read returns one row per group with at least one matching row. An `Aggregate` without group dimensions
  is one group: it returns one row when at least one row matches and no row otherwise, never a row of defaults.
  The one exception is a `TimeSeries` with zero fill, which also returns rows for its empty buckets.

### 8.2 Post-operators

Post-operators run in Rust above the store (`crucible-query`) and are identical in simulation and production:
merge with the recent-span index, fact decoration (section 3.8), histogram quantiles (section 6.5), pick
resolution (section 3.3), provisional-head filtering (section 3.9), span de-duplication by `span_id`, automatic
capture-source selection (section 9.5), and search highlighting.

### 8.3 Rendering rules

- SQL is sent with client-side binding disabled. **Every value is a server-side parameter** `{pN:Type}`; no value
  is ever formatted into SQL text.
- **Identifiers come only from schema enums** (tables, columns, functions). SQL text depends on plan shape and on
  list arity only; `In` lists are padded to the next power of two.
- For each table the renderer computes the sort-key and partition predicates from the window (`start_minute`,
  `toDate`, `minute`, `day`), clamps `from` to the table's retention binding (so expired rows are hidden at once),
  and adds `received_at <= as_of` from the cursor on plain tables.
- Every statement carries a `query_id` (UUIDv7 from `Rng` and `Clock`) and the class settings, explicitly.
- No rendered statement contains `FINAL`. Aggregating tables are always read through an `Aggregate` whose dimensions
  are key columns (any subset of the key: re-aggregating `SimpleAggregateFunction` columns is associative).
- A property test asserts that every rendered statement other than `BatchProbe` carries the project predicate and
  the window predicate and contains only bound values.

### 8.4 Query classes

Defaults are configurable per deployment. The Rust-side deadline is the class time plus 2 s. All classes set
`timeout_overflow_mode = throw` and `read_overflow_mode = throw`.

| Class | `max_execution_time` | `max_memory_usage` | `max_rows_to_read` | `max_threads` | Identity | Used by |
|---|---|---|---|---|---|---|
| `point` | 2 s | 1 GiB | 5,000,000 | 4 | `crucible_read_ui` | Trace and span detail, payload, lookups |
| `list` | 3 s | 2 GiB | 200,000,000 | 8 | `crucible_read_ui` | Trace, span, session, user, operation lists; filter values |
| `search` | 10 s | 4 GiB | 500,000,000 | 8 | `crucible_read_ui` | Full-text search |
| `analytics` | 10 s | 4 GiB | 200,000,000 | 8 | `crucible_read_ui` | Rollup reads: metrics, MCP dashboards, data bounds |
| `analytics_raw` | 30 s | 8 GiB | 2,000,000,000 | 8 | `crucible_read_ui` | Metrics that the rollups do not cover; aggregate filters; MCP users and sessions |
| `alert` | 10 s | 2 GiB | 200,000,000 | 2 | `crucible_read_ui` | Alert evaluation (rollups only) |
| `agent` | 5 s | 1 GiB | 100,000,000 | 4 | `crucible_read_agent` | Every read by an API-key principal or through the MCP server |
| `export` | 300 s | 2 GiB | none | 2 | `crucible_read_ui` | Export jobs |

Guardrail failures are never retried by the server and map to `query_too_broad` (rows or bytes), `query_timeout`
and `query_memory_exceeded`.

### 8.5 Cancellation

Read classes set `cancel_http_readonly_queries_on_client_close = 1` explicitly. A caller cancels a read by dropping
its `RowStream`; a client disconnect drops it. On that drop, or on the Rust deadline (which ends the stream with
`query_timeout`), the adapter drops the connection and issues `KILL QUERY` by `query_id` through `crucible_maint`.
`TelemetryStore::kill(query_id)` with `RowStream::query_id()` is the same path for a caller that cannot drop the
stream. Whether cancel-on-close fires for POSTed SELECTs is pinned by conformance C13; the `KILL QUERY` path must
work regardless.

### 8.6 Recent-span index

Owner: `crucible-recent`. It makes a span readable within about 100 ms of its ack and keeps it readable until
ClickHouse serves it.

| Aspect | Contract |
|---|---|
| Entry | About 200 bytes: ids, times, type, level, model, cost, tokens, interned name, user and session, `lsn`, and a WAL locator (segment, offset, span ordinal). No payload and no previews |
| Payload of a recent span | Re-read the frame at the locator and re-normalize that span (the normalizer is pure) |
| Maps | By (project, trace_id); by (project, start_time, span_id); by (project, lsn) for live tail; a per-trace summary that includes rootless traces. 16 shards behind read-write locks; one writer (stage B) |
| Bound | `recent.max_bytes`, default min(1 GiB, 10% of the container's memory limit (`/sys/fs/cgroup/memory.max`), else of the host's RAM) |
| Eviction | Oldest LSN first, and only entries with `lsn <=` the checkpoint watermark whose checkpoint is older than the visibility grace (2 s single node; 10 s clustered or Cloud) |
| Read barrier | Trace-by-id snapshots the node's durable LSN on arrival and waits, at most 250 ms (`recent.read_barrier`), until stage B has applied through it. If the wait expires the response carries `consistency: lagging` |
| Gap rule | When the bound is hit before entries are evictable (a store outage), the oldest entries are dropped and the dropped LSN range is recorded as a gap. A gap closes when the watermark has passed its upper LSN and the grace has elapsed. Any read whose result could include rows from an open gap must return `partial: true` with `partial_reason: store_unavailable`. A silent miss is a contract violation |
| Rebuild | On recovery: every frame above the watermark, plus frames at or below it younger than the grace (section 4.6) |
| Tombstones | Stage B removes matching entries when it reaches a `Tombstone` frame; the pending-deletion set also filters every read |
| Merge | Lists merge-sort the store stream and the index stream by cursor tuple, de-duplicate by key and apply the limit. The index is consulted only for first pages, `since` requests, or cursors inside its range |
| Scope of the guarantee | Read-your-ack holds on the ingesting node. With more than one registered node, trace-by-id fans out to peers with a 50 ms budget; if a peer does not answer, the response carries `consistency: lagging` |

Response metadata on every telemetry list and detail response:

```text
meta { as_of: Timestamp, partial: bool, partial_reason?: "store_unavailable" | "deadline" | "quarantined",
       consistency: "strong" | "lagging" }
```

`strong` means the response reflects every span this node acknowledged before the request arrived. `lagging` means
it may trail (barrier expired, peer fan-out incomplete, or replica lag beyond the grace).

### 8.7 Endpoint plans

| Endpoint | Plan |
|---|---|
| Trace list | `Scan(trace_heads)` in key order with the cursor `(start_time, trace_hash, trace_id)`, over-reading 3 times the limit; then `Aggregate(trace_stats)` and three fact lookups for the page ids (scores without comment and metadata, tags, flags; trace detail and `GET facts` read all four); drop hidden provisional heads; merge recent |
| Trace detail | Recent index with the read barrier, in parallel with `Aggregate(trace_stats)` for time bounds; then `Scan(span_core)` bounded by them and by `trace_hash` and the bloom index; de-duplicate by `span_id` |
| Span children | `Scan(span_core)` with `parent_span_id = X` inside the trace bounds, keyset `(start_time, span_id)` |
| Span list | `Scan(span_core)` ordered by `start_minute`, `start_time`, `span_id` descending |
| Span detail and payload | `PointLookup(span_core)`, then `PointLookup(span_payload)` or `FieldRange` |
| Sessions, users | `Aggregate` over the rollup for the window, ordered by last seen, keyset `(last_seen, id)` |
| Search | `TextSearch(span_payload)` returning keys, then a core lookup and a bounded payload substring for the highlight. When the phrase function is unavailable, token AND plus verification in Rust |
| Fact filter | `SemiJoin(list scan, Aggregate(fact table by key and window, latest-by-version))` |
| Metrics, dashboards, alerts | Compiled to `span_metrics` or `mcp_tool_rollup` when the dimensions are covered (`source_tier: rollup`); otherwise `Aggregate(span_core)` or `Aggregate(mcp_ops)` in `analytics_raw` (`source_tier: core`). Alerts may use rollups only |
| MCP analytics | Section 9.9 |
| Data bounds | `Aggregate(span_metrics)` and `Aggregate(mcp_tool_rollup)` for min and max `minute`, merged with the recent index; class `analytics`; cached 30 s per project |
| Filter values | `Aggregate` grouped by the field, ordered by count, limit at most 100, optional `StartsWith`; over a rollup when the field is a rollup dimension, else over `trace_heads` or `span_core` in class `list` with the window clamped to 24 h (default; revisit) |
| Export | Streaming `Scan(span_core)` joined in Rust with payload lookups and fact decoration; class `export`; fixed `as_of` |

### 8.8 Owners for the UI's requests

Each item has a contract here, a route in section 12 and owning crates. `crucible-api` owns every route and
`crucible-api-types` every DTO; they are omitted from the last column.

| UI request | Contract | Plan and index support | Owning crates |
|---|---|---|---|
| `span_count` and paged span children above 5,000 spans | Trace detail always returns `span_count` (merged `trace_stats` plus recent entries). At or below `TRACE_INLINE_SPAN_LIMIT = 5000` it returns every core-tier span and `paged: false`. Above it, it returns `paged: true`, the root spans and their direct children (at most 1,000, in `(start_time, span_id)` order), and every returned span carries `child_count`. `GET .../traces/{trace_id}/spans?parent_span_id=&cursor=&limit=` (default 500, max 2,000) pages the direct children of one span; `?ancestors_of=<span_id>` returns that span and its ancestor chain (at most 256 levels) for deep links | `Scan(span_core)` keyset `(start_time, span_id)` with a `parent_span_id` predicate; `Aggregate(span_core)` grouped by `parent_span_id` for `child_count`; `PointLookup` chain for ancestors | `crucible-plan`, `crucible-query`, `crucible-recent` |
| Live-tail `since` | Every first page of the trace list returns `head_cursor` (a signed cursor of kind `head` holding the newest position and `as_of`). `GET .../traces?since=<head_cursor>` with unchanged filters returns `{ data, new_count, has_more, head_cursor }`: traces whose head arrived after the cursor's `as_of`, newest first, at most `limit`; `new_count` saturates at 100. It never changes existing cursors | `Scan(trace_heads)` with the arrival bound `received_at > since` (minmax index on `received_at`), merged with the recent index's (project, lsn) map | `crucible-plan`, `crucible-recent`, `crucible-query`, `crucible-cursor` |
| `partial: true` and `consistency: lagging` | The `meta` object of section 8.6 on every telemetry list and detail response | Gap tracking and the read barrier | `crucible-recent` (gap, barrier), `crucible-query` (meta) |
| `data-bounds` | `GET projects/{id}/data-bounds` returns `{ spans: { first, last }, mcp: { client: { first, last }, server: { first, last } }, retention_horizon, max_window_days, server_time }`. `first` and `last` are null when the project has never had data. Rollup bounds have minute precision | Section 8.7 | `crucible-query`, `crucible-plan`, `crucible-recent` |
| Filter-values lookup | `GET projects/{id}/filter-values?source=&field=&prefix=&from=&to=&limit=` returns `{ data: [{ value, count }], truncated }`. Fields come from the metrics registry; an unknown field is `validation_failed` | Section 8.7 | `crucible-plan` (registry), `crucible-query` |
| Metrics schema and caps | `GET projects/{id}/metrics/schema` returns, per source (`spans`, `mcp_ops`, `traces`): measures (id, label, unit, allowed aggregations, additive flag, tier), dimensions (id, type, high-cardinality flag), filter fields and operators, granularities and limits. It is generated from the one registry the plan compiler uses. Caps on `metrics/query`: at most 500 buckets per series, at most 20 series (the remainder becomes an `other` series for additive measures, or is dropped with `truncated: true` for percentiles), at most 10,000 points in total. `auto` granularity picks the finest step that fits; an explicit step that does not fit returns `query_too_broad` with `suggested_granularity` | `TimeSeries` and `Aggregate` over the rollups or the core tier | `crucible-plan` (registry and compiler), `crucible-query` |

`MetricsQuery` v1: `source`; up to 4 measures with aggregation; up to 2 dimensions; the shared filter AST;
`granularity` (`auto`, minute, 5 minutes, 15 minutes, hour, day, week); window; timezone; `top_n`; fill mode.
Response: series with keys and points, the granularity used, `truncated`, `source_tier`, `as_of`,
`partial_last_bucket`.

### 8.9 Settled in code

- Trace detail reads spans in `[min_start, max_end + 1 µs)`, so a span that starts exactly at the trace's end is
  included.
- `dedup_selftest` is reachable only through `TelemetryStore::dedup_selftest`, never through a plan.
- The retention clamp of 8.3 is structural: every plan builder takes a `RetentionClamp` and returns `None` when the
  window has fully expired; `QueryPlan::new` is private to `crucible-plan`.
- `partial_reason: store_unavailable` takes precedence over the other reasons. `not_found` is returned only when the
  response would be whole and strong; otherwise the answer is an empty trace carrying `partial` or `lagging`.
- Sums in lists and paged detail (`span_count`, cost, tokens, `child_count`) are store sums plus index entries above
  the watermark, so they can run ahead until the low watermark passes the spans; inline trace detail is exact.
- Token and cost sums follow the sums rule of 3.1 on every read path: store sums, the index entries above the
  watermark and sums computed at read time over `span_core` (inline trace detail included) leave out spans with
  `usage_aggregate = true`. Counts are not filtered.
- Filtered lists read the store only, because the index keeps no tags or release (it keeps each span's environment), and answer
  `consistency: lagging` when unconfirmed index heads were left out. The index is consulted on every list page.
- Heads of one trace with the same `(start_time, trace_hash, trace_id)` collapse to one row; a trace with several
  roots at different start times appears once per root.
- The read barrier must stay shorter than the visibility grace.
- The shipper feeds the index through `RecentHook`: `applied(frame, normalized, admission)` for ingest frames and
  `passed(frame)` for every other frame. After a restart the host restores quarantine marks from the manifests under
  the quarantine root (the `Quarantine` frames may have been collected), with reach `All`.
- `WalLocator.ordinal` is the span's stage A position in the frame (`Admission::stage_a_ordinals`), which is what a
  payload re-read of the frame sees.
- `ChModel` applies the query-class guardrails (rows, bytes, time and a memory estimate) and refuses as `TooBroad`
  like the server. It charges every landed row and byte of the table across all projects, so it refuses earlier than
  a server would, never later.
- Live tail: a head cursor carries the issuing node's `wal_epoch` and applied LSN. The store is read with
  `(wal_epoch = E AND (lsn > L OR received_at > as_of)) OR (wal_epoch != E AND received_at >= as_of)`, and the recent
  index by LSN only for a cursor from this node's WAL, so a head that was durable but unapplied when the cursor was
  issued, or that shares the cursor's microsecond, is not missed. The `OR` stops the `received_at` skip index from
  pruning the tail scan (a performance follow-up). A head applied while a page is being built can appear on that page
  and on the next tail, which only inflates `new_count`.
- `Fill::Zero` is supported. A zero-filled histogram bucket is `([], [])` and a zero-filled set merge is `[]`, not
  null, because the floor version has no nullable tuple or array; `sum` in a filled bucket is 0, including a bucket
  whose values are all null.
- `MetricsQuery.timezone` is accepted but not yet compiled: buckets are UTC. Only the `spans` source over
  `span_metrics` is compiled; the `analytics_raw` fallback and the `mcp_ops` and `traces` sources come with their
  slices.
- A filter's canonical form and its 16-byte hash (`Filter::hash`) are what cursors bind to (`FilterHash`). A span-list
  metadata condition must be a top-level conjunct; any other placement is refused as `UnsupportedFilter`.
- Session and user list pages are not snapshots: the rollups carry no `as_of`, so a later page can reflect newer
  rows.
- `TokenMode::Phrase` renders as all-tokens, because 26.3 has no phrase-match function; `Pred::TextMatch` is a
  case-insensitive substring match.
- `SemiJoin` enforces `max_ids` through `max_rows_in_set`, which applies to every set in the statement.
- Mutation ids are the newest mutation of the table and partition; lifecycle work is leader-only and sequential.
- Output columns are nullable wherever a value can be absent: zero-filled `TimeSeries` buckets (for `min`, `max`,
  `uniq` and pick measures) and `uniq` over a nullable column. The closed measure set has `set_merge` for the
  `groupUniqArrayArray` columns, returning an array. A set merge above the column bound is not compared with a server
  in the differential suite, because which elements a merged partial keeps is the server's choice.
- `TextSearch` tokens are lowered by the caller; the server lowers the column only. `Pred::TextMatch` is
  case-insensitive for ASCII. A histogram merge carries no zero-count keys (`sumMap` drops them). A block resent after
  its partition was dropped lands again.
- `KILL QUERY` is sent to one node, not `ON CLUSTER`.
- The window on `mcp_tool_inventory` and `mcp_deploys` applies after grouping (`max(last_seen) >= from` and
  `min(first_seen) < to`), because their time columns are aggregated.
- Search (#59): `TextSearch` takes a key order (descending), a keyset cursor and arrival bounds (`received_at <=
  as_of`). Each field kind is searched by its own plan and merged by key, and the page and cursor come from the hit
  stream. In phrase mode the snippet needle is the tokens joined by single spaces, and a phrase is verified on its
  snippets (anchored on the phrase, else on the field's start), so a phrase written with other separators is found
  only within a field's first 240 bytes. Tokens are lowered only for lowered indexes; metadata entries are compared as
  written, a metadata term needs an `=`, and a phrase over metadata means all of its entries. Mixing metadata with
  the text fields in one plan is refused (`PlanError::Tokenizers`).
- Metrics (#61): the `traces` source is compiled (one measure, `traces`, counting root heads of `trace_heads` by
  `start_time`, class `analytics_raw`, tier `core`, no dimensions or filters); `metrics/schema` lists `spans` and
  `traces`; `metrics/query` and `filter-values` responses carry `saturated` and `meta`. Not built yet: the
  `analytics_raw` fallback of the spans source over `span_core`, and filter values beyond the `span_metrics`
  dimensions. A `timezone` other than UTC is refused (`validation_failed`, code `unsupported`).
- Lists (#58): the filtered trace list filters `trace_heads` on the fields heads carry; `observation_type` and
  `metadata` keep only traces with a matching span, through a semi-join into the trace ids of `span_core` (or
  `span_payload` for metadata), at most 10,000 traces. It refuses metadata combined with `observation_type`, or
  metadata under `or`/`not` with any field but `user_id` and `session_id` (422 `validation_failed`, pointer
  `/metadata`, code `conflict`). The span list accepts metadata with `observation_type` (other conditions on
  `span_core`, metadata semi-joined by `span_id`, up to 10,000 payload rows). The sessions of a user are
  `Aggregate(trace_heads)` for that user with a non-empty `session_id`, ordered by (latest start, `session_id`)
  descending, class `list`; for sessions and users, "for the window" means the rollup rows of the window's UTC days.
  Session and user reads read the store only and answer `lagging` while the recent index holds unconfirmed spans on
  those days; their detail follows the trace-detail `not_found` rule; a filtered trace list counts the span table it
  reads toward the quarantine rule.

## 9. MCP

### 9.1 Wire attribute contract

SDKs and the proxy send OTLP spans that carry the OTel MCP semantic-convention keys plus `crucible.mcp.*` extension
keys. **A span becomes an `McpOpRecord` if and only if it has `mcp.method.name`.** A plain semantic-convention span
from a server with no Crucible wrapper still yields a row, with `capture_source = otlp`. The key registry is
`conformance/registry.json`, shared by every SDK, the proxy and the normalizer: each producer's emitted keys and
types, and the projector's mapping, are tested against it. Rust code may hold the keys as constants (for example in
`crucible-records`), as long as those tests pass.

| `McpOpRecord` field | Span or resource key | When absent |
|---|---|---|
| `side` | SpanKind (SERVER or CLIENT) | `crucible.mcp.side`; else `server` |
| `capture_source` | `crucible.mcp.capture_source` | `otlp` |
| `method` | `mcp.method.name` | (required) |
| `target_name`, `target_kind` | `gen_ai.tool.name`, `gen_ai.prompt.name`, `crucible.mcp.resource.template` | Kind from the method; name empty |
| `protocol_version`, `era` | `mcp.protocol.version`, `crucible.mcp.era` | Era is `modern` when the version is at or after `2026-07-28`, else `legacy` |
| `transport` | `crucible.mcp.transport`, else `network.transport` | `unknown` |
| `jsonrpc_id` | `jsonrpc.request.id` | Empty |
| `direction`, `is_notification`, `is_batch` | `crucible.mcp.direction`, `.is_notification`, `.is_batch` | `client_to_server`, false, false |
| `status`, `error_class`, `jsonrpc_code`, `caller_fault`, `error_type` | `error.type`, `rpc.response.status_code`, `crucible.mcp.error.class` | Classified by section 9.2 |
| `error_subclass` | `crucible.mcp.error.subclass` | `none` |
| `error_message` | span status message | Empty |
| `result_type`, `mrtr_round`, `task_id`, `subscription_id` | `crucible.mcp.result_type`, `.mrtr.round`, `.task.id`, `.subscription.id` | `complete` for a successful request, else `none`; 0; empty |
| `mrtr_state_in`, `mrtr_state_out` | `crucible.mcp.mrtr.state_in`, `.mrtr.state_out` (32 hex characters) | Absent |
| `progress_count`, `cancel_source`, `timed_out` | `crucible.mcp.progress.count`, `.cancel.source`, `.timed_out` | Absent (unobserved), `none`, false |
| `client_name`, `client_version`, `client_capabilities_hash` | `crucible.mcp.client.name`, `.client.version`, `.client.capabilities_hash` | Empty, empty, 0 |
| `oauth_client_id`, `tenant`, `user_id`, `handle_id` | `crucible.mcp.oauth.client_id`, `.tenant`, `user.id`, `crucible.mcp.handle.id` | Empty |
| `user_subject_hash`, `user_issuer` | `crucible.mcp.user.issuer` and `crucible.mcp.user.subject` (raw; the subject is hashed server-side and discarded, the issuer is kept) | Absent, empty |
| `identity_source` | `crucible.mcp.identity.source` | `none`, or `identify_hook` when `user.id` is present |
| `session_key` | `mcp.session.id` (legacy era only) | Absent |
| `request_bytes`, `response_bytes`, `content_blocks` | `crucible.mcp.request.bytes`, `.response.bytes`, `.content.blocks.<type>` | 0 |
| `args`, `result`, capture flags | `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`, `crucible.mcp.content.captured`, `.content.sample_rate`, `.content.truncation` | Not captured |
| `server_name`, `server_version` | `crucible.mcp.server.name`, `.server.version` | Resource `service.name`, `service.version` |
| `deploy_id`, `instance_id` | Resource `crucible.deploy.id`, `service.instance.id` | Section 9.7; empty |
| `transport_link_*` | First span link | Absent |
| `ip_prefix`, `user_agent`, `http_status` | `crucible.mcp.client.ip_prefix`, `user_agent.original`, `http.response.status_code` | Empty, empty, absent |

Senders must truncate IP addresses themselves and must never emit `client.address`, Authorization values,
`requestState`, cookies, `Mcp-Param-*` values or baggage. Consumed keys are removed from residual `attributes`.
Trace context travels in `params._meta` (`traceparent`, `tracestate`); SDK client wrappers inject it, and the proxy
only reads it.

### 9.2 Error taxonomy

The classifier is one total function in `crucible-records`, shared as data by the normalizer, simulation and the
SDK conformance suite. An explicit `crucible.mcp.error.class` from an SDK wins when it is consistent with the
observed channel; otherwise the first matching row applies.

| Order | Condition | `error_class` | `status` |
|---|---|---|---|
| 1 | `cancel_source` is not `none` | `cancelled` | `cancelled` |
| 2 | `timed_out` (server deadline, or no response by the deadline) | `timeout` | `transport_error` |
| 3 | Stream break, stdio EOF with the operation pending, or an unparseable message | `transport` | `transport_error` |
| 4 | HTTP 401 or 403 | `auth` | `protocol_error` |
| 5 | HTTP 429 | `rate_limit` | `protocol_error` |
| 6 | JSON-RPC code -32700, -32600, -32601, -32020, -32021 or -32022 | `protocol` | `protocol_error` |
| 7 | JSON-RPC code -32602 with the SDK `mark_not_found` annotation, or code -32002 | `not_found` | `protocol_error` |
| 8 | JSON-RPC code -32602 otherwise | `validation` | `protocol_error` |
| 9 | JSON-RPC code -32603, any other JSON-RPC error code, or an unhandled exception | `internal` | `protocol_error` |
| 10 | Result with `isError: true` and the `mark_validation` annotation | `validation` | `tool_error` |
| 11 | Result with `isError: true` and the `mark_not_found` annotation | `not_found` | `tool_error` |
| 12 | Result with `isError: true` otherwise | `tool_execution` | `tool_error` |
| 13 | Any other HTTP status of 400 or above with no JSON-RPC body | `internal` | `protocol_error` |
| 14 | None of the above | `none` | `ok` |

Row 7 classifies an unannotated -32002 as `not_found` (default; revisit).

- `error_subclass` (`auth`, `rate_limit`, `upstream`, `business`) is set only by the SDK `annotate_error` call and
  refines `tool_execution`. The proxy cannot observe subclasses or the validation and not-found annotations.
- `error_type` keeps the semantic-convention string: the JSON-RPC code as text, or `tool_error` for `isError`.
- **Caller-fault rule.** `caller_fault` is true exactly when `side = server` and the JSON-RPC code is one of
  -32700, -32600, -32601, -32602 or -32002. Error-rate measures, alert measures and widgets exclude caller-fault
  rows by default; the API exposes them separately.

### 9.3 Identity sourcing

Client name and version are always self-reported and labelled so.

| Era and transport | Client info | User identity | Where the wrapper or proxy reads it |
|---|---|---|---|
| Legacy, stdio | `initialize` request `clientInfo` and capabilities, remembered for the session | `identify` hook only | Session state |
| Legacy, HTTP | `initialize` `clientInfo`, keyed by the session id | OAuth access token (subject, issuer, client id) when the server validates one; else the `identify` hook | Session state and the request's auth info |
| Modern, stdio | Per-request `_meta` client info and capabilities | `identify` hook only | Each request |
| Modern, HTTP | Per-request `_meta` client info | OAuth access token; else the `identify` hook | Each request and its auth info |
| Proxy, stdio | As observed on the wire for the era | None | Analyser copy |
| Proxy, HTTP | As observed on the wire | Off by default. Opt-in `claims` mode decodes the token payload without verification (`identity_source = token_claims`); opt-in `verified` mode checks the signature against a configured key set (`token_verified`) | Allowlisted header view; the token itself never reaches the analyser |

- SDKs send the raw issuer and subject. Stage B computes
  `user_subject_hash = first 16 bytes of HMAC-SHA256(key = project salt, message = issuer || 0x00 || subject)` and
  discards the raw values. The salt is immutable for the life of the project. Delete-by-user accepts the raw id and
  matches both `user_id` and the hash computed for every issuer seen for the project (section 13.3).
- `identity: none` in SDK and proxy configuration suppresses every identity key.
- `identity_source` is `identify_hook` when the hook supplied any value, `token_verified` when the hosting server
  or the proxy verified the token, `token_claims` for unverified claims, else `none`.

### 9.4 Group key

"MCP session" in the API means a group of operations with one `group_key`. The first matching rule applies:

| Order | Condition | `group_key_kind` | `group_key` |
|---|---|---|---|
| 1 | Legacy era and a session id is present | `legacy_session` | `session_key` |
| 2 | `handle_id` is non-empty | `handle` | `handle_id` |
| 3 | The span has a parent (trace context was propagated) | `trace` | `trace_id` as hex |
| 4 | Otherwise | `process` | `instance_id` (empty when unknown) |

`session_key`: senders should send `mcp.session.id` already hashed as 32 lowercase hex characters (the first 16
bytes of SHA-256 of the raw id). The normalizer hashes any value that is not in that form, so a raw session id is
never stored. `session_key` is never required.

### 9.5 Double capture

An SDK wrapper and a proxy in front of one server both produce `side = server` rows for one call.

- `capture_source` is part of the `mcp_tool_rollup` key and of `mcp_deploys`.
- Every MCP analytics endpoint and every `mcp_ops` metrics source takes a `source` query parameter:
  `auto` (default), `sdk`, `proxy` (both proxy sources) or `otlp`.
- `auto` selects one source per `(server_name, side)` for the request window by preference **SDK, then proxy, then
  OTLP**: the best-ranked source that has at least one row in the window. The selection is a post-operator over
  the rollup, and the response names the chosen source per server (`capture_sources`).
- Rows from different capture sources are never summed.
- Alert rules store an explicit `source`; `auto` is resolved per evaluation.

### 9.6 Multi-round (MRTR) correlation

A wrapper that does not own `requestState` cannot mint a correlation id, and the state itself must never be
emitted. Instead each operation carries two keyed hashes:

- `mrtr_state_out`: present on an operation whose result is `input_required`; the hash of the `requestState` it
  returned.
- `mrtr_state_in`: present on an operation whose request carries a `requestState`; the hash of that value.
- Hash: the first 16 bytes of `HMAC-SHA256(key = the sender's Crucible secret key bytes, message =
  "crucible.mrtr.v1" || 0x00 || requestState bytes)`, sent as 32 hex characters (default; revisit).
- Readers chain operations within one trace (or one `group_key` when there is no shared trace): an operation whose
  `mrtr_state_in` equals another's `mrtr_state_out` continues that exchange. `mrtr_round` is the sender's count
  when it knows it, else the chain position computed at read time. Chains never cross capture sources.

### 9.7 Deploy id

`deploy_id` is the resource attribute `crucible.deploy.id` (SDK option `deploy_id`); when absent it **defaults to
`server_version`**; when that is also empty it is the empty string, shown as "unknown". `server_version` is
therefore not a rollup dimension. `mcp_deploys` (section 3.7) records every observed
`(server_name, deploy_id, server_version, capture_source)` with first and last seen.

### 9.8 Handle id

`handle_id` comes only from the SDK `identify` hook, or from a per-tool `handle_param` option that names an
existing argument whose string value is used (cut at 256 bytes, after scrubbing). Nothing is ever injected into a
tool's schema or arguments. The proxy never produces a `handle_id`.

### 9.9 Analytics endpoints and their sources

Every endpoint under `projects/{id}/mcp/` requires `side`, `from` and `to`, and accepts `source` and
`include_caller_faults` (default false). "Gate" marks the endpoints bound by the 300 ms p95 dashboard target; they
must read only the listed aggregating tables.

| Endpoint | Source tables | Class | Gate |
|---|---|---|---|
| `servers` | `mcp_tool_rollup`, `mcp_deploys` | `analytics` | yes |
| `tools` | `mcp_tool_rollup`, `mcp_tool_inventory` (unused flag, schema hash) | `analytics` | yes |
| `tools/{name}` | `mcp_tool_rollup`, `mcp_deploys` (deploy markers) | `analytics` | yes |
| `errors` | `mcp_tool_rollup` for the class distribution; the breakdown by subclass, JSON-RPC code and message is a separate section from `Aggregate(mcp_ops)` in `analytics_raw` (default; revisit) | both | class distribution only |
| `clients` | `mcp_tool_rollup` by `client_name`; the per-version breakdown is a separate section from `Aggregate(mcp_ops)` (default; revisit) | both | by name only |
| `users` | `Aggregate(mcp_ops)` by `user_subject_hash` or `user_id` | `analytics_raw` | no |
| `sessions` | `Aggregate(mcp_ops)` by `group_key`, with `group_key_kind` | `analytics_raw` | no |
| `operations` | `Scan(mcp_ops)` | `list` | no |
| `operations/{trace_id}/{span_id}` | `PointLookup(mcp_ops)`, `PointLookup(mcp_content)`, and the other side's row in that trace when present | `point` | no |
| `deploys` | `mcp_deploys` | `analytics` | yes |
| `compare` | `mcp_tool_rollup` for two deploys or two windows (per-tool deltas of calls, error rate, percentiles) and `mcp_tool_inventory` (tools added, removed, or with a changed schema hash) | `analytics` | yes |

Alert measures over `mcp_ops`: `mcp.calls`, `mcp.error_rate` (caller faults excluded), `mcp.error_count`,
`mcp.duration_p50`, `mcp.duration_p90`, `mcp.duration_p99`.

### 9.10 Capture rules for SDKs and the proxy

| Rule | Contract |
|---|---|
| Defaults | MCP metadata is always captured. Arguments and results are opt-in per server or tool glob, sampled before any copy, cut at 16 KiB per field by default, and scrubbed in the sender |
| Scrub rules | Data in `conformance/scrub-rules.json` with test vectors, shared by the four SDKs and the proxy; patterns stay in the linear-time regular-expression subset |
| Export | Off the request path through a queue bounded by spans and by bytes (defaults 2,048 spans and 8 MiB). The byte bound counts the batch being exported and estimates each span from the lengths of its name, attributes, events and status message; a span beyond either bound is dropped and counted, never blocks. Whether the span bound counts the in-flight batch is left to each SDK (default; revisit) |
| Proxy passthrough | Bytes are forwarded unchanged and immediately in both directions; analysis runs on a copy through a bounded non-blocking channel; the proxy never injects; telemetry buffer 8 MiB or 10,000 operations, drop-oldest; logs go to stderr only |
| Proxy correlation | Pending operations are keyed by (direction, JSON-RPC id); operations pending at child exit become `transport` errors |
| Shipping | OTLP HTTP protobuf to `/v1/traces` with a project key holding the `ingest` scope |
| SDK targets | The MCP server wrappers target the 2.x lines of the official SDKs; servers on 1.x use the proxy |

The Crucible MCP server (`crucible-mcp-server`, at `/mcp`, both eras): Bearer project secret key (Basic also
accepted) with the `read` scope builds the `ProjectScope`; no tool takes a project argument; tools are read-only
plus `create_export` and `get_export`; every telemetry tool requires `from` and `to`; previews only; any result is
capped at 64 KiB with `truncated` and `next_cursor`; guardrail problem codes return `isError: true` with the stable
code; a SQL tool exists only for keys with the `sql` scope.

### 9.11 Settled in code

- MCP storage (#79): the tool inventory key is `crucible.mcp.tools` (JSON text, registry type string); the normalizer
  computes the schema hash. An explicit error class of `transport` or `timeout` is the capture source's observation;
  a numeric `error.type` equal to the HTTP status is not read as a JSON-RPC code. Content without the captured flag
  counts as captured when arguments or a result are present; a content block type is cut at 64 bytes. The normalizer
  version is 5.

## 10. Compat dialect registry

`compat_dialect` is a `u8`. **Product names, attribute key lists, path strings, header names and fixtures for
dialects 1 to 7 live only in `crates/crucible-compat`**, one data table per dialect, compiled in and validated by
tests. This document describes dialects generically on purpose.

### 10.1 Registry

| Id | Description | Detected by |
|---|---|---|
| 0 | Native: the OTel GenAI and MCP semantic conventions and `crucible.*` keys | Default |
| 1 | A third-party tracing SDK's OTLP traffic on its compatibility path, with Basic auth carrying Crucible keys | The compatibility path (request hint), its instrumentation scope name, its type key, or its attribute key prefix |
| 2 | An open instrumentation convention identified by a span-kind attribute | That attribute |
| 3 | An open instrumentation library family identified by its attribute namespace | Keys in that namespace |
| 4 | A web framework's AI SDK telemetry identified by an operation-id attribute | That attribute |
| 5 | An ML lifecycle tool's tracing attributes identified by their namespace | Keys in that namespace |
| 6 | An agent framework's instrumentation identified by its scope name | Scope name |
| 7 | A third-party evaluation product's OTLP attribute namespace, on an aliased path with Bearer auth | The alias path or routing header (request hint), or one of its content keys |

Ids are stable and never reused. A new dialect appends an id by contract change.

Rules for every dialect: the project always comes from the credential, so routing headers and routing
attributes are accepted and ignored for routing; keys the tables do not consume stay in residual `attributes`;
a media reference token is recognized, kept as opaque text in every stored field and replaced by `[media]` in
previews (section 14). Detection rules carry a confidence on one scale: 100 the dialect's own scope name, 90 a
key only that dialect writes, 70 likely, 60 the request hint, 40 weak. Keys that an exporter stamps on every
span it forwards are never detection rules.

Media-token handling is recognition plus preview replacement only. Stored bodies are never rewritten: they keep
the token exactly as sent, so the sending SDK's own media round trip keeps working.

### 10.2 The pure map and the traits

```text
map(frame, envelope, normalizer_version) -> MappedFrame          // pure: no clock, randomness or IO
enrich(mapped, snapshots { salt, price table, tombstones, caps }) -> NormalizedFrame   // deterministic given snapshots

trait SpanMapper {
    fn dialect(&self) -> u8;
    fn detect(&self, resource, scope, span, hints) -> Option<Confidence>;
    fn map(&self, ctx: &MapCtx, span: &SpanView, out: &mut SpanDraft);
}
trait McpProjector {
    fn project(&self, draft: &SpanDraft, span: &SpanView) -> Option<McpOpDraft>;
    fn inventory(&self, draft: &SpanDraft, span: &SpanView) -> Vec<InventoryDraft>;
}
```

- `map` is the single mapping engine. It is table-driven: the engine lives in `crucible-normalize` and holds no
  product-specific string; the dialect tables are data from `crucible-compat`.
- Stage A depends only on the two traits. Their one production implementation (`TableMapper`, in
  `crucible-normalize`) is a thin wrapper over `map`: `detect` evaluates a table's detection rules, `map` runs the
  engine for one span, and `McpProjector` applies section 9.1. Test doubles may implement the traits; no second
  production implementation exists in v0.
- Dialect choice per span: the dialect with the highest detection confidence, ties going to the lowest id;
  default 0. A request-level hint in the envelope counts as a detection of its dialect at confidence 60, so a
  span that another dialect's own scope name or exclusive key identifies keeps that dialect on a hinted path.
  `source_format` is decided per span.
- The dialect tables are layers in the section 10.3 order, and the engine maps every span with all of them. A
  source is scoped `any` (read on every span) or `own` (read only when its table's dialect is the chosen
  dialect; used for bare, un-namespaced keys). The chosen dialect decides `compat_dialect`, which `own` sources
  are live, and the table's value encoding; it never disables another table's `any` sources.
- A table may declare the value encoding `json text`: on a span of that dialect, a string attribute that is
  JSON text of one scalar is unwrapped before any scalar decoder or type rule reads it. Bodies and maps are
  never unwrapped.
- The duplicate window and first-seen set are stage B state, not mapper state.

### 10.3 Field priority

Each target field has an ordered list of sources `(dialect, location, key or key prefix or event name, optional
JSON member path, decoder, scope)`. Decoders: string, string-or-integer, integer, boolean, JSON, JSON-or-string,
JSON object, JSON string array, timestamp, offset from span start (milliseconds or seconds), event time,
decimal USD, level map, tag list, prefix-flatten to map, map member, usage object and usage prefix (with an
alias table), cost object, indexed messages, indexed list, tool-call reshape. **The first non-empty source
wins**, with three exceptions: map fields (`metadata`, `trace_metadata`, `model_parameters`) merge every source
and the higher-priority source wins a member clash; usage and provided cost are resolved per key; one attribute
may feed several fields and is consumed once. A source with a member path does not consume its attribute.
Sources are ordered by this dialect priority:

1. `crucible.*` keys
2. dialect 1
3. dialect 7
4. dialect 0: OTel GenAI attributes, then GenAI span events
5. dialect 2
6. dialect 4
7. dialect 3
8. dialect 5
9. dialect 6

A table may repeat a higher- or lower-ranked dialect's key in its own list, for the same field only, to fix the
order among its own keys.

### 10.4 Observation-type priority

The first rule that yields a value wins:

1. explicit `crucible.observation.type`
2. dialect 1's type key
3. dialect 7's span type (its flattened key, then the member of its span-attributes object, then its scorer
   purpose)
4. `mcp.method.name` (`tools/call` maps to `tool`)
5. dialect 2's span-kind attribute
6. `gen_ai.operation.name` with a semantic-convention value (chat, text completion and content generation map
   to `generation`; embeddings to `embedding`; `execute_tool` to `tool`; agent operations to `agent`)
7. dialect 4's operation
8. dialect 3's span kind, then its operation-name values, then its request type
9. dialect 5's span type, then its operation-name values
10. dialect 6's operation-name values
11. `span`

A type rule may read a member of a JSON object held in a string attribute. Operation-name values outside the
semantic conventions are listed by the table of the dialect that emits them and rank at that dialect's
position; the value lists of one key are disjoint across tables.

Mapping must be total (never panics), idempotent, and identical for the protobuf and JSON encodings of one request.

### 10.5 Settled in code

| Topic | Rule |
|---|---|
| Interface | The normalizer takes its own `IngestFrame` (section 1.5 forbids a dependency on the WAL codec) and returns a `Result`. `SpanMapper` and `McpProjector` live in `crucible-normalize`, because they take `SpanView` |
| Dialect 0 sources | Only keys the contract or the semantic conventions name; fields with no named key (trace name, session, tags, metadata, public flag, environment, release, version, level, provided cost, prompt version) stay empty until milestone 2 names their keys |
| Usage parts | `cache_read` and `cache_write` count inside input tokens and `reasoning` inside output tokens; pricing treats them as parts, not additions. Without a provided total, total is input plus output |
| `row_digest` | Version 1: xxh3-64 over a canonical encoding of both tiers, excluding the ingestion group, `event_bytes` and `api_key_id`. `clock_skew` and forward-filled values are included, so a retry that crosses the 24-hour boundary or splits a trace differently is kept |
| Residual JSON | Wire order, duplicates kept; integers as numbers; NaN and infinities as strings; bytes as lowercase hex; events capped at 128 and links at 32 (not listed in `truncated_fields`, because the JSON stays valid) |
| Dropped in milestone 1 | Instrumentation-scope attributes (name and version are kept), link `trace_state`, span flags, dropped counts and schema URLs |
| Stage B | A changed repeat moves its key to the newest position; the first-seen set is bounded at 4,000,000 traces; the provisional head is the first kept span, in wire order, of a trace first seen in a frame without its root; an out-of-order LSN is refused |
| Layers | `crucible.*` keys form their own table at the first position; the tables are applied as layers `[crucible keys, 1, 7, 0, 2, 4, 3, 5, 6]`; a candidate's rank is (layer, source position) |
| Consumption | An attribute leaves the residual when a winning source read its whole value. A family decoder (indexed, prefix) consumes the keys it used; keys it skipped stay. Losing candidates and member-path sources never consume |
| `usage_provided` keys | The six normalized keys, plus other provided keys under the name the dialect table assigns, else the name as sent. A dialect whose buckets are disjoint is converted to the inclusive rule by its usage table; that is a conversion, not an estimate |
| Indexed messages | Stored as the un-flattened JSON array (ascending index, members in wire order, wrapper segments removed), `io_shape = indexed_messages`. Keys with a non-numeric index segment are not part of the family |
| Derived tool fields | When no source fills `tool_calls`, they are read from an `indexed_messages` output by the table's tool-call shape; `tool_names` and `tool_call_count` derive from `tool_calls` unless a source fills `tool_names`. `finish_reasons` falls back to the per-element finish reason of an indexed output |
| Offsets | A start-relative duration becomes `start_time + value`, rounded half to even to the microsecond and clamped to `end_time` |
| Media in previews | A dialect's media reference token is replaced by `[media]` in `input_preview` and `output_preview`; stored fields, sizes and hashes use the original bytes |
| Compat SDK identity | On a compat route, `sdk_name` and `sdk_version` come from that route's SDK headers when present, else from the first `User-Agent` product |
| Not mapped in v0 | Conditional sources, span-name rules, cross-span usage correction (a span's own usage is never rewritten; `usage_aggregate` in 3.1 only keeps a repeated count out of sums), and extraction of tools, system instructions or models from `raw` bodies |

### 10.6 Source keys for fields without a semantic convention

Crucible SDKs (milestone 3) and hand instrumentation set these keys; dialect 0 reads them, and they take priority
1 in section 10.3. Where an OpenTelemetry semantic convention exists, it is the second source, so unmodified OTel
applications fill the field too. These names are wire contract: renaming one is a breaking change.

| Field | Sources, in priority order |
|---|---|
| `trace_name` | `crucible.trace.name`; when no span of the trace sets it, displays use the root span's `name` |
| `session_id` | `crucible.session.id`, `session.id`, `gen_ai.conversation.id` (string, or an integer rendered in decimal) |
| `user_id` | `crucible.user.id`, `user.id` |
| `environment` | `crucible.environment`, resource `deployment.environment.name`, resource `deployment.environment` |
| `release` | `crucible.release`, resource `service.version` |
| `version` | `crucible.version` |
| `tags` | `crucible.tags` (string array) |
| `metadata` | `crucible.metadata.*` (prefix-flattened to a map) |
| `level` | `crucible.level` (`debug`, `default`, `warning`, `error`) |
| `trace_tags`, `trace_metadata`, `trace_public` | `crucible.trace.tags`, `crucible.trace.metadata.*`, `crucible.trace.public` (bool) |
| `input`, `output` (non-GenAI spans) | `crucible.input`, `crucible.output` (JSON or string); GenAI spans keep the message keys of 10.3; tool spans without messages take `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` |
| `prompt_name`, `prompt_version` | `crucible.prompt.name`, `crucible.prompt.version` |
| Provided cost | `crucible.cost.total` and `crucible.cost.<usage key>`, in USD as a decimal string or a double |
| `completion_start_time` | `crucible.completion_start_time` (RFC 3339 string or Unix nanoseconds) |
| `model_parameters` | `crucible.model_parameters.*`, then every `gen_ai.request.*` key other than the model under its suffix name (temperature, top_p, max_tokens and the rest), then `openai.request.service_tier` as `service_tier`. Dialect tables use the same member names |
| `provider` | `gen_ai.provider.name`, then the deprecated `gen_ai.system` |
| `tool_names` | `gen_ai.tool.name` on tool spans; otherwise derived from `tool_calls` |
| `is_root`, `is_app_root` | No native key in v0; dialect tables may set them |

Trace-level fields follow 3.3: never backfilled, forward-filled within one request, picked by rank across
requests. The `crucible.*` keys are read on every span, whatever dialect is chosen.

## 11. Control plane

Postgres (floor 15) behind `ControlStore`: traits and domain types in `crucible-control`, the adapter in
`crucible-control-pg`. One organization per deployment, unlimited projects.

### 11.1 Tables

| Table | Key columns | Notes |
|---|---|---|
| `org` | singleton `id`, `name`, `settings`, `version` | Settings: `signup_mode`, `allowed_email_domains`, `password_login_enabled`, `project_access` |
| `users` | `id`, `email` (unique, case-folded), `name`, `status`, `email_verified_at` | |
| `credentials` | `user_id`, Argon2id PHC string, `params_version`, `failed_count`, `locked_until` | |
| `sessions` | SHA-256 of the 256-bit token (pk), `user_id`, `csrf_token`, `idle_expires_at`, `absolute_expires_at`, `auth_method`, `revoked_at` | The raw token is never stored |
| `auth_providers` | `id`, `kind` (github, google, oidc), `issuer`, `client_id`, `secret_id`, `scopes`, `allowed_domains`, `enabled` | Rows seeded from config are read-only in the UI |
| `oauth_accounts` | (`provider_id`, `subject`) unique, `user_id` | |
| `auth_flows` | state hash (pk), `provider_id`, encrypted PKCE verifier, `nonce`, `next_path`, `expires_at` (10 min), `consumed_at` | |
| `one_time_tokens` | token hash (pk), `kind` (password_reset, invite, setup), `subject`, `expires_at`, `consumed_at` | |
| `projects` | `id`, `name`, `slug`, `settings`, `retention_days`, `payload_retention_days`, `mcp_content_retention_days`, `config_version`, `version`, `deleted_at` | `config_version` increases on every change that the normalizer snapshots |
| `org_memberships`, `project_memberships` | `user_id` (and `project_id`), `role` | |
| `invitations` | `id`, `email`, `org_role`, `project_roles`, `expires_at` (7 days), `accepted_at`, `revoked_at` | |
| `api_keys` | `id`, `project_id`, `public_key` (unique), `secret_salt`, `secret_hash`, `lookup_tag`, `scopes`, `import`, `note`, `expires_at`, `last_used_at`, `revoked_at`, `version` | Section 11.3 |
| `key_feed` | `seq` (pk, sequence), `key_id`, `change`, `key_version`, `at` | Written in the transaction that changes the key |
| `secrets` | `id`, `scope`, `kind`, `key_id`, `nonce`, `ciphertext` | AAD binds id, kind and scope |
| `project_salts` | `project_id`, encrypted salt | Immutable for the project's life: 32 random bytes sealed with the project id as associated data, inserted if absent and re-read, never replaced. Until salts are created with the project, a frame carrying a subject for a project whose salt was never loaded waits for it (a Postgres outage holds the shipper); without a control store the identity keys are dropped |
| `signing_keys` | `kid`, `purpose` (cursor, download, media_token), encrypted key, `state` (active, verify_only, retired) | |
| `channels` | `id`, `project_id`, `kind` (slack, email, webhook), `config`, `secret_id`, `enabled`, `last_test`, `version` | |
| `alert_rules`, `alert_state`, `alert_events` | rule; (`rule_id`, `series_key`); history | |
| `notification_outbox` | `id`, `dedup_key` (unique), `payload`, `attempts`, `next_attempt_at`, `status` | Written in the transaction of the state change |
| `dashboards`, `widgets` | `id`, `project_id`, `version`; widget: `dashboard_id`, `query`, `display`, `layout` | Built-in dashboards are code, not rows |
| `saved_queries`, `sql_runs` | saved query: `id`, `project_id`, `owner_id`, `visibility`, `sql`, `parameters`, `version`; run: `id`, `project_id`, `user_id`, `query_id`, outcome, statistics | `sql_runs` kept 30 days |
| `deletion_requests`, `deletion_node_acks` | request: `id`, `project_id`, `kind`, `selector`, `state`, `requested_at`, `deletion_seq` (sequence), per-place status, attempts, counts; ack: (`request_id`, `wal_epoch`), `tombstone_lsn`, `passed_at` | Section 13.3 |
| `comments` | `id`, `project_id`, target, `version`, `body`, `mentions`, `anchor_path`, `edited_from`, `retracted`, `author_id`, `created_at` | Versioned append-only rows |
| `price_table_versions`, `model_prices` | version: `version` (pk), `digest`, `created_at`; price: `id`, `project_id`, match pattern, start date, prices, tiers, `added_in_version`, `removed_in_version` | Price rows are immutable; a change adds rows and a new version. Bundled defaults are compiled in |
| `export_jobs` | `id`, `project_id`, `format`, `selector`, `state`, `attempt`, `lease_until`, file reference, `checksum`, `as_of`, `expires_at` | |
| `media_objects`, `media_refs` | Section 14 | Used only when object storage is configured |
| `nodes` | `wal_epoch` (pk), `heartbeat_at`, `version`, `checkpoint_lsn`, `address` | Node registry |
| `job_leases`, `job_cursors` | `name` (pk), `holder`, `fencing_token`, `expires_at`; `name`, `cursor` | Section 11.7 |
| `ch_sql_identities` | `project_id`, `generation`, `state`, `last_probe`, `last_used_at` | Section 7.4 |
| `schema_migrations` | `id`, `kind` (postgres, clickhouse_schema, clickhouse_access), `applied_at`, `checksum` | Forward-only |
| `idempotency_keys` | (`principal`, `route`, `key`), request hash, response, `expires_at` (24 h) | |

### 11.2 Secrets at rest

A 32-byte master key comes from `CRUCIBLE_MASTER_KEY` or its `_FILE` form, with a ring of at most two keys for
rotation. HKDF-SHA256 gives one subkey per purpose. Secrets are sealed with XChaCha20-Poly1305 and a random nonce
from `Rng`. Cursor-signing keys are random, stored sealed, rotated every 30 days, and kept `verify_only` for 24 h
(the cursor lifetime). Until the `signing_keys` rows and their rotation job exist (milestone 4), the cursor key is the
master-key subkey with HKDF info `crucible/cursor-signing/v1/0` and key id `master-0`; a node without a master key
(ingest-only mode) uses a random per-process key, so its cursors reset on restart and clients restart from page one.

### 11.3 API keys

| Aspect | Contract |
|---|---|
| Format | Public key `cpk_` and secret key `csk_`, each followed by 32 random bytes in base62 (alphabet `0-9A-Za-z`, fixed 43 characters, left-padded with `0`) |
| Storage | `secret_salt` (16 random bytes) and `secret_hash = SHA-256(secret_salt || secret key string)`. Secrets are 256-bit random, so a fast hash is correct. The secret is shown once |
| `lookup_tag` | The first 8 bytes of `HMAC-SHA256(lookup subkey, secret key string)`; lets a Bearer secret find its row without the public key |
| Verification | From the in-memory snapshot only: parse, map lookup, hash, constant-time compare. No Postgres call and no allocation on the hot path |
| Principal | A key is a project principal. It can never manage keys, members or settings |
| Expiry | Checked locally against `Clock`, also during a Postgres outage |
| `last_used_at` | Updated in 60 s batches; skipped during an outage |
| `import` flag | Set only by `crucible admin create-key --import`; allows the event-age bypass of section 1.2 |

**Scopes.** New secret keys default to `ingest`, `read`, `write`.

| Scope | Allows |
|---|---|
| `ingest` | OTLP and compat ingest; media upload and confirm |
| `read` | Every telemetry and analytics read (including POST reads), exports, the MCP server |
| `write` | Feedback, tags, flags, corrections, comments |
| `delete` | Deletion requests of kind trace, user and session |
| `sql` | SQL workspace runs and the MCP server's SQL tool |

A public key used as a Bearer token is accepted on `POST projects/{id}/feedback` only (body cap 16 KiB; that route
alone answers cross-origin requests, without credentials; a project setting can turn it off).

### 11.4 Sessions and CSRF

| Aspect | Contract |
|---|---|
| Cookie | HttpOnly, SameSite=Lax, Path=/ always. When the public origin is HTTPS the cookie is `__Host-crucible_session` with Secure. When it is plain HTTP (common on internal networks) the cookie is `crucible_session` without Secure, sign-in works, and the server logs one startup notice recommending HTTPS; `security.require_https = true` makes plain HTTP refuse sign-in instead |
| Token | 256 random bits from `Rng`; only its SHA-256 is stored |
| Expiry | Idle 7 days (sliding; writes throttled to once per 60 s); absolute 30 days |
| Rotation | A new token at every login. A password or role change revokes the user's other sessions |
| Fixation | Pre-login state lives in a separate flow cookie (`__Host-crucible_flow` over HTTPS, `crucible_flow` over plain HTTP); no pre-auth identifier ever becomes a session |
| Cache | Nodes cache sessions and memberships for 5 s. During a Postgres outage, sessions and memberships already in the cache stay valid for `auth.session_outage_grace` (default 10 min), so open UIs keep reading telemetry; new sign-ins and control-plane writes answer `control_plane_unavailable`. A revocation made before the outage still applies, because the cache refreshes every 5 s while Postgres is up. `auth.session_outage_grace = 0` restores fail-closed |
| Passwords | Argon2id, 64 MiB, 3 iterations, 1 lane, on `Spawner::spawn_blocking` behind a semaphore of min(cores, 4); overflow is 429. Unknown emails get a dummy verify. 12 to 128 characters |

| Request | Rule |
|---|---|
| Cookie-authenticated, any method other than GET or HEAD (including POST reads such as `search` and `metrics/query`) | `X-Crucible-CSRF` must equal the session's token (constant-time compare). `Origin` must equal the server's own origin: `server.public_url` when set, otherwise the request's `Host` (same-origin), or one of `server.allowed_origins`. `Sec-Fetch-Site`, when present, must be `same-origin`. Failures are `csrf_invalid` or `origin_invalid` |
| Pre-session POSTs (login, reset request and confirm, invitation accept, first-owner setup) | The `Origin` and `Sec-Fetch-Site` checks, plus `X-Crucible-CSRF: prelogin` |
| `Authorization` header present | Key auth only. The cookie is ignored; there is no fallback. Exempt from CSRF |
| OAuth and OIDC callbacks (GET) | Protected by state, the flow cookie, PKCE (S256) and nonce |
| Ingest and compat paths | Never read cookies |

`next` after sign-in must start with a single `/`, must not start with `//` or `/\`, must hold no scheme, host or
control character, and must not be under `/api/`; otherwise it becomes `/`. Sign-in failures redirect to
`/login/?error=<code>` with one of `oauth_denied`, `oauth_state_invalid`, `oauth_exchange_failed`,
`email_unverified`, `not_invited`, `account_disabled`, `provider_disabled`, `sso_required`.

### 11.5 Sign-up mode

Sign-up is **invite-only**. A user can be created only by accepting an unexpired invitation, by first-owner setup
(`crucible admin init` or the one-time setup token), or through the allowed-domain rule: when
`org.settings.allowed_email_domains` is non-empty, a person whose email is verified (by the sign-in provider, or by
a one-time link) and whose domain is listed may join without an invitation, with org role `viewer` (default;
revisit). There is no open sign-up. A first provider login attaches to an existing or invited user only when the
provider asserts a verified email.

### 11.6 Roles, project access and the capability matrix

- Org roles: `owner`, `admin`, `member`, `viewer`. Project roles: `admin`, `member`, `viewer`.
- Org owner and admin are project admin on every project.
- `org.settings.project_access`: `all` (default) gives org members and viewers their org role on every project;
  `explicit` gives them access only through a project membership.
- The effective project role is the higher of the org-given and the project-given role.

| Project capability | Admin | Member | Viewer |
|---|---|---|---|
| Read telemetry, MCP analytics, dashboards, alerts, saved queries | yes | yes | yes |
| Write tags, flags, comments, feedback, corrections | yes | yes | no |
| Create and edit dashboards, saved queries, alert rules | yes | yes | no |
| Run ad hoc SQL; create exports | yes | yes | no |
| SQL workspace routes (#73) | `GET sql/schema`, `runs`, `saved-queries`: `read`; `POST sql/run` and `POST sql/export`: `run_sql` (export takes query text, so it needs `run_sql`, not `export`); running a saved query and cancelling a run: `run_saved_query`; saved-query create, edit, delete: `edit_content`. A key acts as a member with the `sql` scope and as a viewer without it; no key saves a query. The permission check runs before body validation | | |
| Run a project-visible saved query (declared parameters only) | yes | yes | yes |
| Channels (they hold secrets), test-send | yes | no | no |
| API keys, project settings, retention, model prices, project members | yes | no | no |
| Delete traces | yes | yes | no |
| Delete by user or session | yes | no | no |

| Org capability | Owner | Admin | Member, viewer |
|---|---|---|---|
| Org settings, sign-in providers, password-login switch | yes | yes | no |
| Members and invitations | yes | yes (not owners) | list only |
| Create or delete projects | yes | yes | no |
| Transfer ownership | yes | no | no |

**Permission extractor.** Handlers take `Scoped<P>` where `P` is a permission marker. The extractor resolves the
principal, parses `{project_id}`, computes the effective role and checks `P`. It answers 404 (`not_found`) when the
caller cannot see the project, including a key whose project differs from the path, and 403 (`forbidden`) when the
project is visible but the role or scope is too low. It is the only producer of `ProjectScope`. A test walks every
route and fails if a route has neither a permission marker nor an entry on the public allowlist;
`cargo xtask authz-matrix` generates the documentation table from the markers.

### 11.7 Leases with fencing tokens

Leader-only work is coordinated by lease rows, never by session advisory locks.

| Aspect | Contract |
|---|---|
| Row | `job_leases(name, holder, fencing_token, expires_at)` |
| Acquire | One statement: succeeds when no row exists or `expires_at` is before the database clock; sets `holder`, increments `fencing_token`, sets `expires_at = db_now + ttl` |
| TTL and renewal | TTL 15 s; renew every 5 s; renew is a compare-and-set on `(holder, fencing_token)` |
| Local validity | The holder treats its lease as valid only until its local acquire or renew time plus TTL minus 2 s |
| Fencing | Every leader write to Postgres is a compare-and-set that fails if the lease's `fencing_token` has changed. Leader actions on ClickHouse are idempotent and are recorded in Postgres under the token before they are issued |
| Lease names | `migrations`, `lifecycle` (retention, deletion leader steps, mask scheduler, marker purge, rollup rebuild), `sql_identities`, `media_gc`, `signing_keys`, `housekeeping` |
| Claimed work | Alert evaluations, the notification outbox, deletion steps and export jobs use row claims with `lease_until` and an attempt counter, not leases. A claim lasts 60 s; a failed run is retried after a backoff of 5 s doubling to 1 h (less up to a tenth of jitter), computed on the database clock from the claim's `lease_until`; after 12 attempts (a per-queue default) the row is parked (`status = parked`, `Claims::park`) and logged, never deleted. Handlers are idempotent |
| Job state | A job's cursor (`job_cursors`) is keyed by its lease name, so jobs that share a lease (for example under `lifecycle`) share one cursor. `crucible-jobs` holds the registry: per-node timers, leader jobs under a lease name and claimed-work consumers, each added by one registration line (`docs/architecture/adding-a-job.md`). Without Postgres, leader and claimed jobs do not run; this is logged once and readiness is unaffected |
| Housekeeping | The `housekeeping` lease job runs every 60 s and prunes `key_feed` rows older than the poll overlap window plus the reconcile interval (75 s) and `sessions` rows more than 10 min past their absolute expiry, each in its own fenced transaction |
| Shutdown | A leader run in flight may finish within 5 s, then its lease is released; a run that has to be cut leaves its lease to expire, because one of its actions may still be on its way; an acquisition in flight at shutdown is awaited briefly and released. Claimed rows simply expire |

### 11.8 Key feed and revocation

| Step | Contract |
|---|---|
| Write | Every key create, change, revoke or expiry edit increments `api_keys.version` and inserts a `key_feed` row in that transaction |
| Poll | Each node polls `key_feed` every 1 s (jitter from `Rng`) on the reserved pool with a 2 s timeout |
| Overlap | Sequence numbers can commit out of order, so each poll re-reads an overlap window that covers the 10 s transaction cap, and applies rows idempotently by per-key `version` (a lower version never overwrites a higher one) |
| Reconcile | A full snapshot reload every 60 s |
| Wake-up | NOTIFY may wake the poller early; no bound depends on it |
| Local revocation | A revocation made through a node applies on that node before the response returns |
| Snapshot | Immutable, swapped atomically, never persisted to disk |
| Bound | With Postgres reachable: 1 s interval plus 2 s timeout plus one retry, under 5 s |

**Outage behaviour** (`auth.pg_outage_mode`):

| Mode | Behaviour |
|---|---|
| `fail_open` (default) | Keys in the snapshot keep working for ingest. Local expiry still applies. A key that is not in the snapshot gets 503 with `Retry-After`, because the node cannot tell a new key from an invalid one. `auth.fail_open_max` optionally caps the duration (default unlimited); after it the node behaves as `fail_closed` |
| `fail_closed` | After 5 s without a successful poll, secret-key requests get 503 |
| Cold start with Postgres unreachable | No snapshot exists: the node is not ready and accepts nothing |

Fail-open applies to ingest only. Readiness (`/readyz`) requires a writable WAL and a loaded key snapshot; a stale
snapshot under `fail_open` is still ready, and the snapshot age is exported as a metric and a health field. The
resulting guarantee is invariant I10.

### 11.9 Rate limits

A GCRA limiter on `Clock` in `crucible-auth`, per node. Client IP comes from `X-Forwarded-For` only for
`server.trusted_proxies`.

| Scope | Default |
|---|---|
| Login | 5 per minute per (IP prefix, email); 20 per minute per IP |
| Reset request | 3 per hour per email; 10 per hour per IP |
| Reset confirm, invitation accept | 10 per minute per IP |
| API reads per key or session | 50 per second, burst 200 |
| Public-key feedback | 10 per second per key; 2 per second per (key, IP prefix) |
| SQL runs | 1 concurrent per user, 4 per project, 30 per minute per user |
| Exports | 5 per hour per project |
| Channel test-send | 6 per minute per channel |
| Ingest | Not rate-limited; backpressure governs (section 4.8). Optional per-key byte rate |

## 12. API

### 12.1 Base path, ports and versioning

| Listener | Serves |
|---|---|
| `:8080` | REST API under `/api/v1`; the UI at `/`; the MCP server at `/mcp`; OTLP HTTP at `/v1/traces`; compat ingest routes; `/healthz`, `/readyz` |
| `:4318` | OTLP HTTP at `/v1/traces`; compat ingest routes |
| `:4317` | OTLP gRPC `opentelemetry.proto.collector.trace.v1.TraceService/Export` |
| `127.0.0.1:9464` | `/metrics` (Prometheus) |

- `/api/v1` is additive only. Breaking changes go to `/api/v2`. `Deprecation` and `Sunset` headers mark
  retirements. `info.version` in the OpenAPI document is the binary's version.
- The OpenAPI 3.1 document is generated from the handlers (`crucible openapi`), committed at
  `specs/openapi/crucible-v1.json`, and checked for drift and for `openapi: 3.1.x` in CI.
- Paths under `/api/`, `/v1/`, `/mcp` and the health paths never fall back to HTML; unknown paths there return
  problem 404.
- Compat ingest routes are listed in `crates/crucible-compat` (path, dialect hint, accepted auth forms, SDK
  identity headers). Both ingest listeners serve them. A compat route takes the same bodies as `/v1/traces`
  (`application/x-protobuf` or `application/json`, optionally gzip), has the same limits, admission and ack
  rule, answers with the OTLP response and refusal bodies of `/v1/traces`, and sets the envelope's
  `compat_dialect` hint. An empty export answers 200.
- A dialect's routing header on `/v1/traces` sets the same hint. Routing headers never select a project.
- Compat routes never fall back to HTML: an unknown path under a compat route's first segment answers problem
  404, as do that dialect's non-trace signal paths (logs), which v0 does not ingest.
- The compat media endpoints of section 14.3 live under the same route prefix as their dialect's ingest route.
- `server.public_url` is needed only where an absolute URL leaves the server: OAuth and OIDC redirect URIs and links in email. Without it, origin checks and cookie flags follow the request (same-origin; HTTPS when the request arrived over TLS or a trusted proxy says so), and the startup log says which features need it.
- Middleware order, outermost first: request id, trace span, panic catcher, security headers, body limit, `Clock`
  timeout, test fault (debug builds only, header `X-Crucible-Test-Fault`), IP rate limit, principal resolution,
  CSRF and origin, principal rate limit, handler.
- Request id: UUIDv7. An inbound `X-Request-Id` is kept only from a trusted proxy and only if it matches
  `[A-Za-z0-9_-]{1,64}`. It is echoed as `X-Request-Id` and in every problem body.
- `crucible serve` flags so far: `--listen` (API and UI, default `0.0.0.0:8080`), `--metrics-listen` (default
  `127.0.0.1:9464`), `--data-dir` (holds the `wal`, `quarantine` and `exports` roots), `--wal-backend`
  (`auto`, `portable`, `uring`), `--ui-dir` (serve a UI build from a directory instead of the embedded one),
  `--ingest-listen` (default `0.0.0.0:4318`) and `--keys-file` (TOML `[[project_key]]` entries with `project_id`,
  `public_key` and `secret`; error messages never echo the file). The keys file is the mode without Postgres
  (ingest and the API on file keys, no sign-in); with Postgres configured, keys live in the control plane and a keys
  file is refused with a message that says how to fix it. `crucible admin create-key --project <slug|id>
  [--scopes ...] [--note ...] [--import]` creates a key from the command line; running servers see it within 5 s
  (the key feed, 11.8).
- Refusals on `/v1/traces` use the OTLP-specified `google.rpc.Status` body (the protobuf form carries the 4.8 gRPC
  code), not problem+json. `sdk_name` and `sdk_version` come from the first `User-Agent` product; on a compat route
  that declares SDK identity headers, from those headers when present.
- With ClickHouse configured, `serve` also takes `--clickhouse-url`, `--clickhouse-user`, `--clickhouse-password-file`,
  `--master-key-file`, `--migrate` and `--recent-max-bytes` (the first four also read `CRUCIBLE_*` variables; the
  secrets have no value-taking flag). Listeners bind first with ingest closed (503 with `Retry-After`, `/readyz` gives
  the reason) until migration (when asked), `detect`, `dedup_selftest`, the recent-index rebuild through the shipper's
  replay, quarantine restore and shipper start have finished. Without ClickHouse, `serve` runs ingest only and the API
  answers 503.
- Service-identity passwords are HKDF-SHA256 of the 64-hex-character master key with info
  `crucible/clickhouse-identity/v1/<user>/<generation>` and no salt, written as 64 hex characters; generation 0 until
  rotation lands (milestone 4). A server migrated under another master key fails `detect`.
- `/api/v1` defaults: 30 s request timeout and 1 MiB body limit (default; revisit). A malformed project id answers 404
  (visibility); malformed trace and span ids answer 422.
- UI serving: try-files ignores the query string; slashless routes serve the page directly; under `/api/`, `/v1/`,
  `/mcp` and the health paths only the 404 fallback is replaced by a problem 404 (the UI's own pages under `/mcp/`
  still resolve); the empty source answers `/` with 404 and a build hint. Each page's CSP carries its inline-script
  hashes, `style-src-attr 'unsafe-inline'` and `img-src 'self' data:`.

### 12.2 Auth forms

| Form | Where | Notes |
|---|---|---|
| Session cookie plus `X-Crucible-CSRF` | UI requests to `/api/v1` | Section 11.4 |
| `Authorization: Basic base64(public_key:secret_key)` | API, ingest, compat ingest, MCP server | The value is split at the first `:`: the username is a Crucible public key (`cpk_`), the password a Crucible secret key (`csk_`). The row is found by the secret's `lookup_tag` and verified as in 11.3; the public key must be that row's public key (constant-time compare), else 401 with the message "the public key does not belong to the same project as the secret key; use the key pair shown together in Settings". An empty username or password answers 401 |
| `Authorization: Bearer <secret key>` | API, ingest, every compat ingest route, MCP server | Row found by `lookup_tag` |
| `Authorization: Bearer <public key>` | `POST projects/{id}/feedback` only | |
| None | `auth/providers`, pre-session auth routes, `/healthz`, `/readyz`, `version` | Public allowlist |

On ingest and compat ingest routes the key needs the `ingest` scope (403 otherwise). A public key alone
(Basic with an empty password, or Bearer with a public key) is refused with 401 on every ingest route. The
dialect 1 compatibility path is the route third-party SDKs reach with Basic auth; users configure those SDKs
with a Crucible public and secret key pair in place of the upstream pair. Refusals use the OTLP status body
and never echo either key. A mismatched pair is refused rather than logged, because it means the keys of two
projects were mixed; per 1.7 the refusal says what to fix, with the message quoted in the Basic row.

### 12.3 Errors

Errors are `application/problem+json` (RFC 9457):

```text
{ "type": "urn:crucible:problem:<code>", "title": string, "status": number, "detail": string,
  "code": string, "request_id": string,
  "errors"?: [{ "pointer": string, "code": string, "message": string }],
  "retry_after_s"?: number, "limit"?: { "name": string, "value": number }, "suggested_granularity"?: string }
```

`code` is stable. `title` and `detail` are for logs and developers; the UI maps `code` to its own copy and never
shows server text, with one exception: the `/sql/` page shows a `sql_*` problem's `detail` and the database's own
message, because a user writing SQL needs the engine's error. Guardrail errors are never retried by the server.

| Code | Status | Meaning |
|---|---|---|
| `unauthenticated` | 401 | No or invalid credential |
| `invalid_credentials` | 401 | Wrong email or password |
| `forbidden` | 403 | Role or scope too low |
| `csrf_invalid` | 403 | Missing or wrong CSRF header |
| `origin_invalid` | 403 | `Origin` or `Sec-Fetch-Site` check failed |
| `not_found` | 404 | Unknown route or entity, or an entity the caller cannot see, or one that was deleted or expired |
| `method_not_allowed` | 405 | |
| `request_timeout` | 408 | The server-side request deadline passed |
| `conflict` | 409 | Unique or state conflict |
| `idempotency_key_reused` | 409 | An `Idempotency-Key` was reused with a different request |
| `precondition_failed` | 412 | `If-Match` did not match |
| `payload_too_large` | 413 | |
| `unsupported_media_type` | 415 | |
| `validation_failed` | 422 | Body, query or path invalid; `errors[]` has JSON pointers |
| `token_invalid` | 422 | Reset, invitation or setup token invalid, expired or used |
| `precondition_required` | 428 | A mutable resource was changed without `If-Match` |
| `rate_limited` | 429 | With `Retry-After` and `retry_after_s` |
| `query_too_broad` | 422 | Window above the project maximum, row or byte guardrail, too many ids, or a granularity that exceeds the caps; may carry `limit` and `suggested_granularity` |
| `query_timeout` | 504 | Class time limit |
| `query_memory_exceeded` | 422 | Class memory limit |
| `cursor_invalid` | 400 | Tampered, expired, cross-project, or bound to other filters, sort or window |
| `control_plane_unavailable` | 503 | Postgres unreachable |
| `telemetry_store_unavailable` | 503 | ClickHouse unreachable and the recent index cannot answer |
| `ingest_unavailable` | 503 | The WAL cannot accept a fact or tombstone write |
| `feature_disabled` | 404 | A module is off for the deployment or project |
| `ssrf_blocked` | 422 | Webhook target rejected by the egress policy |
| `media_not_supported` | 501 | No object store is configured |
| `media_too_large` | 413 | Above the media cap |
| `media_hash_mismatch` | 422 | Uploaded object does not match the declared hash or length |
| `sql_invalid` | 422 | Syntax or lexical rejection; may carry line and column |
| `sql_forbidden` | 403 | Read-only or access-denied by ClickHouse |
| `sql_time_window_required` | 422 | The text does not use `{from}` and `{to}` |
| `sql_quota_exceeded` | 429 | Quota or concurrency limit |
| `sql_workspace_unavailable` | 503 | Disabled by the self-probe or by configuration; carries a reason |
| `export_expired` | 410 | The file expired or predates a deletion request |
| `internal_error` | 500 | Anything else, including a caught panic |

### 12.4 Pagination and signed cursors

- Lists return `{ data, next_cursor, has_more, meta }` with no total counts. `limit` defaults to 50, maximum 200.
- Every telemetry list and analytics request requires `from` and `to`. The maximum window is a project setting
  (default 30 days); a wider request is `query_too_broad` with `limit`.
- Telemetry lists paginate only when sorted by time.
- Cursor format: `v1.<kid>.<payload>.<mac>`. `kid` names a `signing_keys` row of purpose `cursor`. `payload` is
  the base64url (no padding) encoding of a version byte, the cursor kind (`page`, `head` or span children), route
  id, sort, project id, filter hash, issue time and an expiry 24 h after issue, then the position: for trace-list
  pages the window, `as_of` and the sort tuple of the last row; for heads the window, `as_of`, `wal_epoch` and LSN;
  for span children the trace, the parent and the last sort tuple. `mac` is the base64url of
  HMAC-SHA256 over `v1.<kid>.<payload>`, truncated to 16 bytes. Codec: `crucible-cursor` (pure, Kani-checked).
- A cursor that fails the MAC, is expired, or does not match the request's project, route, filters, sort or window
  is `cursor_invalid`. Clients treat that as "restart from the first page".
- `as_of` is fixed by the first page and bounds later pages (`received_at <= as_of`), so pages do not shift.

### 12.5 Concurrency and idempotency

- Mutable control-plane resources carry `version` (also sent as `ETag`). `PATCH`, `PUT` and `DELETE` on them
  require `If-Match`: a missing header is `precondition_required`, a mismatch is `precondition_failed`.
- `Idempotency-Key` (1 to 128 characters of `[A-Za-z0-9_-]`) is honoured on POST creates for exports, deletion
  requests, media and fact writes. Stored for 24 h per (principal, route, key) with a request hash; a replay
  returns the stored response; a different request under one key is `idempotency_key_reused`. For fact writes the
  principal is the author: a signed-in user writes facts as themselves (author kind `user`, capability
  `write_facts`), a key as the key, and idempotency keys are kept per author.
- The Crucible MCP server at `/mcp` accepts control-plane keys only with the `read` scope, and answers 503 with
  `Retry-After` when the control plane cannot check a key (the fail-open rule of 11.8 for unknown keys).
- A 405 carries an `Allow` header built from the route's methods.
- Fact-write idempotency keys live in the node's memory (at most 10,000, oldest dropped first, lost on restart) until
  the control plane's `idempotency_keys` table exists; a key whose first request is still running answers `conflict`,
  and a failed or cancelled request frees its key. A fact write answers 201 with `fact_id`, `key`, `version`,
  `created_at` and `target_time`. A span's facts are filed under its trace's earliest known start; a fact about an
  unknown target is `not_found` only when the lookup is whole and strong, else 503 with `Retry-After: 1`. The API's
  1 MiB body limit caps a correction request at 1 MiB.
- Search and payload routes (#59): `POST search` takes query, mode, fields (input, output, metadata), from, to,
  limit and cursor (cursor route 3: window, `as_of` and `(start_minute, trace_hash, span_id)`; its filter hash covers
  the mode, the fields and the tokens); `name` and `filters` are refused with a 422 until the shared filter DTO lands.
  The payload route takes a required `start_time`, serves only the nine `FieldRange` fields (others are a 422),
  honours a single byte range (206, or 416 without a problem body), and answers a miss that is not whole and strong
  with 503 `telemetry_store_unavailable`, because a byte response cannot carry `meta`.
- List routes (#58): `GET users/{user_id}/sessions` joins the session and user routes. `GET traces` and `GET spans`
  take the filters environment, release, version, level, observation_type, user_id, session_id, tags, metadata
  (`key=value`, split at the first `=`) and name (a case-insensitive substring, at most once); repeating a parameter
  matches any of its values, different parameters must all hold, at most 100 values per parameter. Cursor routes 4
  to 7 are the span list, the session list, the user list and the sessions of a user; a span-list page holds the
  window, `as_of` and `(start_time, span_id)`, and an id page holds the list, the window and `(last_seen, id)` with
  no `as_of`. Trace and span cursors bind to `Filter::hash`, the sessions of a user to a hash of the user id, and the
  session and user lists to an empty filter.

### 12.6 Endpoints

All paths are under `/api/v1`. Entity routes live under `projects/{id}/` (`{id}` is the project id). "Perm" names
the capability from section 11.6; key principals need the matching scope from 11.3.

**Auth and account**

| Method and path | Perm | Notes |
|---|---|---|
| `GET auth/providers` | public | Enabled providers, whether password login is on, whether setup is required |
| `POST auth/login`, `POST auth/logout` | public; session | |
| `GET auth/session` | session | Session DTO (12.7) |
| `GET auth/oauth/{provider_id}/start`, `GET auth/oauth/{provider_id}/callback` | public | One route family for all three provider kinds; full-page redirects; `next` validated |
| `POST auth/password/reset-request`, `POST auth/password/reset-confirm` | public | Request always answers 202; needs SMTP. Tokens: 256-bit, hashed, 30 minutes, single use; success revokes all sessions |
| `POST auth/password/change` | session | Revokes the user's other sessions |
| `POST auth/invitations/accept`, `POST auth/setup` | public | One-time tokens |
| `GET account`, `PATCH account` | session | Own profile |

**Organization**

| Method and path | Perm |
|---|---|
| `GET org`, `PATCH org` | read: any member; write: org settings |
| `GET org/members`, `PATCH org/members/{user_id}`, `DELETE org/members/{user_id}` | members |
| `POST org/transfer-ownership` | owner |
| `GET org/invitations`, `POST org/invitations`, `DELETE org/invitations/{invitation_id}` | members |
| `GET org/auth-providers`, `POST org/auth-providers`, `PATCH org/auth-providers/{provider_id}`, `DELETE org/auth-providers/{provider_id}` | org settings |

**Projects, members, keys, settings**

| Method and path | Perm |
|---|---|
| `GET projects`, `POST projects` | visible projects; create projects |
| `GET projects/{id}`, `PATCH projects/{id}`, `DELETE projects/{id}` | read; project settings; delete projects |
| `GET projects/{id}/settings`, `PATCH projects/{id}/settings` | read; project settings (capture defaults, max query window, enabled modules, public-key feedback switch) |
| `GET projects/{id}/members`, `POST projects/{id}/members`, `PATCH projects/{id}/members/{user_id}`, `DELETE projects/{id}/members/{user_id}` | project members |
| `GET projects/{id}/api-keys`, `POST projects/{id}/api-keys`, `PATCH projects/{id}/api-keys/{key_id}`, `DELETE projects/{id}/api-keys/{key_id}` | API keys. The secret is returned once by POST; DELETE revokes |
| `GET projects/{id}/retention`, `PUT projects/{id}/retention` | read; retention |
| `GET projects/{id}/models`, `POST projects/{id}/models`, `PATCH projects/{id}/models/{model_price_id}`, `DELETE projects/{id}/models/{model_price_id}` | read; model prices. Bundled defaults are read-only |

**Telemetry** (all under `projects/{id}/`; all require `from` and `to` unless noted)

| Method and path | Notes |
|---|---|
| `GET data-bounds` | No window; section 8.8 |
| `GET filter-values` | Section 8.8 |
| `GET traces` | Filters, `sort`, `cursor`, `since`; rows from heads, stats and facts |
| `GET traces/{trace_id}` | Optional `hint_time` instead of a window; `span_count`, `paged`; section 8.8 |
| `GET traces/{trace_id}/spans` | `parent_span_id` or `ancestors_of`; section 8.8 |
| `GET spans` | |
| `GET spans/{trace_id}/{span_id}` | Core tier, payload-tier small fields, facts |
| `GET spans/{trace_id}/{span_id}/payload` | `field` selector (`input`, `output`, `system_instructions`, `metadata`, `attributes`, `resource_attributes`, `events`, `links`, `tool_definitions`, `tool_calls`, `mcp_args`, `mcp_result`); raw bytes with `Content-Length`, `ETag`, immutable caching and HTTP byte ranges; `download=1` forces attachment disposition |
| `GET sessions`, `GET sessions/{session_id}` | |
| `GET users`, `GET users/{user_id}` | |
| `POST search` | Query, fields (`input`, `output`, `metadata`, `name`), filters, window; span hits with highlights |

**Facts** (under `projects/{id}/`; append-only writes)

| Method and path | Notes |
|---|---|
| `POST feedback` | Score with `source = feedback`; the one route that accepts a public-key Bearer |
| `POST tags`, `POST flags`, `POST corrections` | |
| `GET facts` | Effective facts for one target (`trace_id`, optional `span_id`, or `session_id`) |
| `GET comments`, `POST comments`, `PATCH comments/{comment_id}`, `DELETE comments/{comment_id}` | Postgres; edits and deletes are new versions |

**MCP analytics** (under `projects/{id}/mcp/`): `GET servers`, `GET tools`, `GET tools/{name}`, `GET errors`,
`GET clients`, `GET users`, `GET sessions`, `GET operations`, `GET operations/{trace_id}/{span_id}`, `GET deploys`,
`GET compare`. Parameters and sources are in section 9.9.

**Metrics, dashboards, alerts** (under `projects/{id}/`)

| Method and path | Notes |
|---|---|
| `POST metrics/query`, `GET metrics/schema` | Section 8.8. No SQL-backed widgets in v0 |
| `GET dashboards`, `POST dashboards`, `GET dashboards/{dashboard_id}`, `PATCH dashboards/{dashboard_id}`, `DELETE dashboards/{dashboard_id}` | The list includes the built-in starter dashboards, served read-only with a `builtin_key` |
| `POST dashboards/{dashboard_id}/duplicate` | Also accepts a `builtin_key`; copies into rows |
| `POST dashboards/{dashboard_id}/widgets`, `PATCH dashboards/{dashboard_id}/widgets/{widget_id}`, `DELETE dashboards/{dashboard_id}/widgets/{widget_id}` | At most 50 widgets; a widget holds a `MetricsQuery` without a window, display options and a rectangle on a 12-column grid |
| `GET alerts`, `POST alerts`, `GET alerts/{alert_id}`, `PATCH alerts/{alert_id}`, `DELETE alerts/{alert_id}`, `GET alerts/{alert_id}/history`, `POST alerts/preview` | A rule is one measure, at most 2 dimensions, comparison and threshold, `window` (1 min to 24 h), `interval` (at least 60 s), `for`, no-data policy, `renotify` (default 4 h, minimum 5 min), `source`, channels; at most 50 series |
| `GET channels`, `POST channels`, `GET channels/{channel_id}`, `PATCH channels/{channel_id}`, `DELETE channels/{channel_id}`, `POST channels/{channel_id}/test` | Kinds: Slack incoming webhook, email, signed webhook (`X-Crucible-Signature: t=<unix>,v1=<hex HMAC-SHA256 of "t.body">`). Test-send returns `{ ok, status_class, latency_ms, error_code }` and never echoes the receiver's body. Egress policy (`crucible-egress`): by default HTTP and HTTPS targets on any network are allowed, including private networks (self-hosted receivers often live there); cloud metadata addresses (169.254.169.254, fd00:ec2::254 and the link-local metadata ranges) are always refused; redirects are not followed; connect and response timeouts are 5 s and 10 s. `egress.mode = strict` adds HTTPS-only and refuses loopback, private, link-local, CGNAT and mapped ranges except `egress.allow_cidrs`. A refusal is `ssrf_blocked` with the reason and the setting that would allow it |

**Deletion, exports, SQL, media** (under `projects/{id}/`)

| Method and path | Notes |
|---|---|
| `GET deletion-requests`, `POST deletion-requests`, `GET deletion-requests/{request_id}` | Section 13 |
| `GET exports`, `POST exports`, `GET exports/{export_id}`, `GET exports/{export_id}/download` | Section 12.8 |
| `POST sql/run`, `DELETE sql/runs/{run_id}`, `GET sql/runs`, `GET sql/schema`, `POST sql/export` | Section 7.4. Export formats CSV, JSON lines, Parquet; CSV cells that start with `=`, `+`, `-` or `@` are prefixed |
| `GET sql/saved-queries`, `POST sql/saved-queries`, `GET sql/saved-queries/{query_id}`, `PATCH sql/saved-queries/{query_id}`, `DELETE sql/saved-queries/{query_id}`, `POST sql/saved-queries/{query_id}/run` | Visibility `private` or `project` |
| `POST media`, `PATCH media/{media_id}`, `GET media/{media_id}` | Section 14 |

**System**

| Method and path | Notes |
|---|---|
| `GET /healthz`, `GET /readyz` | Liveness; readiness (writable WAL and a loaded key snapshot) |
| `GET health` | Org admins: component detail (snapshot age, SQL probe, leases, WAL, store, durability mode) |
| `GET version`, `GET openapi.json` | Public |

**Ingest** (not under `/api/v1`)

| Endpoint | Notes |
|---|---|
| `POST /v1/traces` | OTLP HTTP, protobuf or JSON, optional gzip; partial-success responses; scope `ingest` |
| gRPC `TraceService/Export` on `:4317` | Raw-bytes codec; RetryInfo on backpressure |
| Compat ingest routes | Registered by `crucible-compat` (paths, header names and the media-token format are defined there). Each accepts Basic or Bearer Crucible keys and writes the same `IngestRequest` frame with its dialect hint. The compat media endpoint answers 501 `media_not_supported` when no object store is configured |

### 12.7 Session DTO

`GET auth/session` returns:

| Field | Shape |
|---|---|
| `user` | `{ id, email, name }` |
| `org` | `{ id, name, role }` |
| `projects` | `[{ id, name, slug, role, permissions: [string] }]` |
| `providers` | `[{ id, kind, display_name }]` |
| `password_login_enabled` | bool |
| `csrf_token` | string |
| `session` | `{ expires_at, idle_expires_at, auth_method }` |
| `features` | `{ sql_workspace: { enabled, reason? }, media: { enabled, max_bytes }, smtp: bool, signup_mode }`; `sql_workspace` reports the workspace's real state: `enabled`, or `not_configured`, or the runtime probe's reason |
| `server` | `{ version }` |

### 12.8 Export formats

Exports are jobs (claimed work) with a fixed `as_of`, written to an attempt-scoped temporary object and committed
atomically; gzip is optional; files go to the object store when configured, else to the `exports` disk root
(multi-node deployments need the object store for downloads). Both formats are re-emitted from the normalized
record with full payloads, never previews.

| Format | Contract |
|---|---|
| `crucible-jsonl` | One JSON object per line with **exactly three top-level keys: `schema`, `span`, `facts`**. `schema` is the string `crucible.span/1`. `span` nests the whole record (both tiers, wire rules of section 1.3, the MCP operation under `span.mcp` when present). `facts` holds the effective facts for the span and its trace. No other top-level key may ever be added, so format detectors that look at top-level keys cannot confuse it with another format |
| `otlp-json` | **One span per line**: each line is a complete OTLP JSON resource-spans document that holds exactly one span, with `startTimeUnixNano`, re-emitted with OTel GenAI keys (operation name, request and response model, input and output messages, system instructions, token usage, finish reasons, agent and prompt names) plus every residual attribute. Attribute keys that dialect mapping consumed are never re-emitted |

`otlp-json` message emission by `io_shape`: `genai_messages` as stored; `indexed_messages` converted to a message
array; `raw` whose input is an object with a `messages` array emits that array; any other `raw` value is wrapped
in one user message and the span carries `crucible.io_shape`. A span with a truncated field is skipped and counted
in the job summary (job option; default skip). In `crucible-jsonl` truncated fields carry their truncation
markers.

## 13. Retention and deletion

Owner: `crucible-lifecycle` (executors, leader lease `lifecycle` and claimed steps), `crucible-plan` (window
clamp), `crucible-sqlws` (policy rewrite), `crucible-media` (media reach).

### 13.1 Settings

| Project setting | Meaning | Bound tables |
|---|---|---|
| `retention_days` | Null keeps data forever | Every table not listed below |
| `payload_retention_days` | At most `retention_days` | `span_payload`, `fact_corrections`, media |
| `mcp_content_retention_days` | At most `retention_days` | `mcp_content` |

- Retention is evaluated on `start_time` (facts: `target_time`; rollups: their bucket time; inventory and deploys:
  `last_seen`).
- **Reads hide expired rows immediately**: the plan renderer clamps `from` to the retention cut-off, and the SQL
  workspace's per-project row policy carries the same cut-off.
- Shortening retention applies to existing data. Lengthening never resurrects data that was already hidden.

### 13.2 Retention executor

Runs hourly under the `lifecycle` lease. Cut-offs are aligned to UTC days. The plan is keyed by (project, table
binding, cut-off day), so a rerun is safe.

1. For each partitioned table and each partition that lies wholly before the cut-off of at least one project:
   list the distinct projects with rows in it (`partition_projects`).
2. **If every listed project has expired for that partition, drop the partition.**
3. **Otherwise issue `DELETE FROM <table> IN PARTITION <p> WHERE project_id IN (<expired projects>)`.** For a
   monthly partition that straddles a project's cut-off the predicate also carries
   `<time column> < <cut-off day>`.
4. Tables without a partition (`mcp_tool_inventory`, `mcp_deploys`) get a weekly delete of keys whose `last_seen`
   is before the cut-off (default; revisit).
5. Deletes are sent with `lightweight_deletes_sync = 1`; the executor polls `mutation_status`.
6. **Mask application**: the mask scheduler coalesces every pending lightweight delete and issues
   `ALTER TABLE ... APPLY DELETED MASK IN PARTITION <p>` at most once per partition per cycle. The cycle is **6 h**
   (`lifecycle.mask_cycle`), which gives physical removal within 24 h in the worst case. If mask application is
   unavailable on a target (pinned by conformance row "mask application"), the fallback is an `ALTER TABLE ...
   DELETE` mutation on that schedule.

No table has a TTL and no table has projections, so a lightweight delete never throws for that reason.

### 13.3 Deletion requests

Kinds: `trace` (a list of trace ids, at most 10,000), `user` (`user_id`, optional `issuer`), `session`
(`session_id`), `project`. A delete is point-in-time: data received at or after `requested_at` is new data and is
kept. States: `pending`, `logged`, `applied`, `verified`, `completed`, `failed`.

| Step | Actor | Action | State after |
|---|---|---|---|
| 1 | API | Insert the Postgres row with `requested_at` and a `deletion_seq`. The accepting node adds it to its pending-deletion set before responding | `pending` |
| 2 | Every node (timer, at most every 5 s) | Refresh the pending-deletion set; append a `Tombstone` frame to its own WAL (durable); record `(request, wal_epoch, tombstone_lsn)` in `deletion_node_acks` | `logged` once every live node has recorded |
| 3 | Every node, stage B | On reaching the tombstone: drop matching un-shipped spans and facts whose `received_at < requested_at`; evict matching recent-index entries | |
| 4 | Every node | Set `passed_at` when its checkpoint watermark is at or above its `tombstone_lsn` | |
| 5 | Deletion executor (claimed job, every 30 s) | When every live node (heartbeat within 60 s) has passed: resolve the selector, then issue typed deletes for each reach-list table with `received_at < requested_at`. Each place is an idempotent step with its own status | |
| 6 | Executor | All mutations report done | `applied` |
| 7 | Executor | After the mask cycle: a count of zero in every place and no part in the touched partitions still carrying a delete mask | `verified` |
| 8 | Executor | Comments, media references and objects, and export files handled | `completed` |
| any | Executor | Attempts exhausted | `failed`, with an operator alert; resumable |

Selector resolution: trace deletes are bounded by the traces' `trace_stats` times. User deletes match `user_id` on
span and head tables; on MCP tables they match `user_id` or `user_subject_hash` computed with the project salt for
the given issuer, or for every distinct `user_issuer` seen for the project when no issuer is given. User and
session deletes first resolve their trace ids (needed for MCP rows by trace, facts, comments and media).

A node that is down does not block a request. When it returns, its recovery and any replay consult the deletion
requests and drop matching rows before shipping. `crucible wal drain` does likewise.

### 13.4 Reach list

| Place | Rule |
|---|---|
| Typed reads (API, MCP server, exports in progress) | Hidden immediately through the pending-deletion set: at once on the accepting node, within 5 s on other nodes |
| `span_core`, `span_payload`, `trace_heads` | Rows deleted by selector; physically removed by the mask cycle |
| `mcp_ops`, `mcp_content` | Deleted by trace id, and for user deletes by `user_id` and subject hash |
| `fact_scores`, `fact_tags`, `fact_flags`, `fact_corrections` | Deleted by target |
| `trace_stats`, `session_rollup`, `user_rollup` | Rows deleted by key |
| `span_metrics`, `mcp_tool_rollup`, `mcp_deploys`, `mcp_tool_inventory` | Hold no user dimension and no content. Counts are kept by default; `crucible admin rebuild-rollups` rebuilds a project and day range. Project deletion removes them |
| Postgres `comments` | Deleted by target |
| Un-shipped WAL frames | Dropped by stage B (step 3) |
| Retained and quarantined WAL segments | Replay consults deletion requests; retention is capped at 24 h, so no raw copy outlives the SLA (a pinned quarantine is the documented exception) |
| Recent-span index | Entries evicted at the tombstone; reads filtered before that |
| **Media** | `media_refs` rows for the affected traces are deleted; an object is deleted when its last reference goes; project deletion removes the project's key prefix. Media also follows `payload_retention_days` by reference target time. A presigned download issued earlier can outlive a deletion by at most 5 minutes |
| **SQL workspace path** | Retention applies immediately through the per-project row policy. A deletion becomes invisible to SQL when the executor issues the masking delete (step 5), not at request time. The target is 60 s after the request; it is a target and not a bound, because step 5 waits for every live node's checkpoint. Project deletion also drops the project's ClickHouse identity and policies |
| Export files | Files created before the request are expired (`export_expired`) |
| Control-plane rows (project deletion) | Project, memberships, keys (through the key feed), dashboards, alerts, channels, saved queries, runs, salts and secrets |

### 13.5 SLA

| Guarantee | Bound |
|---|---|
| Hidden from typed reads | Immediately on the accepting node; 5 s elsewhere |
| Hidden from SQL (deletion) | Target 60 s |
| Hidden from every read (retention) | Immediately |
| Physical removal from ClickHouse | 24 h (6 h mask cycle) |
| Raw copies in the WAL | 24 h |
| Media objects | 24 h; presigned links 5 minutes |
| Minute rollup counts | Kept unless the operator rebuilds |

## 14. Media

Optional. Owner: `crucible-media` on `ObjectStore`. Without an object store, clients keep inline base64 under the
8 MiB field cap and every media route answers `media_not_supported`.

### 14.1 Configuration and caps

| Item | Value |
|---|---|
| `[object_store]` | Internal `endpoint`, client-reachable `public_endpoint`, region, bucket, credentials (value or file), URL style, key prefix |
| Size cap | 64 MiB per object (`media.max_bytes`) |
| Content types | Allowlist; anything outside a small inline-safe list (common image, audio and video types) is served with attachment disposition |
| Presigned PUT lifetime | 1 h |
| Presigned GET lifetime | 5 min |

### 14.2 Flow

1. `POST projects/{id}/media` with content type, length, SHA-256, trace id, and optional span id and field.
2. If `(project, sha256)` is already `uploaded`, the response carries the existing media id and no URL, and a
   reference is added. Otherwise a `pending` row is created and a presigned PUT is returned with content type and
   checksum as signed headers.
3. The client uploads directly to the public endpoint.
4. `PATCH projects/{id}/media/{media_id}` confirms: the server calls `head`, checks the length (and the checksum
   where the store returns it), and marks the object `uploaded`; a mismatch is `media_hash_mismatch`.
5. `GET projects/{id}/media/{media_id}` returns metadata and a presigned GET.

### 14.3 Keys, references and tokens

| Item | Contract |
|---|---|
| Object key | `<prefix>/<project_id>/<yyyy>/<mm>/<media_id>` |
| `media_objects` | `id`, `project_id`, `sha256`, `content_type`, `len`, `state` (pending, uploaded, deleted), `created_at`; unique `(project_id, sha256)` |
| `media_refs` | `project_id`, `media_id`, `trace_id`, `span_id`, `field`, `target_time`; one row per use |
| Crucible media token | An opaque string in a payload field that carries the media id and content type; the normalizer leaves it in place and the UI resolves it through the GET route |
| Compat media token and endpoint | Parsed and served inside `crucible-compat`, mapped onto this flow |

### 14.4 GC

Under the `media_gc` lease: hourly, pending uploads older than 24 h are removed and objects whose last reference
is gone are deleted; weekly, an orphan scan deletes stored objects older than 48 h that have no row. Operators are
advised to add a bucket lifecycle rule as a backstop.

## 15. Change control

- This document is contract version **v2**.
- A contract changes only through a pull request that **updates this document first**, in the commit that changes
  the code, schema or fixture it describes. A code change that contradicts this page is a defect in the code.
- Additive changes (a new enum variant appended, a new optional field, a new endpoint, a new table) keep the
  version and add a line to the change log below. A breaking change (a renumbered enum, a changed frame layout,
  token format, sort key, partition expression, cursor format or problem code) bumps the version and states the
  migration: a new `wal_epoch`, a new table plus backfill, or a new API version.
- Every "(default; revisit)" value may be changed by an ordinary contract pull request without a version bump,
  unless it is stored on disk or on the wire.
- Each choice between conflicting designs is recorded in [`decisions.md`](decisions.md).

| Version | Date | Change |
|---|---|---|
| v2 | 2026-11-17 | First frozen contract; reconciles the six track designs |
| v2 | 2026-11-17 | Additive: internal dependency edges, `ProjectScope` minting by lint, settled-in-code tables (2.14, 3.11, 4.10, 6.7, 8.9), pico-USD prices, frozen MCP duration bounds, real-server grants |
| v2 | 2026-11-17 | Additive: `TelemetryStore` final shape with `probe(BatchProbe)`, typed result rows and `Stores` (2, 2.6, 8.1, 8.5) |
| v2 | 2026-11-17 | Clarification from the ingest model: probe once in doubt, probe results expire, quarantine only after an absent probe, quarantined pairs are read gaps (5.5, 5.6, 6.2, I3) |
| v2 | 2026-11-18 | Dialect contract (3.1, 3.9, 3.10, 6.4, 6.6, 8.9, 10.1 to 10.6, 12.1, 12.2, I7). Additive: dialect tables as layers with scoped sources, the detection confidence scale, new decoders, merge and consumption rules, native source keys for `provider` and `tool_names`, compat ingest routes, the Basic auth rules, and the `span_core` column `usage_aggregate` with its forward-only schema step. Changes behaviour: the request hint is a detection at confidence 60, no longer an override; token and cost sums leave out spans with `usage_aggregate = true` (an observed usage-bearing descendant); a Basic public key that does not match the secret key's row is refused with 401 |
| v2 | 2026-11-18 | Settled in code by the read IR (8.9): model guardrails, zero fill and its empty values, UTC buckets, compiled sources, filter hash binding, metadata conjuncts, rollup pages without `as_of`, set merge |
| v2 | 2026-11-18 | Changes behaviour (I7 fix): stage B holds only what the retained prefix holds, evicting by last sighting and forgetting after each collection; admissions record `relies_on` and garbage collection keeps those frames; a failed collection pauses admission until one succeeds (3.1, 4.6, 4.7, 5.3) |
| v2 | 2026-11-18 | Settled in code by the dialect contract's implementation: the usage descendant set evicts and forgets by support (the LSN of the frame carrying the usage), and the 3.1 known limit covers a descendant whose frame the WAL collected (3.1, 5.3) |
| v2 | 2026-11-18 | Additive (wave 3): edges for `crucible-proxy`, `crucible-mcp-server`, `crucible-sdk` and its conformance runner, and the SDK area row (1.5); the MCP key registry is `conformance/registry.json` (9.1); `ChildProcess` is not part of `Env` and its simulation lives in the proxy's tests (2.14) |
| v2 | 2026-11-18 | Settled in code by the job runner (#72): the `crucible-jobs` edge (1.5); claim length, retry backoff, the attempt cap and parked rows; cursors keyed by lease name; the housekeeping job; the narrower shutdown rule (11.7) |
| v2 | 2026-11-18 | Settled by identity (#56): edges `crucible-api` to `crucible-control` and `xtask` to `crucible-api` (1.5); `--keys-file` is the mode without Postgres and is refused alongside it, `admin create-key`, fact authors and per-author idempotency, the `/mcp` read scope, 405 `Allow` (12.1, 12.5) |
| v2 | 2026-11-18 | Settled in code by #59, #61 and #76: search plans, phrases and metadata terms (8.9); the compiled `traces` metrics source and what is not built (8.9); three fact lookups per list page (8.7); fact-write idempotency and responses, search and payload routes (12.5); store connection keep-alive and memory-budget warnings (2.14); `recent.max_bytes` from the container limit (8.6) |
| v2 | 2026-11-18 | Settled by the SQL workspace (#73): provisioning order, the 1,800-statement quota as an operator setting (7.4), the SQL route permissions and the export decision (11.6), the `/sql/` exception for engine errors (12.3), `features.sql_workspace` (12.7), the `crucible-sqlws` edges (1.5) |
| v2 | 2026-11-18 | Settled in code by #58, #78 and #79: SDK export queues bounded by spans and bytes (9.10); the MCP storage readings and normalizer version 5 (9.11); project salts (11.1); the fake collector's normalizer edge (1.5); the filtered trace list, span list, sessions of a user and list consistency (8.9); list filters, routes and cursor routes 4 to 7 (12.5) |
