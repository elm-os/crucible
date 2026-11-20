# Testing and verification

How Crucible is verified. [`methodology.md`](methodology.md) is the overview; [`invariants.md`](invariants.md) is the
catalogue of properties the checks hold the system to. [CONTRIBUTING.md](../../CONTRIBUTING.md) lists the commands
that run them.

| Page | What it covers |
|---|---|
| [Methodology](methodology.md) | Deterministic simulation, swarm testing, formal methods, conformance and the other test layers |
| [Invariants](invariants.md) | The invariant catalogue (I1 to I14) and the conformance rows the code relies on |
| [Formal model](formal-model.md) | The Quint model of the ingest protocol (`specs/ingest.qnt`), its properties, mutants and how to run it |
| [System simulation](system-simulation.md) | The whole node in simulation with swarm testing: dimensions, the oracle, the corpus and replaying a failing run |
| [Swarm report](swarm-report.md) | The per-dimension trigger and suppressor summary written by `cargo xtask sim report` |
| [WAL simulation](wal-simulation.md) | `crucible-wal` under disk faults and crashes |
| [Shipper simulation](shipper-simulation.md) | `crucible-shipper` and the insert protocol under ClickHouse faults |
| [Jobs simulation](jobs-simulation.md) | The leader, claimed-work and per-node runners of `crucible-jobs` |
| [Alerts simulation](alerts-simulation.md) | Alert schedules, series states, evaluation, the outbox and channels |
| [Lifecycle simulation](lifecycle-simulation.md) | Retention, the mask scheduler and deletion requests |
| [Control store](control-store.md) | The Postgres adapter against its model, and the lease protocol |
| [SQL workspace isolation](sqliso.md) | The `sqliso` suite: isolation between projects through ClickHouse access control |
| [SDK conformance](sdk-conformance.md) | The shared suite the four SDKs pass: the driver, the runner contract, scenarios and goldens |
| [UI surfaces](ui-surfaces.md) | Every UI section in its loading, success, error and empty states, and the Playwright tests that pin them |
| [Performance rig](perf.md) | The A/B gate, the load generator and the fsync-honesty probe |
| [Throughput baseline](perf-baseline.md) | A first end-to-end measurement of `crucible serve` on a laptop |
