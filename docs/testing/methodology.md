# Testing methodology

Crucible is verified empirically and formally. There is no review-based gate: a change is done when its checks pass.

## Deterministic simulation

- Every source of nondeterminism in product crates (time, randomness, task scheduling, disk, network, ClickHouse,
  Postgres) goes through the `Env` traits in `crucible-env`.
- In simulation they are seeded: one seed fixes the task schedule, the random streams and every fault decision, so a
  failing seed replays exactly (`cargo xtask sim replay --seed <n>`).
- The network and host lifecycle run on turmoil; disk, ClickHouse and Postgres are in-process models with seeded fault
  injection (ENOSPC, EIO, torn writes, fsync lies, lost acks, timeouts, outages, crashes at any await point).
- A virtual clock runs hours of traffic in seconds, inside a realism envelope: a violation only blocks a merge when it
  is reachable under realistic timing.
- Invariants are checked continuously and at quiescence. The catalogue (I1 to I14) lives in
  [`invariants.md`](invariants.md); examples: no acknowledged span is lost, rows land exactly once, an acknowledged span
  stays readable across restarts, projects never see each other's data.

## Swarm testing

Each simulation configuration flips an independent coin for about 34 dimensions (workload features, fault types and
config flags), so some runs leave out the features that would otherwise hide a bug. With 200 configurations per pull
request, any 6 specific features appear together in at least one run with 95.7% probability; with 5,000 nightly
configurations, any 10 do with 99.2% probability.

## Formal methods

- **Quint** (`specs/ingest.qnt`) models receive, append, sync, ack, seal, insert, checkpoint, crash and recovery, and
  checks that no acknowledged span is lost, no row is duplicated and the WAL is truncated only below the checkpoint.
- **Kani** proves pure kernels: the WAL frame codec, batch-token derivation, backpressure accounting, the cursor codec,
  partition routing arithmetic and the MCP error classifier.

## Conformance

The in-memory ClickHouse and Postgres models may be stricter than the real systems but never more permissive.
Differential scripts run the same operations against the models, ClickHouse 26.3 LTS, 26.8 LTS and the latest stable
release in Docker, and ClickHouse Cloud. A mismatch means the model is fixed first.

## Other test layers

- Property tests (proptest) for decoders, mappers and the plan-to-SQL renderer.
- Fuzz targets (cargo-fuzz) for every parser that sees untrusted bytes.
- Playwright for the UI, covering the loading, success, error and empty state of every section.
- A shared SDK conformance suite that all four SDKs and the proxy must pass.
- Performance gates: interleaved A/B benchmarks on fixed containers; relative regressions block merges
  ([`perf.md`](perf.md)).
