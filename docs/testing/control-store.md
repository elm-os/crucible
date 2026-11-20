# Control store verification

How the control plane's store (contract 2.7) is verified: the Postgres adapter `crucible-control-pg`, the
simulation model `crucible_sim::pg_model::PgModel`, and the lease protocol of contract 11.7. The method is in
[`methodology.md`](methodology.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Traits and types | `crates/crucible-control` | `ControlStore`, the repositories, `TxError`, `HeldLease` (the holder's local window) |
| Adapter | `crates/crucible-control-pg` | tokio-postgres over `env.net`, two pools, forward-only migrations |
| Model | `crucible_sim::pg_model` | The simulated Postgres and its seeded faults |
| Scripts | `crates/crucible-conformance/src/control` | Repository, transaction, key-feed, lease, claim, pool and job scripts over any `ControlStore`; each writes a transcript |
| Model snapshots | `crates/crucible-conformance/tests/control_model.rs` | The transcripts of the scripts against `PgModel`, pinned |
| Differential | `crucible-pg-conformance` (`crates/crucible-conformance/src/control_pg.rs`) | The same scripts against a real server; the transcript must equal the model's |
| Lease simulation | `crucible_sim::lease_sim`, `crates/crucible-sim/tests/lease_dst.rs` | Leader election over `PgModel` under faults, with its oracle and its mutant |

## What `PgModel` models

- Snapshot transactions: a transaction reads the snapshot of its first statement and its own writes. A write to a
  row that changed since the snapshot, or that another open transaction has locked, is a serialization failure.
  Postgres would wait for the lock and fail only if the other transaction commits, so the model is stricter.
- Unique and foreign-key constraints under the names of the production schema.
- A commit whose outcome is unknown (applied or not, by a coin), calls that fail as unavailable, spurious
  serialization failures, and slow commits that hold their locks and their sequence numbers.
- Two pools with their sizes; an exhausted pool fails the call as unavailable after the acquire timeout.
- Failover: every open transaction is lost and every call fails for a while; afterwards every acknowledged commit
  is still there.
- Lease rows with fencing tokens, job cursors, and claimed rows with `lease_until`, an attempt counter and the
  statuses pending, done and parked.
- Key-feed sequence numbers that are drawn at the write and survive a rollback, so they become visible out of
  order and leave gaps.
- A database clock that a test steps against the node clocks.

The model may be stricter than Postgres and is never more permissive: that is what the differential checks.

## The differential

```sh
docker network create crucible-pg
docker run -d --rm --name crucible-pg18 --network crucible-pg --memory=768m \
  -e POSTGRES_PASSWORD=crucible -e POSTGRES_DB=crucible postgres:18
cargo run -p crucible-conformance --bin crucible-pg-conformance -- \
  --url postgres://postgres:crucible@crucible-pg18:5432/crucible --label 18
```

The database must be empty or hold only Crucible's tables; the suite empties them. Each row prints `PASS` or
`FAIL`. The workflow `pg-conformance.yml` runs it against Postgres 15 (the floor) and 18 on pull requests that
touch the control crates, the model or the workflow, and nightly.

| Row | What it checks |
|---|---|
| PG1 | The migrations apply once, are recorded in the ledger, and a second run is a no-op |
| PG2 | Both timeouts are 10 s on pooled connections and in transactions; transactions run at repeatable read |
| PgModel differential (13 rows) | Each script's transcript against the server equals the model's. The `jobs` row covers the statements of the job runners: parking a claimed row and pruning the key feed. The `repositories` row covers `project_salts`: the first salt of a project stays and a second insert changes nothing, a salt of a project that does not exist is refused by `project_salts_project_id_fkey`, the list is ordered by project, and a delete removes the salt once. The `alerts` row covers channels (with the failure count that turns one off in one statement), alert rules, the state of their series, their history, the queue of evaluations and the pruning of done queue rows. The `lifecycle` row covers the node registry, deletion requests with their idempotency key and acknowledgements, the ledger of leader actions and the retention floors. The `dashboards` row covers dashboards with their widgets: one statement per write, a stale or cross-project version that changes nothing, a widget set rewritten in place (kept, changed, dropped and added in one write), widget ids unique within their dashboard, and widgets deleted with their dashboard. The `media` row covers media objects and their uses, and the foreign key that keeps a used object's row: a delete of the row that races a new use fails in both orders, as a foreign-key violation or a serialization failure as Postgres reports it, and the model's delete checks the uses committed after its snapshot as Postgres does |
| PG3 | After every connection is terminated, both pools replace their connections and every later call succeeds |
| PG4 | Deleting a project's row deletes its salt (`ON DELETE CASCADE`). No repository deletes a project's row yet, so the row has no model side |

Transcripts hold no raw database time and no raw feed sequence number, because those differ between two stores
that behave the same.

## The lease simulation

Three or five nodes compete for one lease over `PgModel`. A leader renews every 5 s, trusts its lease only inside
the local window of `HeldLease` (acquire or renew time plus the TTL minus 2 s), and writes as the leader in two
ways: a fenced single statement (the job cursor) and a fenced transaction (a ledger row). One round in eight the
leader stalls after its validity check, for up to 25 s, and writes anyway. Meanwhile the store fails over, loses
commits with an unknown outcome, aborts transactions, delays commits and steps its clock by up to 20 s either way.
A fault-free tail of two minutes follows.

The oracle:

| Property | Check |
|---|---|
| One leader per token | A token is acknowledged to one node; every committed leader write carries the token and holder of the lease row at its commit |
| Stale tokens are refused | No leader write is acknowledged when a higher token had been acknowledged before the write began |
| An unknown acquire never acts | No committed leader write carries a token that was never acknowledged |
| Acknowledged writes survive | Every acknowledged cursor and ledger row is in the store after the faults |
| The local window is safe | Without clock steps, a write begun inside the local window is never refused as fenced |
| Progress | Some node leads in the fault-free tail |

The suite fails unless takeovers, stalls past the window, fenced refusals, unknown commits (of acquisitions too),
serialization failures and failovers all occurred. The mutant `Options::unfenced` skips the transaction's fence;
the oracle must report the stale write.

Replay one seed:

```sh
CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test lease_dst replay_seed --run-ignored all --no-capture
```

The runners that work over these leases and claims (`crucible-jobs`) have their own simulation:
[`jobs-simulation.md`](jobs-simulation.md).
