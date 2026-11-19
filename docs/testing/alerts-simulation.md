# Alerts simulation

How alerting (contracts 11.7 and 12.6) is verified: the schedule, the state machine of a series, the evaluation
that applies a tick, the outbox consumer and the channels. The method is in [`methodology.md`](methodology.md); the
job runners below it are in [`jobs-simulation.md`](jobs-simulation.md) and the store model in
[`control-store.md`](control-store.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Schedule | `crates/crucible-alerts/src/schedule.rs` | Property tests: the offset of a rule inside its interval is a function of its id, the current tick is the latest one due, the window is whole minutes and ends 30 to 90 s before the tick |
| State machine | `crates/crucible-alerts/src/machine.rs` | Example and property tests of the pure step function |
| Egress policy | `crates/crucible-egress/tests/egress_props.rs` | Property tests: no spelling of a metadata address passes under any policy, strict mode refuses every internal address, no resolution or redirect path reaches a metadata address, a resolver that changes its answer cannot move the call |
| Simulation | `crucible_sim::alerts_sim`, `crates/crucible-sim/tests/alerts_dst.rs` | The real scheduler, evaluations consumer and outbox consumer on several nodes over `PgModel` and over rollup rows in `ChModel`, under faults, with the oracle and the mutants |
| Differential | The `alerts` script of `crucible-conformance` | Every new statement (channels, rules, series state, history, the queue of evaluations, pruning of done queue rows) on `PgModel` and on Postgres 15 and 18 |
| API | `crates/crucible-api/tests/api/alerts.rs` | The routes over `PgModel`: versions, field errors, the preview and the history, secrets shown once, the egress refusal, the test-send and its rate limit; the route walk checks each route's permission |

## What runs

Two nodes (three in one test) each start the registry with the jobs the binary registers: the scheduler, the
consumer of the queue of evaluations and the consumer of the notification outbox.

The telemetry is a seeded scenario. Per minute and environment (`prod`, `staging`, `dev`) there is calm traffic,
traffic with errors, or no rows at all, in phases of 3 to 10 minutes. It is written to `ChModel` before the nodes
start, as the blocks the shipper builds for `span_metrics`, so what an evaluation reads is a function of its
window alone and the oracle can compute it again. The evaluations read through the production source: the metrics
registry, a typed plan of class `alert`, the query service.

Four rules watch it:

| Rule | Measure | Series | `for` | No data | Channels |
|---|---|---|---|---|---|
| fast | errors above 5 over 1 min | per environment | 0 | keep | webhook and email |
| patient | errors above 8 over 2 min | per environment | 3 min | `no_data` after 3 min | webhook |
| quiet | errors above 5 over 1 min, every 2 min | per environment | 0 | `ok` after 4 min | email |
| volume | spans below 30 over 1 min | one | 1 min | keep | webhook |

Email goes through the production transport, which renders the message, to a simulated mail server (contract
2.11) with seeded transient and permanent failures. Webhooks go to a scripted receiver.

## Faults

| Fault | How it is injected |
|---|---|
| Crash | Every task of a node is aborted at its await point. The node starts again at once, within 20 s, or after 2 to 7 min, so at times no node runs |
| Time skip | The wall clock of the database steps forward by 30 s to 4 min, at most 15 min in a run. Ticks pass that nobody evaluated |
| Clock skew | Each node's wall clock is up to 5 min off the database's |
| Postgres | Failovers of up to 8 s, commits with an unknown outcome, aborted transactions, spurious serialization failures, slow commits |
| ClickHouse | Unreachable for 5 to 90 s; one outage in four lasts 7 to 10 min, long enough for a rule to report that its checks keep failing |
| Receivers | The webhook receiver and the mail server are down for 30 s to 10 min at a time and refuse 3% of what reaches them. In half of the runs the webhook receiver is down from the fifth minute to the end of the phase, and its channel is turned off after 50 failures in a row |

The faulty phase lasts 45 min of virtual time. Then everything heals, an administrator turns the channels that
were turned off on again through the production channel service, and a fault-free tail of 95 min follows in which
the scenario is steady: `prod` breaches, `staging` is silent and `dev` is calm. The tail covers the largest clock
skip, the longest retry backoff of the outbox (1 h) and several repeats.

## The oracle

| Invariant | Check |
|---|---|
| A1: the outbox is exactly-once per transition | For every history row that says it notified and every channel of its rule there is one outbox row, found by rule, tick, kind, channel and series; the outbox holds no other row and no two rows for one notice; every delivered notification is in an outbox row |
| A2: the state machine | The value of every history row is the value of its window in the scenario. A series fires only on a value that breaches. In a rule with a `for` duration a series fires only from `pending` and only after that duration, except from `resolved`. At the end of the tail `prod` fires in every rule with the environment dimension, `dev` is ok, and `staging` is in the state its no-data policy gives: `no_data`, ok, or anything but `no_data` under `keep` |
| A3: a firing never goes unnotified once a channel heals | At the end of the tail every outbox row older than two minutes is done. A done row was delivered at least once, unless its channel was turned off. Every series that fires has, on every channel of its rule, a delivered firing notice of its current episode, and one in the last `renotify` plus three intervals when it has fired for longer than that |

A notification that comes due while its channel is turned off is dropped, by design: an administrator sees the
reason on the channel. A3 holds across that because a series that still fires is notified again within
`renotify` of the channel being turned on. The runs check that this happened: channels were turned off and rows
were dropped in the sweep.

A sweep also requires that the interesting cases occurred: crashes, long downtimes, time skips, failovers, store
outages, failed reads, skipped ticks, repeats, failed and repeated deliveries, mail, channels turned off, series
that fire and series in `no_data` at the end, and a rule that reported failing checks.

## Mutants

Each is a seeded defect behind the feature `test-mutants` of `crucible-alerts`, which only the simulation enables.

| Mutant | Caught by |
|---|---|
| Notify without the outbox transaction: the outbox rows are written after the transition commits | A1: a failed statement, a lost connection or a crash between the two leaves a history row without its outbox rows |
| Skip the `for` duration: a breach fires at once | A2: the patient rule fires from ok |
| Ignore the no-data policy: a series without rows keeps its state | A2: `staging` is not in the state its policy gives at the end of the tail |

## Commands

```sh
cargo nextest run -p crucible-sim --test alerts_dst
cargo nextest run -p crucible-alerts -p crucible-notify -p crucible-egress
cargo nextest run -p crucible-conformance --test control_model
```

Replay one seed:

```sh
CRUCIBLE_SIM_SEED=<n> cargo nextest run -p crucible-sim --test alerts_dst replay_seed --run-ignored all --no-capture
```
