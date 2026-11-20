# SQL workspace isolation (`sqliso`)

How the SQL workspace (contracts 7.4 and 7.5) is verified. The workspace is the one place where a user's own
SQL reaches ClickHouse, so isolation between projects is carried by ClickHouse access control alone: one
ClickHouse user per project, row policies in two layers and two forms, a read-only settings profile whose every
value is constant, and a quota. Nothing in Crucible's own code is a security boundary there. The suite therefore
runs against real servers only, on ClickHouse 26.3, 26.8 and latest, and attacks them as a project's own
identity. The method is in [`methodology.md`](methodology.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Access step A2 | `crates/crucible-schema/src/workspace.rs` | The role, the profile, the quota, the column grants, the static policies, the curated views and the two probe identities, rendered from `access_workspace_shape()` |
| Store | `crates/crucible-store-clickhouse/src/sqlws.rs` | `SqlWorkspaceStore`: the text verbatim as the project identity, `EXPLAIN` validation, the grid decoder, provisioning, the runtime self-probe |
| Service | `crates/crucible-sqlws` | The lexical check, parameters, limits, saved queries, exports, audit rows, the probe gate and the identity reconciler |
| Layers A, B, D | `crates/crucible-conformance/src/sqliso`, binary `crucible-sqliso` | Direct attacks, grammar fuzzing, broken deployments |
| Layer C | `crates/crucible/tests/sqliso_api.rs` | The real binary with a real ClickHouse and Postgres, through the REST API |
| Master key | `crates/crucible/tests/sqlws_master_key.rs` | The real binary started with the right master key, then once with a wrong one, then with the right one again: the workspace must still run queries |
| Fuzz targets | `crates/crucible-sqlws/fuzz` (`lexer`), `crates/crucible-store-clickhouse/fuzz` (`grid`) | The lexical check and the result decoder under libFuzzer |
| CI | `.github/workflows/sqliso.yml` | Every layer on 26.3, 26.8 and latest, under both values of the server switch |

## Fixtures and the oracle

Projects A and B each have one row in every SQL-readable table. Every string column of B's rows holds B's
canary, a random token made for the run. A second canary sits in an admin-only table and in the text of an admin
query, where a query log or a process list would show it. Every statement sent as project A passes one oracle:
neither canary may appear in the answer or in the error text. No statement the suite writes holds a canary
itself (errors quote the statement), so searches use the canaries' shared prefix.

The server switch `users_without_row_policies_can_read_rows` decides what a user without a policy reads on a
table that has policies: every row (the shipped default) or none. The access scheme must behave the same under
both (contract 7.2), so every layer runs under both values and the suite checks that the server behaves as the
run declares.

## Layer A: direct attacks

Scripts W1 to W15 send statements straight to ClickHouse as project A's user, with no Crucible code in the
path.

| Script | What it attacks | What must hold |
|---|---|---|
| W0 | The deployment itself, read as the admin | Every policy of both layers in both forms; the role holds column grants and the view database only; every profile element is `CONST` and `readonly = 1`; the quota's limits; the server switch as declared |
| W1 | Cross-project reads by every path: joins, unions, CTEs, subqueries, `merge()`, `view()`, `FINAL`, `SAMPLE`, `PREWHERE`, virtual columns, the trivial count, the views | Project A's rows only, and no ungranted column |
| W2 | Every table function the server lists, with five argument shapes | Refused, or on a short list of functions that read only their arguments |
| W3 | Every system and information-schema table | Refused, or readable without a canary; log tables, the process list and the access entities must be refused |
| W4 | `SET`, `SETTINGS` clauses, URL parameters, profile and role switches, `EXECUTE AS` | Refused or without effect; the session keeps `readonly = 1` and its own user |
| W5 | Writes, DDL, temporary and attached objects, `SYSTEM`, `KILL`, users, roles, grants, policies, quotas, profiles, backups, `OUTFILE` | Refused, and the admin sees an unchanged catalogue |
| W6 | Every function the server lists, with six argument shapes | No canary, an unchanged catalogue, a healthy admin ping |
| W7 | Each resource guard | Its error code: rows to read, sleep, AST depth and size, subquery depth, query size, join, set and group-by caps, the result cap, the window backstop, the concurrency cap |
| W8 | The quota, at its real limits, on an identity of its own | Statement 1,801 of the hour is refused with code 201; another identity is unaffected. The quota's hour follows the clock and the server starts a new hour's count at the first statement over the limit, so a run that crosses into the next hour runs again on a fresh identity |
| W9 | Settings a read-only identity may still change | No profile element is changeable; what the server itself leaves changeable is a presentation setting and shows no other row |
| W10 | Error text, `EXPLAIN` in nine forms, the query cache | Errors quote project A's rows only; no row of B is evaluated; the same text answers A and B with their own rows |
| W11 | Lifecycle races: the user before its policies, policies dropped, a malformed user name, a retention change, deprovisioning | No state reads another project; a retention change hides old rows at once |
| W12 | `currentUser()` inside a definer-rights view | Recorded; it decides whether grants can move from base tables to views |
| W13 | The window backstops through the curated views | `force_index_by_date` refuses an unwindowed read of a partitioned relation; `max_partitions_to_read` refuses a 400-day window |
| W14 | Many identities | Provisioning latency and the cost of the policies on a read are recorded; isolation holds with them present |
| W15 | ClickHouse Cloud specifics | Not run: it needs a Cloud service |

## Layer B: grammar fuzzing

A seeded generator writes statements over the base tables, the views, table functions and system tables, with
random conditions, `PREWHERE`, `FINAL`, `SAMPLE`, grouping, set operations, joins and `SETTINGS` clauses; every
third statement is a mutation of a corpus of 93 escape attempts. The oracle is the canary oracle, and the admin's
view of the catalogue must be the same after the run as before it. A pull request runs 5,000 statements under
the strict switch and 1,000 under the permissive one; the nightly run sends 500,000. The seed is printed, and
`--seed` replays a run.

## Layer C: through the API

The real binary, started with `--migrate` against a real ClickHouse and Postgres. `crucible admin init` makes
the owner and `crucible admin create-key` a key with the `read` and `sql` scopes for each of two projects. The
keys run the queries; the owner signs in for the saved-query writes, which need `edit_content` and so are
never a key's. Every response to a caller on project A passes the canary oracle. A key without the `sql` scope
is refused with `forbidden`, and a key is refused when it saves a query. The test also checks what the API adds:
the validation messages (each refusal says how to fix the query), the mapping of ClickHouse's refusals to problem
codes, a statement that outlives its limit ending as `query_timeout`, the result cap, the concurrency cap,
cancellation by `DELETE` and by closing the connection (the audit row says cancelled and ClickHouse holds no
statement of the project's identity afterwards), the audit rows, saved queries with declared parameters and
versions, the CSV formula guard, and that an export longer than it holds is refused instead of cut.

## Layer D: broken deployments

Each mutant breaks the deployment the way an operator or a bad migration could. The detectors (W0 to W5 and the
runtime self-probe) run against it, and at least one must fail; otherwise the suite would pass a broken
deployment. The row records which detectors caught it and whether the runtime self-probe was among them, because
the probe is all a running server has.

The runtime self-probe reads the probe table only. It catches a profile that is not read-only, a granted system
table, a missing profile and a base table granted in full; it does not see a policy dropped on another table. The
reconciler writes each project's policies again at least once an hour, and access step A2 restores the static
layer.

| Mutant | Break |
|---|---|
| D1 | Every policy of both layers dropped on one table. Under the permissive switch project A then reads every row of it, and the canary oracle must be what reports it |
| D2 | One restrictive policy dropped |
| D3 | A source granted (`READ ON URL` and `CREATE TEMPORARY TABLE`) |
| D4 | `readonly = 2` |
| D5 | A system log table granted |
| D6 | The settings profile detached from the user |
| D7 | A base table granted in full |
| D8 | The server switch flipped: the suite runs against a server whose switch is not the declared one |

## Findings

What the suite found on real servers, and what was done about each.

- **A row policy cannot name a user that does not exist** (code 511). Provisioning therefore creates the user
  first, without the role, then its policies, then the role grant. The user reads nothing until the grant.
- **A user can clear its own default roles, even in read-only mode** (`SET DEFAULT ROLE NONE TO CURRENT_USER`).
  The user then holds no role and reads nothing; no other identity is touched. Provisioning names the default
  role again, and the reconciler provisions every identity at least once an hour. The API cannot send the
  statement: the lexical check admits `SELECT` and `WITH` only.
- **A `SETTINGS` clause inside a subquery is accepted and ignored.** The value it names never takes effect.
- **A constant `SELECT 1` does not count against the quota; `EXPLAIN` does.** A run through the API is three
  statements (two validations and the run), so the quota's default of 1,800 statements an hour is about 600 runs.
  The operator setting `sql.statements_per_hour` changes it, and the refusal names the setting.
- **A probe is about twenty refused statements.** The probe identities are outside the hourly quota, so probing
  from several nodes cannot use up an error allowance and read as a probe that cannot run. The suite runs 21
  probes in a row at the real quota and checks that the probe identity has no quota usage.
- **From 26.8 the server leaves 15 settings changeable for a read-only user**: the query-string forms (`select`,
  `filter`, `sort`, `order`, `limit`, `offset`, `page`), the formats, `compression`, `database` and
  `implicit_table_at_top_level`. They shape the answer of the caller's own statement. W9 pins the list and
  checks that none shows another row; a setting that joins the list on a later version fails W9 until it is
  reviewed.
- **From 26.8 joins spill to disk by default, which also lifts the join row cap.** The profile pins
  `max_bytes_before_external_join` and `max_bytes_ratio_before_external_join` to 0 where the server knows them
  (26.3 does not), and the server brings the profile up to date at every start, so an upgrade needs no new
  migration step.
- **A granted source does not open by itself.** A table function over a source also needs `CREATE TEMPORARY
  TABLE`, and `readonly = 1` refuses it even then. D3 is caught by W0, which reads the grants.
- **Inside a definer-rights view `currentUser()` is the caller, and the row policies are the definer's.** A
  view-only grant set would have to filter by `currentUser()` itself. v0 keeps base-table grants (contract 7.4).
- **A start with a wrong master key once broke the workspace until a manual repair.** The ledger's checksum of
  access step A2 covered the probe identities' password digests, which come from the master key, so a start with
  a wrong key under `--migrate` applied A2 again and gave the probe identities that key's passwords; back on the
  right key the checksum matched again and the probe could not sign in. The checksum now leaves the digests out,
  `crucible migrate` and `serve --migrate` provision the probe identities again with the current key, and the
  identity reconciler does so on every pass while the probe has not passed. The suite's row "A start with a wrong
  master key" applies A2 under another key, shows that the probe can no longer sign in and that provisioning the
  probe identities again repairs it, and checks that A2's ledger form is the same under both keys.
- **Documented residual:** `system.tables`, `system.columns` and `EXPLAIN ESTIMATE` show whole-table totals for
  the base tables a project may read. They show volume, never rows.

## Results

Head `4781e2b`, run with `crates/crucible-conformance/scripts/sqliso.sh` (seed 1, 5,000 statements of layer B under the
strict switch and 1,000 under the permissive one, 20 extra identities in W14) and with layer C against the same
three versions. Server versions: 26.3 is 26.3.66.4, 26.8 is 26.8.39.19, latest is 26.10.4.207. Each cell is the result under the strict switch, then under the
permissive one. W15 needs a ClickHouse Cloud service and was not run.

### Layer A: direct attacks

| Row | 26.3 | 26.8 | latest |
|---|---|---|---|
| sqliso bootstrap | pass / pass | pass / pass | pass / pass |
| W0 deployment as declared | pass / pass | pass / pass | pass / pass |
| Runtime self-probe (7.5) | pass / pass | pass / pass | pass / pass |
| W8 quota exhaustion | pass / pass | pass / pass | pass / pass |
| Quota lifted for the run | pass / pass | pass / pass | pass / pass |
| W1 cross-project reads | pass / pass | pass / pass | pass / pass |
| W2 table functions | pass / pass | pass / pass | pass / pass |
| W3 system tables | pass / pass | pass / pass | pass / pass |
| W4 setting changes | pass / pass | pass / pass | pass / pass |
| W5 writes and DDL | pass / pass | pass / pass | pass / pass |
| W6 every function | pass / pass | pass / pass | pass / pass |
| W7 resource guards | pass / pass | pass / pass | pass / pass |
| W9 changeable_in_readonly | pass / pass | pass / pass | pass / pass |
| W10 error text, EXPLAIN and caches | pass / pass | pass / pass | pass / pass |
| W11 lifecycle races | pass / pass | pass / pass | pass / pass |
| W12 definer-rights views | pass / pass | pass / pass | pass / pass |
| W13 window backstops through views | pass / pass | pass / pass | pass / pass |
| W14 many identities | pass / pass | pass / pass | pass / pass |
| W15 Cloud specifics | not run / not run | not run / not run | not run / not run |
| Admin ping after the suite | pass / pass | pass / pass | pass / pass |

### Layer B: grammar fuzzing

| Row | 26.3 | 26.8 | latest |
|---|---|---|---|
| Layer B grammar fuzzing with canary oracles | pass / pass | pass / pass | pass / pass |

### Layer D: broken deployments

| Row | 26.3 | 26.8 | latest |
|---|---|---|---|
| D1 mutant: policies dropped on one table | pass / pass | pass / pass | pass / pass |
| D2 mutant: one restrictive policy dropped | pass / pass | pass / pass | pass / pass |
| D3 mutant: a source granted | pass / pass | pass / pass | pass / pass |
| D4 mutant: readonly = 2 | pass / pass | pass / pass | pass / pass |
| D5 mutant: a system log table granted | pass / pass | pass / pass | pass / pass |
| D6 mutant: profile missing | pass / pass | pass / pass | pass / pass |
| D7 mutant: base table granted in full | pass / pass | pass / pass | pass / pass |
| Layer D: detectors pass after repair | pass / pass | pass / pass | pass / pass |
| D8 mutant: the server switch flipped | pass | pass | pass |

### What each version showed (strict switch)

| | 26.3 | 26.8 | latest |
|---|---|---|---|
| W1 statements | 495 | 495 | 495 |
| W2 table functions: listed, refused, benign | 88, 82, 6 | 94, 87, 7 | 99, 92, 7 |
| W3 system tables: listed, refused, readable | 164, 143, 21 | 180, 157, 23 | 192, 169, 23 |
| W4 attempts | 41 | 41 | 41 |
| W5 statements refused | 55 | 55 | 55 |
| W6 functions, calls | 1789, 10734 | 1889, 11334 | 2008, 12048 |
| W9 settings the server leaves changeable | 0 | 15 | 15 |
| W14 provisioning per identity | 70 ms | 48 ms | 54 ms |
| Layer B: answered, refused | 1555, 3445 | 1553, 3447 | 1607, 3393 |

### Which detectors caught each mutant

The same on all three versions unless a cell says otherwise. "Probe" means the runtime self-probe was among
them; "leak" means the canary oracle reported another project's canary.

| Mutant | Strict switch | Permissive switch |
|---|---|---|
| D1 mutant: policies dropped on one table | W0, W1, W4 | W0, W1, W4; leak |
| D2 mutant: one restrictive policy dropped | W0 | W0 |
| D3 mutant: a source granted | W0 | W0 |
| D4 mutant: readonly = 2 | W0, probe, W4 | W0, probe, W4 |
| D5 mutant: a system log table granted | W0, probe, W5 | W0, probe, W5 |
| D6 mutant: profile missing | W0, probe, W4 | W0, probe, W4 |
| D7 mutant: base table granted in full | W0, probe, W1, W2; leak | W0, probe, W1, W2; leak |
| D8 mutant: the server switch flipped | W0, on all three versions | |

### Layer C: through the API

| | 26.3 | 26.8 | latest |
|---|---|---|---|
| `the_sql_workspace_holds_through_the_api` | pass | pass | pass |
| Audit rows of project A | 31 | 31 | 31 |
| Cancellation observed before the statement ended | true | true | true |
| A dropped connection cancelled its statement | true | true | true |

Layer C ran against servers with the shipped (permissive) switch.

## Running it

```sh
# Layers A, B and D on each version, under both values of the server switch (Docker, about 12 minutes a version).
crates/crucible-conformance/scripts/sqliso.sh            # 26.3, 26.8 and latest
crates/crucible-conformance/scripts/sqliso.sh 26.8       # one version
SQLISO_FUZZ=500000 SQLISO_IDENTITIES=2000 crates/crucible-conformance/scripts/sqliso.sh latest   # the nightly sizes

# One server you started yourself.
cargo run -p crucible-conformance --bin crucible-sqliso -- --url http://localhost:8123 \
  --user crucible_admin --password ... --switch permissive --layers a,b,d --fuzz 5000 --seed 7

# Layer C, then the master-key test: a ClickHouse, an empty Postgres database and the server binary built on its
# own. The master-key test starts the server with the key layer C migrated with.
cargo build -p crucible && cp target/debug/crucible /tmp/crucible
export CRUCIBLE_SQLISO_CLICKHOUSE_URL=http://localhost:8123 CRUCIBLE_SQLISO_CLICKHOUSE_PASSWORD=... \
  CRUCIBLE_SQLISO_POSTGRES_URL=postgres://crucible:...@localhost:5432/crucible CRUCIBLE_SQLISO_BIN=/tmp/crucible
cargo test -p crucible --test sqliso_api -- --nocapture
cargo test -p crucible --test sqlws_master_key -- --nocapture

# The fuzz targets.
(cd crates/crucible-sqlws && cargo +nightly fuzz run lexer -- -max_total_time=60)
(cd crates/crucible-store-clickhouse && cargo +nightly fuzz run grid -- -max_total_time=60)
```

`--switch` declares the server's value of `users_without_row_policies_can_read_rows`; the suite fails on W0 when
the server behaves otherwise. The suite lifts the hourly quota after W8 has tested it at its real limits, because
the later rows send thousands of statements as one identity; access step A2 restores the limits when it is
applied again.
