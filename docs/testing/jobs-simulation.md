# Jobs simulation

How the job runners of `crucible-jobs` (contract 11.7) are verified: the leader runner, the claimed-work runner,
the per-node timers and the housekeeping job. The method is in [`methodology.md`](methodology.md); the store model
and the lease protocol below the runners are in [`control-store.md`](control-store.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Runners | `crates/crucible-jobs` | The registry, the leader runner, the claimed-work runner, housekeeping |
| Property tests | `crates/crucible-jobs/src/cadence.rs` | The backoff and the cadence |
| Simulation | `crucible_sim::jobs_sim`, `crates/crucible-sim/tests/jobs_dst.rs` | The real runners on several nodes over `PgModel` under faults, with the oracle and the mutants |
| Directed scenarios | `crates/crucible-sim/tests/jobs_directed.rs` | One behaviour each, on the virtual clock, with exact times |
| Differential | The `jobs` script of `crucible-conformance` | The two statements the runners added (park, key-feed prune) on `PgModel` and on Postgres 15 and 18 |

## What runs

Three nodes (five in one test) each start the real `Registry` with four jobs:

- a leader job under the `lifecycle` lease. Each run makes one fenced write of a value that names its fencing
  token and its holder, in one of three forms: the fenced cursor statement, a fenced transaction, or
  `LeaderCtx::issue`, which records in a fenced transaction and then issues an action;
- the real housekeeping job under the `housekeeping` lease;
- a consumer of the notification outbox with a 12 s claim. A handler works for up to 2.5 s; a flaky row fails its
  first two attempts; then the handler applies the row's effect;
- a per-node timer.

An action stands for a leader action on ClickHouse: it is on the network from the instant it is issued, lands up
to 1.5 s later whatever happens to its sender, and is checked when it lands.

A workload enqueues a row every few seconds and creates API keys, which write key-feed rows. The store starts with
sessions that expired an hour ago and sessions that expire in a day.

## Faults

The harness reaches a node only through its `Env` and its store handle, so the runners are the production code.

| Fault | How it is injected |
|---|---|
| Crash | Every task of the node is aborted at its await point. The node starts again up to 10 s later as a new process with a new holder name |
| Freeze, up to 25 s (longer than the 15 s TTL) | No sleep of the node wakes, and no store call of the node starts or returns, until the thaw |
| Clock skew | Each node's wall clock is up to 5 min off the database's, the bound of the realistic tier |
| Connection loss | The store fails over for up to 8 s, loses commits with an unknown outcome (applied or not), aborts transactions, and delays commits by up to 1 s. One node's reserved pool, which carries the lease statements, is gone for 5 to 20 s while its transactions still get through |
| Lease steal | The lease row is rewritten for another holder with the next token while its holder's local window is open, as an operator's forced expiry would do. The thief writes a fenced cursor, then releases the lease or lets it expire |
| Graceful restart | The shutdown path: the run in flight ends, the lease is released |

The faulty phase lasts 10 min of virtual time. A fault-free tail of 7 min follows, and at its end every node is
stopped at the same instant.

A freeze takes effect where the node waits on its clock or on the store. Between a validity check and the start
of an action there is no such point, so the simulation does not explore a process pause inside that gap; contract
11.7 covers it by requiring leader actions to be idempotent and recorded before they are issued.

## The oracle

| Invariant | Check |
|---|---|
| J1: fenced leader writes never interleave between two holders, and each carries the token current at its commit | Every committed cursor names the fencing token and the holder that the lease row carried at that commit (`PgModel` records both at the commit). A lease row's token never goes back and changes holder only with the next token. Every acknowledged cursor was committed. An action lands only while its lease row still carries its token and has not expired on the database clock; a stolen lease is exempt, because no local window can know of a steal |
| J2: a claimed row's effect is applied at least once, and its completion is recorded at most once | A completion is acknowledged at most once per row, only to the claim whose attempt the row's counter holds, and only after the row's effect was applied. Every row that is done at the end had its effect applied |
| J3: after the faults heal, every enqueued row completes and every lease is eventually held | At the end of the tail every row of the queue is done, both leases are held and unexpired, and leader writes were acknowledged in the tail. After the shutdown both lease rows are released |
| Housekeeping | At the end no key-feed row is left (all are older than the horizon), the expired sessions are gone and the live ones are not |
| The local window is safe | In the profile without steals no leader run ends with a refused fenced write: the runner stops the work first |

The suite fails unless crashes, graceful restarts, freezes longer than the TTL, reserved-pool outages, failovers,
takeovers, all three write forms, landed actions, unknown commits, failed runs, retries, handler failures, stale
completions and released leases all occurred. The profile with steals also requires steals and fenced refusals.

## Mutants

Each seeded defect must be caught, and the test pins one catching seed.

| Mutant | Where the defect is | What the oracle reports | Catching seeds |
|---|---|---|---|
| Skip the local validity margin | `Registry::skip_validity_margin` (feature `test-mutants` of `crucible-jobs`): the local window is the whole TTL | J1: an action landed after its lease had expired | 20 of seeds 0..40 without steals: 3, 7, 9, 10, 12, 14, 15, 18, 20, 22, 23, 26, 27, 29, 31, 32, 33, 35, 38, 39. Pinned: 3 |
| Renew without compare-and-set | `PgMutant::RenewWithoutCompareAndSet` in `PgModel`: `renew` writes the caller's lease back over whatever the row holds | J1: the lease row's token went back | 19 of seeds 0..20 with steals (all but 1). Pinned: 0 |
| Complete without checking the claim | `PgMutant::CompleteWithoutClaimCheck` in `PgModel`: `complete` ignores the attempt counter and the status | J2: a completion recorded twice, or by an attempt the row no longer holds | 7 of seeds 0..20: 0, 4, 5, 11, 13, 16, 19. Pinned: 0 |

The margin mutant needs a leader whose renewals fail while its transactions still commit: that is what the
reserved-pool outage is for. With one shared monotonic clock and a working fence, nothing else distinguishes a
window of the TTL from one of the TTL minus 2 s.

## Directed scenarios

| Scenario | What it pins |
|---|---|
| Release and takeover | A leader renews for four TTLs without a takeover; on shutdown its lease row is released, and the other node leads within 7 s under the next token |
| The end of the window | With every store call failing from 20.5 s, work under a lease last renewed at 20 s stops at 33 s; after the outage the node works again under the next token |
| Shutdown with a run in flight | A 3 s run ends and the lease is released; a 30 s run is cut after 5 s and its lease is left to expire |
| Backoff and parking | A row that always fails is retried after 5, 10 and 20 s (less up to a tenth, plus at most one poll) and parked at a cap of 4, with the node's wall clock 5 min ahead of, behind and equal to the database's |
| Dead attempts | A row whose attempts all ended without an answer is parked without its handler running |
| Housekeeping | Feed rows are kept for 75 s and pruned by the pass after; a session that expired within the 10 min grace stays; a row written later outlives the pass |
| No control store | Only the per-node timer starts, runs at the start and once per cadence, and stops on shutdown |

## Running it

```sh
cargo nextest run -p crucible-jobs
cargo nextest run -p crucible-sim --test jobs_dst --test jobs_directed
```

Replay one seed (`CRUCIBLE_SIM_PROFILE=no-steals` selects the profile without steals):

```sh
CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test jobs_dst replay_seed --run-ignored all --no-capture
```
