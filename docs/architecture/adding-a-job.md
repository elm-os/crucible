# Adding a background job

How a track adds a background job: what it declares, the one line that registers it, the rules its run function
keeps, and the checks that prove it. The binding rules are in [`contracts.md`](contracts.md): leases, fencing and
claimed work (11.7), the tables (11.1), the `Env` traits (2.1 to 2.3) and the control store (2.7). The runners are
the crate `crucible-jobs`; how they are verified is in [`../testing/jobs-simulation.md`](../testing/jobs-simulation.md).

## The three kinds

A job declares its kind, its cadence and an async run function.

| Kind | Constructor | Runs on | Needs Postgres | The runner calls `run` |
|---|---|---|---|---|
| Per-node timer | `Job::per_node(name, cadence, run)` | Every node | No | At the start, then once per cadence |
| Leader job | `Job::leader(name, lease, cadence, run)` | The node that holds the lease row `lease` | Yes | When the node takes the lease, then once per cadence while it holds it |
| Queue consumer | `Job::claimed(name, queue, cadence, policy, run)` | Every node | Yes | Once per claimed row; the queue is polled once per cadence and drained one row at a time |

- `name` is the task name and the `job` field of every log line.
- `Cadence::every(period)` adds a random extra of up to a tenth of the period, drawn from the `Env`'s `Rng`, so the
  nodes of a deployment do not wake together. `with_jitter` sets another bound.
- `lease` is one of the lease names of contract 11.7 (`LeaseName`). A new lease name, like a new `Queue`, is a
  contract change.
- A node without Postgres starts only the per-node timers and says so once in its log. Readiness does not depend on
  any job.

## Register it

The registry of the binary is `registry()` in `crates/crucible/src/jobs.rs`. A job is one line there:

```rust
registry.register(crucible_jobs::housekeeping::job());
```

The job itself lives in the crate of its feature (alerts in `crucible-alerts`, and so on), which exposes a function
that returns the `Job`. That crate depends on `crucible-jobs`, and the edge goes into the table of contract 1.5 with
the pull request that adds it.

## Rules for a leader job

The run function gets a `LeaderCtx`. The runner has taken the lease, renews it every 5 s and trusts it only until
the local acquire or renew time plus the TTL minus 2 s.

1. **Every write to Postgres is fenced.** Use `ctx.fenced(|tx| Box::pin(async move { .. }))`, which runs the
   closure in one transaction that first locks the lease row and compares the fencing token, or
   `ctx.set_cursor(..)`, which is one fenced statement. `ctx.store()` is for reads. A write through it is not
   fenced.
2. **Every action on ClickHouse goes through `ctx.issue(record, action)`.** `record` writes what is about to be
   done in a fenced transaction; `action` starts only after that has committed and while the local window is
   still open. The action must be idempotent: the record can commit and the node stop before the action runs, and
   the next leader then issues it again from the record.
3. **The run can be dropped at any await point.** When the local window ends, or a fenced write is refused, the
   runner drops the future and takes the lease again later. Keep no state that a dropped run would leave
   half-written outside a fenced transaction.
4. **A run is idempotent.** A new leader runs every job of its lease once at the start, so a job runs more often
   than its cadence when leadership moves. A job that must not repeat work within a period stores its progress in
   the job cursor (`ctx.cursor()`, `ctx.set_cursor(..)`). The cursor row is keyed by the lease name, so the jobs
   that share a lease share one cursor.
5. **Keep fenced transactions short.** The fence holds a shared lock on the lease row, and the renewal waits for
   it. A transaction is capped at 10 s (contract 2.7), and when the local window ends the runner drops the run and
   with it the open transaction, which rolls back.
6. **Return errors, do not retry in a loop.** A failed run is logged with the job's name and the job runs again at
   its next cadence. `JobError::LeaseLost` is not a failure of the job.
7. **Bound every wait.** The runner sets no time limit on a run. A run that never returns keeps the other jobs of
   its lease from running, so wrap a wait that could hang in `ctx.env.timeout(..)`.

Jobs that share a lease name run one at a time, in the order they come due.

On shutdown no new run starts. The run in flight may end within 5 s while the lease stays valid, and then the
lease is released. A run that is still going after 5 s is dropped and its lease is left to expire, because an
action of it may still be on its way.

## Rules for a queue consumer

The run function gets a `ClaimCtx` with one claimed row (`ctx.claim`: id, payload, attempt counter).

1. **The handler is idempotent.** A row is handled at least once: the claim can run out, or the node can stop,
   after the effect and before the completion is recorded, and then the row is claimed and handled again. Give
   every external effect a key that the receiver can deduplicate on (the outbox uses its `dedup_key`).
2. **`Ok(())` completes the row. `Err` gives it back** with a backoff of 5 s after the first attempt, doubling to
   at most 1 h, less a random tenth. The retry time is computed on the database clock, so a node whose wall clock
   is off does not move it.
3. **The poison cap parks the row.** A row that fails its `ClaimPolicy::max_attempts`-th attempt (default 12), or
   whose earlier attempts all died without an answer, gets the status `parked`: it stays in its table, is never
   claimed again, and the runner logs a warning with the queue, the row id and the last error.
4. **The handler is stopped 2 s before its claim ends** (`ClaimPolicy::lease_for`, default 60 s) and the row is
   given back with the backoff. Choose a lease longer than the slowest handler run.
5. **Enqueue in the transaction of the state change** (`tx.claims().enqueue(..)`), with a `dedup_key` that makes
   a repeated enqueue a unique violation instead of a second row.

On shutdown a handler in flight is dropped; its claim expires and another node takes the row.

## Determinism

All timing is on `ctx.env.clock`, every task on the `Env`'s `Spawner`, every random draw from a fork of the `Env`'s
`Rng`. A job never reads ambient time, spawns on the ambient runtime or keeps state in a static; the lints of the
workspace refuse the ambient APIs.

## Checklist

1. **The job.** A function in the feature's crate that returns `Job`, and its run function.

   Verify: unit tests of the pure parts; `cargo nextest run -p <crate>`.

2. **New SQL.** A statement the job needs becomes a method of a repository trait in `crucible-control`, with its
   Postgres implementation in `crucible-control-pg` and its model in `crucible_sim::pg_model`. The model may be
   stricter than Postgres, never more permissive.

   Verify: a line for each new statement in a script under `crates/crucible-conformance/src/control/`, its
   snapshot in `tests/control_model.rs`, and the differential green on Postgres 15 and 18
   (`crucible-pg-conformance`, see [`../testing/control-store.md`](../testing/control-store.md)).

3. **The registration line** in `crates/crucible/src/jobs.rs`, and the name in the test
   `the_registry_holds_the_housekeeping_job` there. A job that needs services the binary wires (alerting needs
   the query service, the egress policy and the mailer) is built where they are wired and passed to `registry`,
   as `crates/crucible/src/alerts.rs` does.

4. **A scenario on the virtual clock.** Run the job through the real registry over `PgModel`, as
   `crates/crucible-sim/tests/jobs_directed.rs` does for housekeeping: seed the rows, start the registry on a
   simulated host, advance the clock, assert what the job changed and what it left alone.

   Verify: `cargo nextest run -p crucible-sim --test jobs_directed`.

5. **The jobs simulation.** A leader job that writes to Postgres joins the registry of
   `crucible_sim::jobs_sim` with an audit of what it must have done by the end of the fault-free tail, as the
   housekeeping job has. A job with an invariant of its own gets its own oracle there.

   Verify: `cargo nextest run -p crucible-sim --test jobs_dst`.
