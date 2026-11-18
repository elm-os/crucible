# Adding an endpoint

How a milestone 2 slice adds a read endpoint: the files it creates and the line it adds to each registry, in
pipeline order, each step with the check that proves it. The binding rules are in [`contracts.md`](contracts.md):
the plan of the endpoint (8.7), the request contract of the UI (8.8), the route (12.6), errors (12.3), pagination
and cursors (12.4) and the wire rules (1.3). A slice that finds the contract wrong or silent implements its most
consistent reading and reports the gap; the contract itself changes only under its change control (section 15).

## One module per endpoint in each crate

An endpoint is one module in each of four crates, so the code its slice changes is in those modules. What the
endpoints share stays in the shared modules.

| Endpoint | Plan builders, `crucible-plan/src/endpoints/` | Service, `crucible-query/src/` | Route group, `crucible-api/src/routes/` | DTOs, `crucible-api-types/src/` |
|---|---|---|---|---|
| Trace list and live tail | `trace_list.rs` | `trace_list.rs`, `live_tail.rs` | `traces.rs` | `traces.rs` |
| Trace by id | `trace_bounds.rs`, `trace_spans.rs` | `trace_detail.rs` | `trace_detail.rs` | `trace_detail.rs` |
| Children of a span | `trace_bounds.rs`, `span_children.rs` | `span_children.rs` | `spans.rs` | `spans.rs` |
| Data bounds | `data_bounds.rs` | `data_bounds.rs` | `data_bounds.rs` | `data_bounds.rs` |
| Sessions | `sessions.rs` | `sessions.rs` | `sessions.rs` | `sessions.rs` |
| Users and the sessions of a user | `users.rs` | `users.rs` | `users.rs` | `users.rs` |
| Span list | `span_list.rs` | `span_list.rs` | `span_list.rs` | `span_list.rs` |
| Search (plan builders only so far) | `search.rs` | | | |
| Payload byte range (plan builder only so far) | `payload.rs` | | | |
| Metrics query (plan builders only so far) | `metrics.rs`, with the registry in `registry.rs` and the list filter in `filter.rs` | | | |
| Shared | `build.rs`: the retention clamp, the trace bounds window, the span predicates | `service.rs`, `spans.rs`, `heads.rs`, `context.rs`, `cursor.rs`, `decode.rs` | `routes.rs`, `convert.rs`, `params.rs`, `filters.rs` (the list filter parameters), `cursor.rs`, `access.rs`, `problem.rs` | `meta.rs`, `enums.rs`, `problem.rs`, `wire.rs` |

Each registry is a sorted list with one entry per endpoint; insert a new entry at its sorted place. Two slices whose
names sort next to each other insert at the same place, and git reports a conflict there that is resolved by keeping
both entries.

## Checklist

1. **Plan builder module.** Create `crates/crucible-plan/src/endpoints/<endpoint>.rs`. Each builder takes the
   `ProjectScope`, the window (or `TraceBounds`), `&RetentionClamp` and the `Caller`, returns
   `Result<Option<QueryPlan>, PlanError>` and ends in `clamp.plan(scope, window, caller.class(..), root)`, which
   clamps the window to retention and returns `None` when it has expired. Its doc comment lists the output columns
   in order. Register it in `crates/crucible-plan/src/endpoints.rs` with `mod <endpoint>;` and one
   `pub use <endpoint>::{..};` entry.

   Verify: an output-schema test in `crates/crucible-plan/tests/output.rs`; the builder in
   `every_built_plan_carries_its_scope_and_window` and `every_builder_clamps_its_window_to_retention` in
   `crates/crucible-plan/tests/properties.rs`; `cargo nextest run -p crucible-plan`.

2. **Renderer coverage.** Add one insta snapshot per plan of the builder to
   `crates/crucible-store-clickhouse/tests/render.rs`, named `m2_<endpoint>`, and read the SQL before accepting
   it. A builder that needs a node kind, measure or predicate form the renderer lacks extends
   `crates/crucible-store-clickhouse/src/render.rs` and the plan generator of
   `every_statement_carries_the_project_and_window_predicates_and_only_bound_values` in the same test file.

   Verify: `cargo nextest run -p crucible-store-clickhouse --test render` passes and leaves no `.snap.new` file.

3. **ChModel interpretation and a differential row.** The simulation's store model must answer every plan:
   `crates/crucible-sim/src/ch_read.rs` interprets each node kind, measure and predicate, and a slice that adds one
   adds its interpretation there (the model may be stricter than ClickHouse, never more permissive). Add the
   differential row to `endpoint_plans` in `crates/crucible-conformance/src/reads.rs`: each plan of the builder
   goes through `rows(ctx, plan)`, which runs it on the server and on `ChModel` and compares the answers, and its
   expected rows are checked against the fixture (extend `load` when the endpoint reads rows the fixture lacks).
   Name the builder in the summary line of `reads()`.

   Verify: `crates/crucible-conformance/scripts/conformance.sh single` runs the suite against ClickHouse 26.3,
   26.8 and latest in Docker. The `conformance` workflow runs it on every pull request that touches
   `crucible-plan`, `crucible-store-clickhouse`, `crucible-schema`, `crucible-chfmt`, `crucible-conformance` or
   `crucible-sim/src/ch_*.rs`.

4. **Query service module.** Create `crates/crucible-query/src/<service>.rs` with the request and response types
   and one `impl QueryService` block whose method takes `&ReadContext`. It builds plans only through the builders
   of step 1 and reads them with `Read::rows`, so an unavailable store makes the answer partial instead of failing
   it while the recent index can answer. It filters recent-index entries with `Read::retained` and ends with
   `Read::finish`, which builds `meta` from the tables the read depends on. A read of a trace's spans reuses
   `spans.rs` (the merge with the recent index and de-duplication by `span_id`); a read of trace heads reuses
   `heads.rs`. Register it in `crates/crucible-query/src/lib.rs` with `mod <service>;` and one
   `pub use <service>::{..};` entry.

   Verify: `crates/crucible-query/tests/services/<service>.rs`, registered with `mod <service>;` in `main.rs`, runs
   the service over the in-memory store (`store.rs`) and the node model (`world.rs`) and covers a store outage
   (`partial: store_unavailable`), stage B lagging (`consistency: lagging`) and retention;
   `cargo nextest run -p crucible-query`.

5. **API route module with OpenAPI.** Create `crates/crucible-api/src/routes/<group>.rs` with:
   - the handler and its `#[utoipa::path]`: operation id, tag, path and query parameters, and every problem
     response it can return. It checks every parameter with `Params` (which refuses unknown ones) and
     `check_window` before `state.service(rid)?`, so a server without a store still validates the request. Each
     refusal names what to change: the parameter by JSON pointer (contract 12.3), or the setting that caused it
     (contract 1.7);
   - `pub(super) fn routes() -> Router<ApiState>`, and `pub(super) struct Paths` with
     `#[derive(OpenApi)] #[openapi(paths(..))]` over the group's handlers;
   - the conversion of the service's response to its DTO. `crates/crucible-api/src/convert.rs` holds the
     conversions the groups share (`meta`, `span`, `micros_between`).

   Register the group in `crates/crucible-api/src/routes.rs`: `mod <group>;`, one `.merge(<group>::routes())` line
   in `router()` and one `doc.merge(<group>::Paths::openapi());` line in `document()`. Then regenerate the document
   with `cargo run -p crucible -- openapi > specs/openapi/crucible-v1.json`. A paged endpoint also needs a cursor:
   a `Route` and a `Position` in `crucible-cursor`, and seal and open methods in `crates/crucible-api/src/cursor.rs`
   that bind it to the request's filters, sort and window.

   Verify: `crates/crucible-api/tests/api/<group>.rs`, registered in `main.rs`, with insta snapshots of the JSON
   and the problem cases; the operation count and the path parameters in `tests/api/document.rs`;
   `cargo nextest run -p crucible-api`, whose `the_document_matches_the_committed_file` fails while the committed
   document is stale.

6. **DTOs with ts-rs.** Create `crates/crucible-api-types/src/<group>.rs`. The query parameters derive `IntoParams`
   and `TS` with `#[into_params(parameter_in = Query)]` and `#[serde(deny_unknown_fields)]`; each response derives
   `Serialize`, `Deserialize`, `ToSchema` and `TS`. The wire rules of contract 1.3 apply: 64-bit integers are
   `SafeU64` or `SafeI64` with `saturated` on the enclosing object, costs are `*_usd` from `usd_from_nanos`, ids and
   times are strings, and every telemetry response carries `meta: Meta`. Register it in
   `crates/crucible-api-types/src/lib.rs` with `mod <group>;` and one `pub use <group>::{..};` entry, add the
   parameter and response types to `visit_exported`, and run `cargo xtask gen-ts`.

   Verify: what the `drift` workflow checks. After both regenerations
   (`cargo run -p crucible -- openapi > specs/openapi/crucible-v1.json && cargo xtask gen-ts`),
   `git status --porcelain -- specs/openapi ui/src/api/generated` lists only the files the slice meant to change,
   and `grep -rn bigint ui/src/api/generated` finds nothing. Then `cargo nextest run -p crucible-api-types`.

7. **UI section with the four states.** The section lives in `ui/src/features/<feature>/` and renders through
   `Section` (`ui/src/components/section.tsx`), which keeps loading, error, empty and success in one frame. An
   empty answer counts as empty only when its `meta` is whole (`isWhole`); `MetaNotice` says when it is partial or
   lagging. Its request is a function in `ui/src/api/client.ts` typed by the generated DTOs.

   Verify: a Vitest test beside the section covers the four states (`traces-section.test.tsx` is the pattern),
   and `pnpm --filter crucible-ui test`, `pnpm --filter crucible-ui run typecheck`, `pnpm lint`,
   `pnpm format:check` and `pnpm --filter crucible-ui build` pass, as the `ci-ui` workflow runs them.

   Playwright surface row (planned): milestone 2 is designed to close on a Playwright surface table with one row
   per section, each driving the section's loading, success, error and empty states in a browser. The table does
   not exist yet. Until it does, the Vitest test carries the four states, and `tools/e2e/ui-traces.mjs`, which the
   end-to-end test runs, is the only browser check.

8. **An invariant in the system simulation, where the slice adds data.** A slice that adds data (a table, a fact
   kind, a derived row) adds its check to `crates/crucible-sim/src/system_sim.rs`. The read goes through the real
   `QueryService` after acknowledgements and at quiescence (`read_everything`) and is compared with the oracle's
   ledger, and a failure is a `Violation` mapped to its invariant in
   [`../testing/invariants.md`](../testing/invariants.md), for example I4 for facts or I7 for aggregates. The
   invariant table of [`../testing/system-simulation.md`](../testing/system-simulation.md) gains its row.

   Verify: `cargo nextest run -p crucible-sim --test system_dst` and `cargo xtask sim run --configs 200 --seeds 3`;
   the `sim` workflow runs the corpus and 600 fresh runs on every pull request.

## Lines that slices still share

Every registry entry belongs to one endpoint, and rustfmt formats each entry on its own, so a slice adds lines and
never edits another endpoint's. Insertions can still meet: when two names sort next to each other, git reports a
conflict that is resolved by keeping both entries. These places are still one list for every endpoint, or a line
that every endpoint of a kind changes; each is a follow-up:

- The operation count and `concrete()` in `crates/crucible-api/tests/api/document.rs`: every slice that adds
  a route changes the count.
- The builder calls of `every_built_plan_carries_its_scope_and_window`, and the `plans` and `tables` arrays of
  `every_builder_clamps_its_window_to_retention`, in `crates/crucible-plan/tests/properties.rs`.
- The fixture, `endpoint_plans` and the summary line of `reads()` in `crates/crucible-conformance/src/reads.rs`.
- `crates/crucible-store-clickhouse/tests/render.rs`, which holds every builder's snapshots.
- `visit_exported` in `crates/crucible-api-types/src/lib.rs`, sorted by type rather than by endpoint.
- The `tags(...)` list of the document in `crates/crucible-api/src/openapi.rs`: a group with a new tag adds to it.
- The `Route` and `Position` enums in `crates/crucible-cursor/src/payload.rs` and the cursor methods in
  `crates/crucible-api/src/cursor.rs`: every paged endpoint adds to each.
- The `Allow: GET, HEAD` header that `unknown_method` sends in `crates/crucible-api/src/lib.rs`: the first route with
  another method (for example `POST search`) changes it.
- `ui/src/api/client.ts`, which holds every request function.
- `crates/crucible-sim/src/system_sim.rs`, the one oracle.

## Before the pull request

Run the quality bar in the dev container ([`../development.md`](../development.md)): `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --locked -- -D warnings`,
`cargo nextest run --workspace --locked --no-tests=pass`, `cargo deny check` and
`node tools/policy/policy.mjs files`. The pull request must pass every CI workflow, among them `ci-rust` (Linux and
Windows), `drift`, `sim`, `e2e`, `conformance`, `ci-ui` and `policy`.
