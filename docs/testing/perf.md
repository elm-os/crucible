# Performance rig

Every absolute number on this page was **measured on a laptop, not reference hardware**. Merges are gated on
relative regressions between two builds measured on the same machine, never on absolute numbers. Reports that
quote absolute numbers carry the same label.

The rig has three parts:

- the fsync-honesty probe, which must classify the WAL volume before durable-ack latency can gate anything;
- the load generator, `crucible-loadgen`;
- the A/B gate, `cargo xtask perf gate`, which drives both.

## v0 targets

| Target | Value | Measured by |
|---|---|---|
| Ingest throughput per node | at least 250,000 spans/s of 1 KiB spans, and at least 50,000 spans/s of 10 KiB spans, sustained | the gate's `e2e` path (sustained) and `ack` path (throughput) |
| Durable-ack latency | p99 at most 20 ms, with `wal.durability = group` on a volume the probe classified `honest` | the gate's `ack` path (latency) |
| Acknowledged loss | zero (invariant I1) | every `e2e` arm |
| Freshness | at most 2 s p95 from ack to readable | the gate's `e2e` path (freshness) |
| Trace by id | at most 100 ms p95 | the read bench's `trace_by_id` lane |
| A 30-day trace list over 1 billion spans | at most 500 ms p95 | the read bench's `trace_list_30d` lane, measured at 100 million spans and labelled with its scale |
| The MCP tool dashboard | at most 300 ms p95 | the read bench, once the MCP analytics endpoints exist (below) |
| SDK and proxy overhead | at most 1 ms p99 | the SDK and proxy suites, not this rig |

## The rig

The rig is the development laptop: Docker Desktop on WSL2, inside the 16 GB WSL VM, whose memory limit stays
unchanged. Each container gets a disjoint cpuset (`--cpuset-cpus`) and a memory limit, so no two containers
compete for a core:

| Container | vCPU | Memory | Example cpuset on the 24-CPU VM |
|---|---|---|---|
| Crucible | 6 | 3 GB | 0-5 |
| ClickHouse | 10 | 9 GB | 6-15 |
| Postgres | 1 | 1 GB | 16 |
| `crucible-loadgen` | 6 | 1 GB | 17-22 |

Together these take 23 of the VM's 24 CPUs and 14 of its 16 GB. The remaining CPU and memory belong to the VM and
Docker. The gate stamps every result with the machine's AC-power state and with the probe verdict for the WAL
volume.

## Lanes

Results are reported per lane and never averaged across lanes:

- **Span size:** 1 KiB and 10 KiB spans, matching the two throughput targets.
- **Encoding:** uncompressed protobuf and gzip. Gzip costs the server an inflate per request, which the
  uncompressed lane does not measure.
- **Burst and sustained.** ClickHouse is the likely limiter on the laptop, so the ack path and the end-to-end
  path are measured separately:
  - burst runs are short enough that the WAL's unshipped budget never fills. They measure receive, admission,
    WAL group commit and ack, independent of ClickHouse insert speed;
  - sustained runs are long enough that the shipper must keep up. The sustained rate is the highest offered rate
    with no 429 or 503 responses and a bounded WAL backlog.

## The A/B gate

`cargo xtask perf gate` compares a base build (A) with a candidate (B) on the same rig:

```sh
cargo xtask perf gate --base main                          # the candidate is HEAD
cargo xtask perf gate --base main --candidate my-branch --lane 10kb --pairs 7 --post
cargo xtask perf gate --base main --path e2e --lane 1kb-gzip   # shipped spans/s and freshness, gzip bodies
```

| Flag | Default | Meaning |
|---|---|---|
| `--base` | required | The base ref (A) |
| `--candidate` | `HEAD` | The candidate ref (B) |
| `--path` | `ack` | `ack`: receive to durable acknowledgement (throughput and durable-ack p99). `e2e`: receive to readable in the store (sustained spans/s over a run and its drain, and freshness p95, each with the zero-loss check) |
| `--lane` | `1kb` | `1kb`: 1 KiB spans, 100 per request. `10kb`: 10 KiB spans, 10 per request, which keep the generator accurate ([`perf-baseline.md`](perf-baseline.md)). `1kb-gzip`, `10kb-gzip`: the same with gzip bodies, compressed on half the generator's CPUs |
| `--pairs` | 5 | Interleaved pairs: at least 5 on the full rig, at least 1 on the small rig, at most 100 |
| `--rig` | `full` | `full`: the rig above. `small`: reduced limits for smoke runs (below) |
| `--post` | off | Set the `perf-gate` commit status on the candidate commit through `gh`. Refused with `--rig small` |

### What it does

1. Resolves both refs to commits. The image of a commit is `crucible-gate:<commit>`: an existing image of that
   tag is reused, otherwise it is built with `deploy/docker/Dockerfile` from a clean export of the commit
   (`git archive`), so nothing uncommitted reaches a build and the git directory is only read. One commit gives
   one image for both arms. `CARGO_BUILD_JOBS`, when set, is passed to the build.
2. Builds `crucible-loadgen` in release from the checkout that runs the gate. That one binary measures both arms,
   and the report records its SHA-256.
3. Runs the fsync-honesty probe on a fresh WAL volume of the session, with Crucible's CPUs and memory.
4. Runs a discarded latency arm of A, then of B, so that neither image starts cold in a measured arm and the
   first measured arm follows the same kind of work as every later one. Then runs the pairs. In a pair, the first
   build runs the path's capacity measurement and then its latency measurement, and the second build the same,
   with the same lane and offered load: every capacity arm follows a latency arm and every latency arm follows
   its own build's capacity arm, whichever build goes first (see the A/A calibration below for why). The order
   alternates from pair to pair (AB, BA, AB, ...), so that slow drift in temperature or background load does not
   favour one arm. Every arm gets a fresh
   rig: ClickHouse, Postgres and Crucible (`serve --migrate`) from the arm's image on a new WAL volume; then the
   first owner and a key with the `ingest` and `read` scopes for the project `default` (`crucible admin init` and
   `crucible admin create-key`, run with the image's own binary, because with Postgres the keys live there and a
   keys file is refused), once the server accepts the key; then a discarded warm-up at the latency rate, then the
   measurement, after which the rig is removed. The load generator's container lasts the whole session.
   - Throughput: offer more load than either build can take. The generator is open loop, so the excess is
     refused or queued, and the measurement is `outcome.acked_spans_per_s`. An overloaded schedule lasts until
     every request is sent, so the run takes about `requests / capacity`, longer than its schedule. Acknowledging
     less than the offered load shows that something saturated, not that Crucible did: when the generator or the
     Docker network is the limit, both arms measure that limit and no regression above it can show. The load
     generator's report tells them apart. With Crucible as the limit its 128 connections stay busy (acknowledged
     requests per second times `service_time_us.p50` comes near 128) while `generator_cpu` stays below the
     generator's CPUs.
   - Latency: offer a fixed rate below both builds' capacity, and the measurement is `latency_us.p99`.
   - Sustained (`--path e2e`): offer more than the end-to-end path keeps up with, but less than the ack path
     takes, for a fixed time, then wait until `span_core` holds every acknowledged span (the drain). The
     measurement is acknowledged spans per second over the run and its drain: from the schedule's start
     (`started_at` of the generator's report) to the end of the last `span_core` insert, from
     `system.query_log`. Once the shipper saturates, the backlog drains at its rate, so the measurement is the
     end-to-end capacity whatever the schedule's length.
   - Freshness (`--path e2e`): offer a fixed rate below the end-to-end capacity, drain, and the measurement is
     the p95 over the run's spans of the time from `received_at` (stamped by the WAL committer just before the
     frame's append and sync) to the end of the `span_core` insert that made the span readable, matched through
     the insert's deduplication token in `system.query_log`.
   - Both end-to-end measurements start after the warm-up has drained, so no backlog carries into them, and
     both check zero loss (invariant I1) after the drain: the run's stored `loadgen.seq` values in
     `span_payload` must cover every acknowledged sequence number exactly once, and `span_core` must hold one
     row per span.
5. Compares the builds, writes the report and the summary, and with `--post` sets the commit status.

| Rig | Throughput schedule | Latency schedule | Sustained schedule | Freshness schedule | Drain limit | Warm-up | `--recent-max-bytes` |
|---|---|---|---|---|---|---|---|
| `full` | 20,000 requests/s for 10 s | 1,000 requests/s for 30 s | 1,500 requests/s for 20 s | 300 requests/s for 30 s | 300 s | 3 s | 256 MiB |
| `small` | 10,000 requests/s for 3 s | 100 requests/s for 5 s | 1,000 requests/s for 10 s | 100 requests/s for 10 s | 180 s | 2 s | 64 MiB |

A request is 100 KiB of spans in both span sizes, so one schedule serves both. On the full rig the 1 KiB lane
offers 2,000,000 spans/s for throughput and 100,000 for latency, and the 10 KiB lane 200,000 and 10,000; the
end-to-end path offers 150,000 and 30,000 spans/s of 1 KiB spans, above and below the shipper's limit of about
69,000 in the baseline ([`perf-baseline.md`](perf-baseline.md)). Every measurement uses 128 connections. The
recent-span index is bounded explicitly because its default is a share of the VM's memory, not the container's.

### The decision

For each metric the gate takes the median of each build's values, the per-pair differences B - A, and the exact
one-sided Wilcoxon signed-rank p-value of those differences in the direction of a regression: lower for
throughput, higher for p99. Zero differences are dropped, tied absolute values share their mean rank, and the null
distribution is counted exactly over every assignment of signs.

- A metric **blocks** the merge when B's median throughput is more than 5% below A's, or B's median p99 is more
  than 10% above A's, in either case with p < 0.05. With exactly 5 pairs, that p-value is reachable only when all
  five pairs move in the regression direction (p = 1/32). More pairs make the test more sensitive.
- Durable-ack p99 gates only when the probe classified the WAL volume `honest` in the same session. On a
  `suspicious` volume, or when the probe failed, the latency is reported, labelled `not gated`, and cannot block.
  Sustained spans/s and freshness p95 always gate: the sync is a small part of either. The sustained rate follows
  the throughput rule (a drop of more than 5%) and freshness the p99 rule (a rise of more than 10%).
- An arm cannot be compared, which makes its metric's comparison **invalid**, when:
  - a throughput arm acknowledged 90% or more of the offered spans per second: the offered load did not exceed
    the build's capacity, so the value measures the schedule;
  - a latency arm had a failed request or a non-2xx response. `latency_us` covers 2xx responses only, so a stall
    longer than `--timeout-ms` shows up in `errors.timeout` rather than in the latency;
  - Crucible, ClickHouse or Postgres was not running after the measurement, for example after an OOM kill;
  - an end-to-end arm did not drain within the limit, or lost or duplicated an acknowledged span (the reason says
    how many);
  - a sustained arm shipped 90% or more of the offered spans per second, or its drain after the last response
    took less than a tenth of its time: the store kept up, so the ack path or the generator set the rate;
  - a freshness arm had a failed request or a non-2xx response, or its freshness covers fewer spans than it
    acknowledged.

The verdict is `regression` when a valid, gated metric blocks, otherwise `invalid` when a comparison did not hold,
otherwise `pass`. The command exits with 0 for `pass`, 1 for `regression` or `invalid`, and 2 when it could not
run: a bad argument, a failed build, a rig that did not start.

### Order effects and other jobs

The gate also estimates how much running second rather than first moves a metric, so that an A/A run (the same
commit as base and candidate) can show whether the rig favours a position. In an AB pair the second arm minus the
first is the builds' difference plus the order effect; in a BA pair it is the order effect minus the builds'
difference. The order effect is the mean of the AB pairs' median second-minus-first difference and the BA pairs',
relative to the median of every value, so the builds' difference cancels. It is reported for every metric (the
summary's "Second vs first" column), and a note names it when it exceeds the metric's threshold. It never blocks.

Each measured arm also records what else the machine did: busy cores on the rig's CPUs and on the Docker host's
other CPUs (from `/proc/stat`, which a container reads unnamespaced), the share of the measurement during which
the busiest block device had I/O in flight (from `/proc/diskstats`), the 1-minute load average before and after,
and the containers that ran outside the session. Other jobs' containers carry no cpuset, so they can take the
rig's CPUs as well, and they write to the disk the WAL flushes to; the summary's last table lists every run in
order with this context.

### A/A calibration

Two early smoke runs of `main` against itself on the small rig, with 2 pairs each, differed by +12.9% and +43.7%
in median throughput, and arm A was slower in all four throughput pairs. The question was whether the rig
favours a position (warm caches, image layers, the page cache, Docker network or volume set-up) or whether other
jobs' load made the noise. Both runs below measured one image (`39360ec`) as A and B on the small rig, ack path,
1 KiB lane, 8 pairs, on 2026-11-19, **measured on a laptop, not reference hardware**, with other tracks' builds
sharing the machine. The pass criterion for the second run was written down before it ran: the verdict is `pass`;
for each metric the two-sided sign test of the per-pair second-minus-first differences gives p of at least 0.05;
and the throughput order effect is below the throughput threshold of 5%.

| Run | Schedule | Throughput medians A, B | Throughput order effect | Second arm higher | p99 order effect | Second arm higher | Verdict |
|---|---|---|---|---|---|---|---|
| Before | each pair: both capacity arms, then both latency arms; no warm-up arm | 573,840; 575,878 spans/s | -0.6% | 2 of 8 pairs (sign test p = 0.29) | -11.4% | 3 of 8 (p = 0.73) | pass |
| After | a discarded latency arm of each build, then each build's capacity and latency arms in turn | 542,391; 529,760 spans/s | -3.1% | 3 of 8 pairs (p = 0.73) | +8.0% | 5 of 8 (p = 0.73) | pass |

- **Neither run shows a systematic order effect.** No sign test comes near significance, and the throughput
  order effect stays within its threshold in both. The early smoke runs' differences were noise from other jobs:
  over the first run, throughput fell as the other CPUs' load rose (correlation -0.73 between an arm's
  throughput and the busy cores on the other CPUs), and the second run, with heavier load from other jobs (1-minute
  load average 7 to 10), had two throughput arms at 410,000 and 152,000 spans/s, one in second position and one in
  first. With 2 pairs, one build slower in all four pairs happens one time in eight by chance.
- **The one structural asymmetry is gone.** In the first schedule the second capacity arm of a pair always
  followed a capacity arm, which writes gigabytes and whose rig removal precedes it, while the first followed a
  light latency arm; it measured 1.0% less than the first in the median (6 of 8 pairs, not significant). In the
  current schedule every capacity arm follows a latency arm, and every latency arm its own build's capacity arm,
  whichever build goes first, and the warm-up arms load both images before the first measured arm. A property test
  checks the schedule (`every_arm_follows_the_same_kind_of_work`).
- **Small-rig p99 is too noisy to bound an order effect** at 10% with 8 pairs: single arms swung from 4 ms to 67
  ms while other jobs wrote to the shared disk. Durable-ack p99 decides only on the full rig, alone on the machine.
- Other jobs can only be measured, not removed: the disk busy share now in every arm's context shows when a slow
  arm waited behind other jobs' writes, since the ack path is bound by the WAL's flushes on this disk
  ([`perf-baseline.md`](perf-baseline.md)).

### Output

The run writes `perf/results/local/gate/<time>-<base>-<candidate>[-smoke]/` (ignored by git): `gate.json`
(schema `crucible-perf.gate.v1`, below), `summary.md`, `probe.json`, the load generator's reports under `runs/`
(warm-ups as `*-warmup.json`) and Crucible's logs under `logs/`. The summary is labelled **measured on a laptop,
not reference hardware** and stamped with the AC-power state, the probe verdict, the rig, both commits and the
load generator's hash.

`--post` sets the `perf-gate` status on the candidate commit: `success` for `pass`, `failure` for `regression` and
`error` for `invalid`, with each metric's change and p-value in the description. When `gh` is missing or refuses,
the gate prints the `gh api` command to run where `gh` works. The public repository has no self-hosted runner, so
the gate runs on the laptop.

### The small rig

`--rig small` gives Crucible 1 GB (CPUs 0-1), ClickHouse 1.5 GB (CPUs 2-3), Postgres 512 MB (CPU 4) and the load
generator 512 MB (CPUs 5-6), 3.5 GB in all, with the short schedules above. It is for smoke runs on a busy
machine. Its results are labelled smoke and never posted, and it alone accepts fewer than 5 pairs. With N pairs
the smallest possible p-value is 1/2^N, so a smoke run with fewer than 5 pairs cannot block: it checks the
plumbing, not the builds.

### AC power

The gate reads the AC-power state before the run: from `CRUCIBLE_PERF_AC_POWER` (`online` or `battery`) when it is
set, otherwise from the AC adapters in `/sys/class/power_supply` on Linux, and otherwise it stamps `unknown` with
the reason. A container cannot see its host's power supply, so `tools/perf/xtask.sh` reads it on a Windows host
and passes it in.

### Running it on the laptop

The gate needs `cargo`, `git` and the Docker CLI with buildx, and `gh` for `--post`. It reaches Docker only through
the CLI, `docker cp` and stdout, never through bind mounts, so it runs on a Linux host or in a container that has
the host's Docker socket. On the laptop, `tools/perf/xtask.sh` runs `cargo xtask perf` in the perf dev container
(`deploy/dev/perf.Dockerfile`: the dev image with the Docker and GitHub CLIs), which it builds on first use:

```sh
tools/perf/xtask.sh gate --base main --rig small --pairs 2     # a smoke run beside other work
tools/perf/xtask.sh gate --base main --post                    # the gate: the full rig, alone on the machine
tools/perf/xtask.sh fsync-probe
```

The full rig takes 23 of the VM's 24 CPUs and 14 of its 16 GB, so it runs with nothing else on the machine. The
script mounts the checkout and its git directory (read-only, so a linked worktree works), the Docker socket, the
cargo registry volume and the target volume `$TARGET_VOLUME` (default `crucible-target`). It passes on
`CARGO_BUILD_JOBS` when that is set, and with `--post` the host's `gh auth token`, so the status is posted as the
account the host's `gh` is signed in to.

The gate removes its rig on every exit it sees, errors included. A run that is interrupted, with Ctrl-C or by
stopping its container, leaves the rig running. Everything the gate starts carries the label `crucible.perf-gate`,
and so does the container `tools/perf/xtask.sh` runs it in, so this stops a running gate and removes its rig, for
example to pause the machine's heavy work:

```sh
docker ps --all --quiet --filter label=crucible.perf-gate | xargs -r docker rm --force --volumes
docker volume ls --quiet --filter label=crucible.perf-gate | xargs -r docker volume rm
docker network ls --quiet --filter label=crucible.perf-gate | xargs -r docker network rm
```

### Report schema `crucible-perf.gate.v1`

Paths are relative to the results directory.

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | `crucible-perf.gate.v1` |
| `label` | string | `measured on a laptop, not reference hardware` |
| `smoke` | boolean | A small-rig run: never posted |
| `started_at`, `finished_at` | string | UTC, RFC 3339 |
| `ac_power` | object | `state` (`online`, `battery` or `unknown`) and `source` |
| `rig` | object | `name`, `containers[]` (`role`, `image`, `cpuset`, `memory`) and `docker` (server version, CPUs, memory, kernel, operating system) |
| `lane` | object | `name`, `path` (`ack` or `e2e`), `span_size`, `spans_per_request`, `gzip`, `concurrency`, `warmup_s`, `throughput_requests_per_s`, `throughput_s`, `latency_requests_per_s`, `latency_s`, `sustained_requests_per_s`, `sustained_s`, `freshness_requests_per_s`, `freshness_s`, `drain_s` |
| `base`, `candidate` | object | `reference` as given, `sha`, `image`, `image_id` |
| `loadgen_sha256` | string | The load generator binary that measured both arms |
| `pairs`, `order` | integer, array | The number of pairs, and each pair's order, `AB` or `BA` |
| `probe` | object | `verdict` (`honest`, `suspicious` or `failed`), `report` (path or null) and `error` (null, or why the probe failed) |
| `comparisons[]` | array | The path's two metrics (`throughput` and `latency`, or `sustained` and `freshness`), each with `metric`, `unit`, `better` (`higher` or `lower`), `base[]` and `candidate[]` (one value per pair), `median_base`, `median_candidate`, `change` (relative change of the median, null when the base median is 0), `differences[]` (B - A per pair), `wilcoxon` (`n` non-zero differences, `w_plus`, one-sided `p`), `threshold`, `order_differences[]` (second arm minus first per pair), `order_change` (the order effect above, null without pairs of both orders), `gated`, `valid`, `notes[]` and `blocks` |
| `runs[]` | array | Every measured arm in run order: `pair`, `kind`, `arm`, `report`, `log`, `offered_spans_per_s`, `acked_spans_per_s`, `latency_p99_us`, `errors`, `non_2xx`, `invalid` (null, or why the arm cannot be compared) and `context`: `started_at`, `finished_at`, `rig_cpus_busy` and `other_cpus_busy` (busy cores averaged over the measurement), `disk_busy` (the busiest block device's share of the measurement with I/O in flight), `load_before`, `load_after` and `others[]` (the other containers' names); for an end-to-end arm `stored`: `acked_spans`, `drained_s` and `drain_after_run_s` (null when it did not drain), `shipped_spans_per_s`, `freshness_p95_us`, `freshness_spans`, `lost_spans` and `duplicate_spans` |
| `verdict` | string | `pass`, `regression` or `invalid` |

## The fsync-honesty probe

A durable ack is only as durable as the WAL volume's flush. Write-back caches, tmpfs and virtualization layers
can acknowledge a flush that never reached durable media. The probe measures how much flushing costs and
classifies the volume.

### Method

`crucible-loadgen fsync-probe --dir <path>` treats `<path>` as the WAL root and works in a fresh subdirectory,
which it removes afterwards. Every operation goes through `Env`'s disk with the portable backend, which is the
backend the WAL uses: on Linux, `sync_data` is `fdatasync(2)` and `sync_dir` is `fsync(2)` on the directory.

- **Writes.** For each size of 4 KiB, 64 KiB, 1 MiB and 8 MiB (8 MiB is the WAL's group cap), the probe
  preallocates a file. Each iteration appends the size at the next offset and then calls `sync_data`, mirroring
  how the WAL writes fresh bytes into a preallocated segment. The bytes are incompressible. At most 64 MiB are
  measured per size, because tmpfs bytes are memory. With the default 200 iterations, that gives 200, 200, 64 and
  8 iterations.
- **Directory.** Each iteration creates an empty file and then calls `sync_dir` on the directory.
- **Hop.** The portable backend runs every mutating call on one writer thread per root, so each measured
  operation includes a thread round trip. Every iteration also times an empty append, which is that round trip
  alone.
- The first 3 iterations of every phase are discarded. The report keeps every remaining sample.

### Classification

The checks use medians net of the hop median, because a CPU quota or a noisy neighbour pollutes the tails, not
the median. The volume is `honest` when all three checks pass, and `suspicious` otherwise.

| Check | Measured value | Floor | Why this floor |
|---|---|---|---|
| `small_write_flush` | 4 KiB `sync_data` minus the hop | 100 µs | A flush that reaches media waits for the device: a NAND page program takes hundreds of microseconds, a disk rotation milliseconds, a network block store a round trip. A cache that only acknowledges answers in a few microseconds |
| `sync_scales_with_bytes` | 8 MiB `sync_data` minus 4 KiB `sync_data` | 8 MiB at 10 GB/s, about 0.84 ms | A sync that moved 8 MiB cannot beat the device's bandwidth, and 10 GB/s exceeds what current NVMe drives sustain for writes. A no-op sync costs the same at every size, even when throttling makes it slow |
| `dir_sync` | `sync_dir` after a create, minus the hop | 100 µs | Making a new directory entry durable commits the filesystem journal, which flushes the device |

Known limits:

- A device with power-loss protection can acknowledge a flush from protected cache in tens of microseconds and
  still be durable. The probe classifies it `suspicious`. That false positive errs on the safe side: it stops
  durable-ack latency from gating.
- Latency cannot prove durability, only rule out durability that is too fast to be real. Proving it takes a
  power cut. The verdict is evidence for gating on this rig, not a durability guarantee.

### Running it

`cargo xtask perf fsync-probe` runs the probe the way the gate does: on a fresh named Docker volume, the kind the
rig's WAL uses, in a container with Crucible's CPUs and memory. It prints the verdict and each check and writes the
report. On the laptop, run it through `tools/perf/xtask.sh fsync-probe` (see the gate's section above).

| Flag | Default | Meaning |
|---|---|---|
| `--rig` | `full` | Whose Crucible limits the probe's container gets: `full` or `small` |
| `--out` | `perf/results/local/fsync-probe/<time>.json` | Where to write the JSON report |

The probe itself is `crucible-loadgen fsync-probe`, which can also run on any directory, for example from the dev
container with a release build:

```sh
docker run --rm -v crucible-target:/target -v <wal-volume>:/probe crucible-dev:1 \
  /target/release/crucible-loadgen fsync-probe --dir /probe > probe-report.json
```

| Flag | Default | Meaning |
|---|---|---|
| `--dir` | required | A directory on the volume to probe |
| `--iterations` | 200 | Measured iterations per phase. The larger write sizes use fewer, to stay within 64 MiB per size |
| `--out` | stdout | Where to write the JSON report |

A run takes a few seconds on an honest volume and writes at most about 170 MiB, which it then deletes.

### Report schema `crucible-loadgen.fsync-probe.v1`

All durations are integer nanoseconds.

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | `crucible-loadgen.fsync-probe.v1` |
| `dir` | string | The probed directory, as given |
| `verdict` | string | `honest` or `suspicious` |
| `checks[]` | array | One entry per check: `name`, `value_ns` (net median; negative when faster than its baseline), `floor_ns`, `pass` |
| `hop_ns` | distribution | The empty append (writer-thread round trip), sampled in every measured iteration of every phase |
| `writes[]` | array | One entry per write size, smallest first: `bytes`, plus distributions `write_ns` (the append), `sync_ns` (the `sync_data` after it) and `total_ns` (append plus sync, per iteration) |
| `dir_sync` | object | Distributions `create_ns` (creating an empty file) and `sync_ns` (the `sync_dir` after it) |

A distribution has `count`, `min`, `p10`, `p50`, `p90`, `p99` and `max` (nearest-rank percentiles), and
`samples`, every sample in measurement order.

## The load generator

`crucible-loadgen run` sends OTLP/HTTP `POST /v1/traces` requests with protobuf bodies, uncompressed or gzipped.
The default target is `127.0.0.1:4318` (contract 12.1).

### What it sends

- **Pre-encoded pools.** A pool holds a few template requests (16 by default) of 1 KiB or 10 KiB spans. A span's
  size is the encoded length of its `Span` message. Each template is encoded once. A request is a copy of a
  template with its fixed-width identity fields overwritten, so building one costs a memory copy and a few
  dozen byte writes. Pools respect the request caps: at most 50,000 spans and 67,043,328 bytes per request
  (contract 4.3 and 4.8).
- **Deterministic content.** Span names, kinds, durations and payload text are a function of `--seed` alone. The
  payload text is drawn from a small word list, so the gzip lane compresses text rather than random bytes (about
  3.2 times for 1 KiB spans). Given the same seed, run id and start time, every byte is the same.
- **Request shapes.** A request holds `--spans-per-request` spans in `--traces-per-request` traces (1 by default)
  of consecutive spans whose lengths differ by at most one. The first span of each trace is its root; with
  `--rootless` it names a parent that no span of the run carries, so no trace has a root, the shape of a client
  that exports child spans before their parent.
- **Identity.** Every span carries the attributes `loadgen.run_id` (the run id, minted fresh unless `--run-id`
  is given) and `loadgen.seq`, its sequence number `request_index * spans_per_request + position`, as 20
  zero-padded digits. Trace ids are the run tag followed by a bijective mix of `request_index *
  traces_per_request + trace`, so they never repeat within a run. Span ids are the same mix of the sequence
  number. No id is ever zero. Span times follow the request's intended send time on the wall clock, or with
  `--spread-secs` spread evenly over that many seconds before the schedule's start, to load a dataset that covers
  a read window. The read generator derives every id and start time of a loaded run from its report alone.

### How it sends

- **Open loop.** Request `i` is due at `start + i / rate`, computed from `i` alone. Responses never move the
  schedule.
- **Latency from the intended send time.** When the server stalls, requests that fall due during the stall wait,
  and the wait counts in their latency. A stalled server therefore cannot hide behind a stalled client
  (coordinated omission).
- **Bounded concurrency.** Each of `--concurrency` connections carries one request at a time (HTTP/1.1). A free
  connection takes the next request and prepares its body (the patch, and the gzip in the gzip lane) before the
  request is due.
- **Gzip off the runtime's workers.** A 1 MiB body takes a few milliseconds of CPU to compress (the generator
  spends 4 to 5 ms per such request); on a runtime worker it held up the pacer and the other connections of that
  thread, so the generator sent late: send lag p99 3.3 to 98.7 ms at 25,600 spans/s of 10 KiB spans in 100-span
  requests, and 0.06 ms off the workers ([`perf-baseline.md`](perf-baseline.md)). Bodies are compressed on the
  runtime's blocking threads, at most `--gzip-threads` at a time (by default half the CPUs available, at least
  one; the runtime's workers get the rest), each by an encoder that keeps its deflate state from body to body.
- **A pacer.** The production timer has millisecond resolution, so waiting on it alone sent requests up to
  about 2.5 ms late. One pacer task sleeps on the timer until 3 ms before a due time, then yields in a loop until
  the time comes, and releases the request to its connection. The pacer keeps about one core busy, and the CPU
  figures below include it.
- **No retries.** A failed request is counted and never resent, and its connection is replaced. A connection
  the server closed while it sat idle is replaced before the next request is written.
- **Partial success.** A 2xx response is decoded. A response that reports rejected spans is counted under
  `partial_success_requests` and does not count as acknowledged.

### Zero-loss seam

The run keeps a ledger of which requests were acknowledged: answered 2xx, with no rejected spans. Every span of
an acknowledged request must be readable afterwards (invariant I1). The report lists the requests that were not
acknowledged as index ranges, so the acknowledged sequence numbers can be reconstructed from the report alone.
`SequenceReader` is the interface of a store-backed reader: it returns the stored `loadgen.seq` values of one
run. `check_zero_loss` compares them with the ledger and counts lost and duplicated spans; the end-to-end test of
the binary reads ClickHouse through it, and the gate's end-to-end path runs the same check in SQL.

### Running it

```sh
crucible-loadgen run --target crucible:4318 --rate 2500 --duration-secs 60 \
  --span-size 1k --spans-per-request 100 --header 'Authorization: Bearer <secret key>' --out run.json
```

| Flag | Default | Meaning |
|---|---|---|
| `--target` | `127.0.0.1:4318` | OTLP/HTTP listener as `host:port`. A host name is resolved once |
| `--rate` | required | Requests per second |
| `--duration-secs` | 10 | Schedule length; the schedule holds `rate * duration` requests |
| `--span-size` | `1k` | `1k` or `10k` |
| `--spans-per-request` | 100 | Spans per request |
| `--traces-per-request` | 1 | Traces per request, from 1 to the spans per request |
| `--rootless` | off | Give no trace a root |
| `--gzip` | off | Gzip every body (`Content-Encoding: gzip`) |
| `--gzip-threads` | half the CPUs, at least 1 | With `--gzip`, the most bodies compressed at a time, each on a blocking thread |
| `--concurrency` | 64 | Connections, and so the most requests in flight |
| `--seed` | 1 | Content seed |
| `--run-id` | fresh UUIDv7 | The run id every span carries |
| `--templates` | 16 | Pre-encoded templates; memory is about `templates * spans_per_request * span size` |
| `--timeout-ms` | 10000 | Deadline for connecting and for each request |
| `--header` | none | Extra header as `Name: value`, repeatable; ingest needs an `Authorization` header (contract 12.2) |
| `--spread-secs` | 0 | Spread the span times evenly over this many seconds before the schedule's start (moved back by `--backdate-secs`) |
| `--backdate-secs` | 0 | Move the span times this many seconds back: with a spread, the spread ends there |
| `--threads` | CPUs available, less the gzip threads | Runtime worker threads |
| `--out` | stdout | Where to write the JSON report |

The process exits non-zero, without a report, when the target refuses the initial connections or the
configuration cannot run.

To calibrate the generator without a backend, run the stub server. It answers every valid request with 200 and
an empty `ExportTraceServiceResponse`, and it answers 400 to a body that does not decode, so a clean run also
proves the bodies valid:

```sh
cargo run --release -p crucible-loadgen --example stub_server -- --listen 127.0.0.1:14318 --threads 2
crucible-loadgen run --target 127.0.0.1:14318 --rate 10000 --spans-per-request 10
```

### Report schema `crucible-loadgen.run.v1`

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | `crucible-loadgen.run.v1` |
| `run_id` | string | The run id every span carries |
| `seed` | integer | The content seed |
| `target` | string | The target as given |
| `started_at` | string | The wall-clock time of the schedule's start, RFC 3339 with microseconds |
| `lane` | object | `span_bytes`, `spans_per_request`, `traces_per_request`, `rootless`, `gzip`, `gzip_threads` (0 without gzip), `spread_s` (0 when span times follow the schedule), `backdate_s`, `templates`, `request_bytes_mean` (protobuf body before compression), `wire_bytes_mean` (body on the wire) |
| `schedule` | object | `target_rps`, `duration_s`, `requests` (in the schedule), `concurrency`, `timeout_ms` |
| `outcome.sent` | integer | Requests written to a connection |
| `outcome.responses` | integer | Requests that got an HTTP response |
| `outcome.acked`, `outcome.acked_spans` | integer | Requests acknowledged without rejected spans, and their spans |
| `outcome.partial_success_requests`, `outcome.rejected_spans` | integer | 2xx responses that reported rejected spans, and how many spans they rejected |
| `outcome.status_counts` | object | Responses by HTTP status code (keys are the codes as strings) |
| `outcome.errors` | object | Requests without a usable response: `connect` (no connection, so not sent), `timeout`, `transport` (the connection failed), `bad_response` (a 2xx body that is not an `ExportTraceServiceResponse`) |
| `outcome.achieved_rps` | number | `sent` over `send_window_s` |
| `outcome.acked_rps`, `outcome.acked_spans_per_s` | number | Acknowledged requests and spans over `elapsed_s` |
| `outcome.send_window_s` | number | From the schedule start to the last send, plus one send interval, so a generator exactly on schedule reports the target rate |
| `outcome.elapsed_s` | number | From the schedule start to the last response or failure |
| `latency_us` | summary | Intended send time to complete response, over 2xx responses. This is the gated latency |
| `service_time_us` | summary | Actual send time to complete response, over 2xx responses |
| `send_lag_us` | summary | Intended to actual send time, over every sent request: the generator's own error, plus queueing when every connection is busy |
| `not_acked_requests` | array | Half-open `[start, end)` request index ranges that were not acknowledged |
| `generator_cpu` | object or null | `user_s`, `system_s` and `us_per_request` (user plus system CPU per sent request) during the run, from `/proc/self/stat`; null off Linux |

A summary has `count`, `p50`, `p90`, `p95`, `p99`, `p99_9` and `max`, in microseconds, from an HDR histogram with three
significant digits.

## The read bench

`cargo xtask perf reads` measures the read-side targets on one build. It starts one rig as the gate does
(ClickHouse, Postgres and Crucible, with the first owner and a key with the `ingest` and `read` scopes), and then:

1. **Ack to API on the empty store.** `crucible-loadgen fresh` sends one small trace (two 1 KiB spans) at a time
   to ingest and, from its acknowledgement on, lists the project's traces every 2 ms until the list names it:
   the method of milestone 1's end-to-end figure (13 to 28 ms from ack to the API). 100 probes by default, 200 ms
   apart.
2. **The dataset.** `crucible-loadgen run` loads 1 KiB spans in traces of 100 through ingest, one chunk per day of
   the read window (30 days by default), oldest first: `--backdate-secs` places the chunk on its day and
   `--spread-secs` spreads its spans over the day in time order, as live traffic arrives, so an insert touches
   one or two daily partitions. Each chunk drains into the store before the next, so the WAL's backlog stays
   bounded. The load runs below the rig's
   end-to-end capacity: 15,000 spans/s on the small rig, 50,000 on the full one. It records the bytes of the
   store's active parts per span, from `system.parts`.
3. **Ack to API on the loaded store**, the same probe again.
4. **One lane per read class**, more than the visibility grace after the load, so that the store rather than the
   recent index answers for the dataset. A lane is `crucible-loadgen read` with that class alone: an open-loop
   schedule of 2 reads/s on average for 60 s with exponential think times, 8 connections, its traces and spans
   drawn from the dataset's run reports. Crucible's counters are read from its metrics listener before and
   after the lane, so their change belongs to the lane's class.

| Lane | Request | Answers that count | Server class |
|---|---|---|---|
| `trace_by_id` | `GET /traces/{trace_id}` | non-empty `spans` | `trace_by_id` |
| `trace_by_id_hinted` | the same with `?hint_time=` the trace's start, as the UI sends it from a list | non-empty `spans` | `trace_by_id` |
| `trace_list_30d` | `GET /traces?from=...&to=...&limit=50` over the window | non-empty `data` | `trace_list` |
| `span_list_30d` | `GET /spans?from=...&to=...&limit=50` over the window | non-empty `data` | `span_list` |
| `session_list_30d` | `GET /sessions?from=...&to=...&limit=50` over the window (the dataset has no sessions) | 2xx | `session_list` |
| `user_list_30d` | `GET /users?from=...&to=...&limit=50` over the window (the dataset has no users) | 2xx | `user_list` |
| `span_children` | `GET /traces/{trace_id}/spans?parent_span_id={root}&limit=100` | non-empty `data` | `span_children` |
| `payload` | `GET /spans/{trace_id}/{span_id}/payload?field=attributes&start_time=...` | 2xx | `payload` |
| `facts` | `GET /facts?trace_id=...` (the dataset has no facts) | 2xx | `facts` |
| `filter_values_30d` | `GET /filter-values?source=spans&field=observation_type` over the window | non-empty `data` | `filter_values` |
| `data_bounds` | `GET /data-bounds` | non-empty `spans.first` | `data_bounds` |
| `search_30d` | `POST /search` over the window for a word no span holds: the dataset carries no input or output text, so this is a full-window search that matches nothing | 2xx | `search` |
| `metrics_30d` | `POST /metrics/query`: the span count over the window | non-empty `series` | `metrics` |

Paths are relative to `/api/v1/projects/{project_id}`. A lane whose requests were not all answered 2xx and
non-empty is marked invalid: its latency would not cover them. So is a saturated lane, whose requests waited more
than 25 ms (send lag p95) for a free connection: the schedule outran the server, and the latency grew with the
queue rather than with the read. The session and user detail routes need ids that
the dataset does not carry (its spans have no session or user), so they have no lane yet.

**The MCP tool dashboard** gets its lane in `LANES` (`xtask/src/perf/reads.rs`) once the MCP analytics endpoints
exist: one entry with the dashboard's request, the JSON pointer that must not be empty, and the request class its
read service records.

```sh
tools/perf/xtask.sh reads                                   # the small rig: 2 million spans, HEAD's build
tools/perf/xtask.sh reads --rig full --spans 100000000      # the 100-million-span dataset, alone on the machine
```

| Flag | Default | Meaning |
|---|---|---|
| `--image` | `HEAD` | The build to measure, as a ref; its image is built as the gate builds one |
| `--rig` | `small` | `small` or `full`, as for the gate |
| `--spans` | 2,000,000 on the small rig, 10,000,000 on the full one | Spans to load |
| `--spread-days` | 30 | Days the spans' start times spread over, ending at the load, from 1 to 400 (ingest's maximum event age) |
| `--rate` | 2 | Reads per second of each lane, on average. On the small rig, trace by id takes about 0.5 s and a 30-day span list about 1 s of its two ClickHouse CPUs, so a higher rate saturates those lanes |
| `--seconds` | 60 | Seconds of each lane |
| `--probes` | 100 | Traces of each ack-to-API probe |
| `--lanes` | every lane | Only these lanes, comma-separated |

The run writes `perf/results/local/reads/<time>-<commit>[-smoke]/`: `reads.json` (schema
`crucible-perf.reads.v1`: the build, the rig, the dataset with its bytes per span, both probes and every lane
with its percentiles, store reads per request, mean barrier wait, connections opened, ClickHouse merges at its
start and why it is invalid), `summary.md`, the generator's reports under `runs/` and `dataset/`, and Crucible's
metrics before and after each lane under `metrics/`.

### Per-request counters

The read services record two histograms per request class (one class per read service: `trace_by_id`,
`trace_list`, `trace_list_since`, `span_children`, `span_list`, `payload`, `facts`, `fact_target_bounds`,
`filter_values`, `data_bounds`, `search`, `metrics`, `session_list`, `session`, `user_list`, `user`,
`user_sessions`, `export_page`):

- `crucible_api_store_reads{class}`: the store reads each request issued. Its count is the requests and its sum
  the reads.
- `crucible_api_barrier_wait_microseconds{class}`: how long each request that waits for the recent index's read
  barrier waited (trace by id and the fact target bounds), timed by the read service with the `Env` clock, since
  the metrics recorder never reads a clock (contract 2.12).

Connections come from `crucible_clickhouse_connections_opened_total`, which the telemetry store counts at
`Net::connect`. It has no class, and the shipper's inserts count too, so the bench reads it around one class at a
time on a store that is not loading.

### `crucible-loadgen read` and `crucible-loadgen fresh`

`read` sends the reads of a lane open loop: request `i` is due at `start + t_i`, fixed before the first request,
and its latency counts from that intended time, as in `run`. The gaps are the think times: exponential with mean
`1 / rate` from `--seed` (a Poisson process), or all `1 / rate` with `--think fixed`. Request `i` uses class `i %
classes`. A class is JSON (`name`, `method`, `path`, optional `body`, optional `nonempty`, a JSON pointer that
must name a non-empty array, object or string in every 2xx answer), and its path and body take placeholders that
name the dataset that `--dataset` reports describe: `{project}`, `{trace_id}`, `{root_span_id}`, `{span_id}`,
`{span_start}`, `{trace_start}`, and the window `{from}` and `{to}` (`to` is the request's due time,
`from` is `--window-secs` before it). Each request draws an acknowledged request of the dataset uniformly, then one
of its traces and one of its spans, as a function of the seed and the request's index.

The report (`crucible-loadgen.read.v1`) has `schedule` (`rate`, `duration_s`, `think`, `requests`,
`concurrency`, `timeout_ms`, `window_s`), `dataset_runs` and `dataset_requests`, and per class `sent`, `ok`,
`empty`, `status_counts`, `errors`, `latency_us` and `service_time_us` over the `ok` answers; and `send_lag_us`
and `elapsed_s`.

`fresh`'s report (`crucible-loadgen.fresh.v1`) has `probes`, `listed`, `ack_us` (send to acknowledgement),
`ack_to_list_us`, `lists_per_probe`, `ingest_errors`, `list_errors` and `missing` (acknowledged but not listed
within `--timeout-ms`).

## Measurements

This section covers the probe and the generator on their own. The first end-to-end baseline of `crucible serve`
is in [`perf-baseline.md`](perf-baseline.md).

Measured on 2026-11-17, on a laptop, not reference hardware: Windows 11, Docker Desktop with the WSL2 VM (24
CPUs, 16 GB, kernel 6.18.58.1-microsoft-standard-WSL2), dev image `crucible-dev:1`, one release build, one pass.
Other jobs shared the VM during the pass. Among them were another track's conformance containers, which used the
same virtual disk as the named volume.

### Probe

| Volume | Verdict | Hop p50 | 4 KiB sync p50 (p99) | 8 MiB sync p50 | Directory sync p50 | Checks (net ns: small write, scaling, directory) |
|---|---|---|---|---|---|---|
| Named Docker volume (ext4 on the VM's virtual disk) | `honest` | 46 µs | 8.07 ms (12.75 ms) | 19.18 ms | 7.62 ms | 8,028,454; 11,106,822; 7,573,872 |
| tmpfs (`--tmpfs /probe:size=512m`) | `suspicious` | 23 µs | 20 µs (47 µs) | 32 µs | 22 µs | -3,126; 12,754; -998 |

On tmpfs every sync costs about the hop, so all three checks fail. The named volume passes all three, which
suggests that flushes reach the host's disk through the virtualization layer. Its latency depends on what else
is writing to the shared disk: two earlier passes the same day, with the same probe code and less disk traffic,
measured a 4 KiB sync p50 of 1.08 ms and 1.22 ms and gave the same verdicts. Neither result describes reference
hardware.

### Generator accuracy

The generator and the stub ran as separate processes in one container (`--cpuset-cpus=18-23 --cpus=6`), on
loopback. The generator used 4 runtime threads pinned to 4 CPUs, and the stub used 2 threads pinned to the
other 2. Each rate ran for 20 seconds, with 1 KiB spans, 10 spans per request (10,341-byte bodies) and 64
connections. Latency columns are in microseconds and give p50 / p99 / p99.9 (max for send lag).

| Lane | Target req/s | Achieved req/s | Acked | Send lag | Service time | Latency | CPU µs per request |
|---|---|---|---|---|---|---|---|
| uncompressed | 1,000 | 1,000.0 | 20,000 of 20,000 | 2 / 20 / 582 | 72 / 180 / 380 | 75 / 210 / 414 | 1,270 |
| uncompressed | 10,000 | 10,000.0 | 200,000 of 200,000 | 1 / 43 / 555 | 45 / 109 / 243 | 46 / 165 / 329 | 129 |
| uncompressed | 50,000 | 49,999.9 | 1,000,000 of 1,000,000 | 0 / 52 / 1,338 | 43 / 117 / 279 | 44 / 167 / 379 | 29 |
| gzip | 1,000 | 1,000.0 | 20,000 of 20,000 | 2 / 21 / 3,811 | 97 / 206 / 396 | 100 / 226 / 503 | 1,280 |
| gzip | 10,000 | 10,000.0 | 200,000 of 200,000 | 1 / 60 / 4,899 | 70 / 176 / 348 | 78 / 218 / 428 | 153 |

- Every run had zero errors and every status was 200.
- Achieved rates were within 0.0002% of the target, and the median send lag stayed at or below 2 µs.
- In the uncompressed lane the generator used 1.3 to 1.5 cores at every rate, and in the gzip lane 1.3 and 1.5.
  About one core is the pacer, which also runs other tasks in the gaps between due times, so CPU per request
  falls as the rate rises.
- An earlier build that waited on the millisecond timer alone, with no pacer, cost 55, 21 and 12 µs per request
  at the three rates, but sent up to 2.5 ms late (p50 0.65 to 1.05 ms).
- Gzip bodies were 3.2 times smaller (about 3,220 bytes).
- At 50,000 req/s the gzip lane fell slightly behind and is left out of the table. It achieved 49,144 req/s, the
  backlog pushed latency to a p50 of 78 ms and a maximum of 365 ms, and service time rose to a p50 of 1.2 ms.
  The generator used 2.9 of its 4 cores. The stub inflates and decodes every body on its 2 threads, so the
  shared 6 CPUs set the limit, not the generator alone. In the rig, the generator gets its own 6 CPUs.

### The end-to-end lanes

Measured on 2026-11-19, on a laptop, not reference hardware, on the small rig (Crucible 2 CPUs and 1 GB,
ClickHouse 2 CPUs and 1.5 GB), path `e2e`, lane `1kb`, with other tracks' builds sharing the machine (1-minute
load average 6 to 11). The image is `39360ec` in both arms unless stated.

| Run | Pairs | Sustained medians A, B (shipped spans/s) | Change, p | Freshness p95 medians A, B | Change, p | Verdict |
|---|---|---|---|---|---|---|
| Smoke | 1 | 57,888; 58,957 | +1.8% | 575 ms; 574 ms | -0.3% | pass |
| A/A | 5 | 61,053; 58,828 | -3.6%, p = 0.22 | 579 ms; 587 ms | +1.4%, p = 0.69 | pass |
| Seeded: B inserts with zstd level 1 instead of LZ4 | 5 | 60,288; 59,614 | -1.1%, p = 0.50 | 571 ms; 580 ms | +1.5%, p = 0.031 | pass |
| Seeded: B inserts with zstd level 9 instead of LZ4 | 5 | 47,854; 35,243 | -26.4%, p = 0.031 | 611 ms; 738 ms | +20.9%, p = 0.031 | **regression** |

- Every arm of the four runs acknowledged every request with 200 and drained in time, and the zero-loss check
  found no lost or duplicated span: each sustained arm stored all 1,000,000 acknowledged spans and each freshness
  arm all 100,000, and the freshness covered every one of them.
- **The seeded slowdown is flagged.** Zstd at level 1 costs the store path little on this rig: the arms that
  inserted with it measured 1.5% worse freshness in all 5 pairs (p = 0.031), below the 10% threshold, and no
  sustained change. At level 9 every pair measured both metrics worse, the medians by 26.4% (sustained) and
  20.9% (freshness), and the gate answered `regression` with exit code 1. The seeded builds were local commits on
  top of `39360ec`, never pushed. The level 9 run's base arms measured lower than the A/A's (47,854 against
  61,053) because image builds ran on the machine at the same time.
- A sustained arm offers 100,000 spans/s for 10 s; the ack path takes them at that rate while the store falls
  behind. In the A/A it drained for 5.9 to 8.5 s after the last response, and the store path shipped 54,000 to
  63,000 spans/s, about what the full rig's single shipper task moved in the baseline (69,000).
- The A/A's order effect was -0.7% (sustained) and -3.0% (freshness), and its arms spread by up to 15% from
  pair to pair: throughput again followed the other jobs' load (correlation -0.50 with the other CPUs' busy
  cores).

### The read bench

Measured on 2026-11-19, on a laptop, not reference hardware, on the small rig (ClickHouse 2 CPUs and 1.5 GB),
build `7abc007`, with other tracks' work on the machine. Each run loaded 2,250,000 spans of 1 KiB over 30 days
through ingest in 177 s; the store held them in 516 MB of active parts, 230 bytes per span, so the 100-million-span
dataset needs about 23 GB of ClickHouse disk and the billion-span one about 230 GB.

Two runs, each lane 120 reads at 2 per second; latency in ms, p50 / p95 / p99:

| Lane | Run 1 | Run 2 | p95 change | Store reads per request |
|---|---|---|---|---|
| `trace_by_id` | 90.0 / 154.8 / 209.0 | 83.1 / 137.2 / 174.5 | -11.3% | 6 |
| `trace_by_id_hinted` | 35.3 / 60.2 / 67.1 | 32.7 / 38.4 / 41.9 | -36.1% | 6 |
| `trace_list_30d` | 43.3 / 54.7 / 79.1 | 40.2 / 46.8 / 57.9 | -14.3% | 5 |
| `span_list_30d` | 261.6 / 398.6 / 494.3 | 249.5 / 366.1 / 477.7 | -8.2% | 1 |
| `session_list_30d` | 3.7 / 4.7 / 9.0 | 3.5 / 3.9 / 5.2 | -16.7% | 1 |
| `user_list_30d` | 3.4 / 4.0 / 4.5 | 3.4 / 4.1 / 4.5 | +2.6% | 1 |
| `span_children` | 35.4 / 61.6 / 73.0 | 32.7 / 38.2 / 40.3 | -38.0% | 3 |
| `payload` | 11.4 / 20.0 / 22.3 | 10.7 / 13.6 / 15.9 | -31.6% | 1 |
| `facts` | 17.2 / 21.0 / 24.8 | 19.1 / 65.7 / 90.6 | +212.8% | 4 |
| `filter_values_30d` | 6.4 / 7.7 / 12.1 | 6.0 / 7.0 / 13.4 | -9.3% | 1 |
| `data_bounds` | 7.2 / 13.4 / 36.1 | 7.4 / 22.0 / 43.2 | +64.1% | 2 |
| `search_30d` | 19.4 / 47.1 / 286.2 | 19.9 / 28.0 / 485.6 | -40.5% | 1 |
| `metrics_30d` | 6.7 / 10.7 / 14.1 | 9.3 / 38.1 / 60.7 | +257.4% | 1 |

- Every lane was valid in both runs: every read answered 2xx and non-empty where it must be, Crucible counted
  every request under the lane's class, and no lane was saturated. Trace by id waited 0 ms for the read barrier
  (the index was caught up), and each lane opened at most 2 store connections.
- **Against the targets, at 2.25 million spans on 2 ClickHouse CPUs:** trace by id without a hint misses 100 ms
  p95 (137 to 155 ms); with the trace's start as the hint, as the UI sends it, it meets it (38 to 60 ms). The
  30-day trace list stays far below 500 ms p95 (47 to 55 ms); the billion-span figure needs the full rig and the
  larger dataset.
- **Two runs agree within 10% at p95 in 3 of 13 lanes**, not all of them. 120 reads put p95 on the 6th slowest
  read, so a stall of a second or two on the shared machine moves it; the medians agree better (within 10% in 11 of
  13 lanes), and run 2 was faster in most lanes, which points at the machine's load rather than the build. A pair of
  longer runs of the three gate lanes (360 reads each) was stopped when another track's gate started on the same
  CPUs (the empty-store probe rose to 514 ms p95): two sessions on one rig measure each other. The stability
  check belongs in the exclusive window: `tools/perf/xtask.sh reads --rig full`, twice, alone on the machine.
- An earlier run at 20 reads/s saturated trace by id (about 470 ms per read under that load, with
  requests queued up to 4 s for a connection) and the 30-day span list (about 1.1 s per read): the saturation rule
  and the default rate of 2 per second come from it.

**Ack to API.** On the empty store the probe measured 59.8 / 75.5 / 81.9 ms and 52.0 / 64.6 / 77.1 ms (p50 / p95 /
p99, two runs; an earlier run while images were building, 74.6 / 99.5 / 111.5 ms), not milestone 1's 13 to 28 ms.
Every probe took exactly two lists: the first, sent at the acknowledgement, never named the trace, and the second
did, so the figure is two list round trips of about 30 ms each. A list issues 5 store reads in sequence (the
counter above), each a ClickHouse query on 2 CPUs; the trace itself was visible by the start of the second list.
On the loaded store the same probe measured 108.6 / 121.0 / 139.4 and 99.7 / 113.0 / 125.5 ms.

### Pre-ack copies

Measured on 2026-11-19, on a laptop, not reference hardware, with release builds in the dev container (3 CPUs),
each timing run interleaving the two forms over its rounds and taking medians.

- **Inflate** (`crucible-ingest`, `inflate_throughput`, ignored): inflating into one allocation sized from the
  gzip trailer, against 64 KiB reads copied into a doubling buffer. Time per body changed by -1.2%, -0.7% and
  -0.5% in three runs for 100 KiB bodies (the 1 KiB lane), and by -1.7%, +0.4% and -0.7% for 1 MiB bodies (the
  10 KiB lane). The decode itself takes most of the time (about 1.25 ms per 1 MiB body). What changes more is the
  buffer: a body ends in exactly its size instead of up to twice it (131,072 bytes for a 102,400-byte body),
  which the in-flight reservation now matches. Kept.
- **Block frames** (`crucible-chfmt`): encoding a 262,144-row `span_core` block (62.3 MB of LZ4 frames) into a
  buffer sized from the previous block measured 1.2%, 1.0% and 4.0% slower than from an empty buffer, in three
  runs. Growing a large buffer remaps its pages rather than copying them, so there is nothing to save; the
  change was not kept.
