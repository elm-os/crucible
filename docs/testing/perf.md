# Performance rig

Every absolute number on this page was **measured on a laptop, not reference hardware**. Merges are gated on
relative regressions between two builds measured on the same machine, never on absolute numbers. Reports that
quote absolute numbers carry the same label.

The rig has three parts:

- the fsync-honesty probe, which must classify the WAL volume before durable-ack latency can gate anything;
- the load generator, `crucible-loadgen`;
- the A/B gate, `cargo xtask perf gate`, which drives both.

## v0 targets

| Target | Value |
|---|---|
| Ingest throughput per node | at least 250,000 spans/s of 1 KiB spans, and at least 50,000 spans/s of 10 KiB spans |
| Durable-ack latency | p99 at most 20 ms, with `wal.durability = group` on a volume the probe classified `honest` |
| Acknowledged loss | zero (invariant I1) |
| Freshness | at most 2 s p95 from ack to readable |

The read-side targets (trace by id, list queries, MCP dashboards, SDK and proxy overhead) will be added with the
tools that measure them.

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
```

| Flag | Default | Meaning |
|---|---|---|
| `--base` | required | The base ref (A) |
| `--candidate` | `HEAD` | The candidate ref (B) |
| `--lane` | `1kb` | `1kb`: 1 KiB spans, 100 per request. `10kb`: 10 KiB spans, 10 per request, which keep the generator accurate ([`perf-baseline.md`](perf-baseline.md)) |
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
4. Runs the pairs. Each pair runs the throughput measurement for A and B back to back, then the latency
   measurement for both, with the same lane and offered load. The order alternates from pair to pair (AB, BA,
   AB, ...), so that slow drift in temperature or background load does not favour one arm. Every arm gets a fresh
   rig: ClickHouse, Postgres and Crucible (`serve --migrate`) from the arm's image on a new WAL volume, then a
   discarded warm-up at the latency rate, then the measurement, after which the rig is removed. The load
   generator's container lasts the whole session.
   - Throughput: offer more load than either build can take. The generator is open loop, so the excess is
     refused or queued, and the measurement is `outcome.acked_spans_per_s`. An overloaded schedule lasts until
     every request is sent, so the run takes about `requests / capacity`, longer than its schedule. Acknowledging
     less than the offered load shows that something saturated, not that Crucible did: when the generator or the
     Docker network is the limit, both arms measure that limit and no regression above it can show. The load
     generator's report tells them apart. With Crucible as the limit its 128 connections stay busy (acknowledged
     requests per second times `service_time_us.p50` comes near 128) while `generator_cpu` stays below the
     generator's CPUs.
   - Latency: offer a fixed rate below both builds' capacity, and the measurement is `latency_us.p99`.
5. Compares the builds, writes the report and the summary, and with `--post` sets the commit status.

| Rig | Throughput schedule | Latency schedule | Warm-up | `--recent-max-bytes` |
|---|---|---|---|---|
| `full` | 20,000 requests/s for 10 s | 1,000 requests/s for 30 s | 3 s | 256 MiB |
| `small` | 10,000 requests/s for 3 s | 100 requests/s for 5 s | 2 s | 64 MiB |

On the full rig the 1 KiB lane offers 2,000,000 spans/s for throughput and 100,000 for latency, and the 10 KiB
lane 200,000 and 10,000. Both measurements use 128 connections and uncompressed protobuf. The recent-span index
is bounded explicitly because its default is a share of the VM's memory, not the container's.

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
- An arm cannot be compared, which makes its metric's comparison **invalid**, when:
  - a throughput arm acknowledged 90% or more of the offered spans per second: the offered load did not exceed
    the build's capacity, so the value measures the schedule;
  - a latency arm had a failed request or a non-2xx response. `latency_us` covers 2xx responses only, so a stall
    longer than `--timeout-ms` shows up in `errors.timeout` rather than in the latency;
  - Crucible, ClickHouse or Postgres was not running after the measurement, for example after an OOM kill.

The verdict is `regression` when a valid, gated metric blocks, otherwise `invalid` when a comparison did not hold,
otherwise `pass`. The command exits with 0 for `pass`, 1 for `regression` or `invalid`, and 2 when it could not
run: a bad argument, a failed build, a rig that did not start.

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

The gate removes its rig on every exit it sees, errors included. A run that is killed, for example by stopping its
container, leaves the rig running. Everything the gate starts carries the label `crucible.perf-gate`, so this
removes what is left:

```sh
docker rm --force --volumes $(docker ps --all --quiet --filter label=crucible.perf-gate)
docker volume rm $(docker volume ls --quiet --filter label=crucible.perf-gate)
docker network rm $(docker network ls --quiet --filter label=crucible.perf-gate)
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
| `lane` | object | `name`, `span_size`, `spans_per_request`, `concurrency`, `warmup_s`, `throughput_requests_per_s`, `throughput_s`, `latency_requests_per_s`, `latency_s` |
| `base`, `candidate` | object | `reference` as given, `sha`, `image`, `image_id` |
| `loadgen_sha256` | string | The load generator binary that measured both arms |
| `pairs`, `order` | integer, array | The number of pairs, and each pair's order, `AB` or `BA` |
| `probe` | object | `verdict` (`honest`, `suspicious` or `failed`), `report` (path or null) and `error` (null, or why the probe failed) |
| `comparisons[]` | array | `throughput`, then `latency`, each with `metric`, `unit`, `better` (`higher` or `lower`), `base[]` and `candidate[]` (one value per pair), `median_base`, `median_candidate`, `change` (relative change of the median, null when the base median is 0), `differences[]` (B - A per pair), `wilcoxon` (`n` non-zero differences, `w_plus`, one-sided `p`), `threshold`, `gated`, `valid`, `notes[]` and `blocks` |
| `runs[]` | array | Every measured arm in run order: `pair`, `kind`, `arm`, `report`, `log`, `offered_spans_per_s`, `acked_spans_per_s`, `latency_p99_us`, `errors`, `non_2xx` and `invalid` (null, or why the arm cannot be compared) |
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
- **Identity.** Every request is one trace. Every span carries the attributes `loadgen.run_id` (the run id,
  minted fresh unless `--run-id` is given) and `loadgen.seq`, its sequence number
  `request_index * spans_per_request + position`, as 20 zero-padded digits. Trace ids are the run tag followed
  by a bijective mix of the request index, so they never repeat within a run. Span ids are the same mix of the
  sequence number. No id is ever zero. Span times follow the request's intended send time on the wall clock.

### How it sends

- **Open loop.** Request `i` is due at `start + i / rate`, computed from `i` alone. Responses never move the
  schedule.
- **Latency from the intended send time.** When the server stalls, requests that fall due during the stall wait,
  and the wait counts in their latency. A stalled server therefore cannot hide behind a stalled client
  (coordinated omission).
- **Bounded concurrency.** Each of `--concurrency` connections carries one request at a time (HTTP/1.1). A free
  connection takes the next request and prepares its body (the patch, and the gzip in the gzip lane) before the
  request is due.
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
`SequenceReader` is the interface a store-backed reader will implement: it returns the stored `loadgen.seq`
values of one run. `check_zero_loss` compares them with the ledger and counts lost and duplicated spans. No
reader exists yet, because ingest and the read API land in a later wave.

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
| `--gzip` | off | Gzip every body (`Content-Encoding: gzip`) |
| `--concurrency` | 64 | Connections, and so the most requests in flight |
| `--seed` | 1 | Content seed |
| `--run-id` | fresh UUIDv7 | The run id every span carries |
| `--templates` | 16 | Pre-encoded templates; memory is about `templates * spans_per_request * span size` |
| `--timeout-ms` | 10000 | Deadline for connecting and for each request |
| `--header` | none | Extra header as `Name: value`, repeatable; ingest needs an `Authorization` header (contract 12.2) |
| `--threads` | CPUs available | Runtime worker threads |
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
| `lane` | object | `span_bytes`, `spans_per_request`, `gzip`, `templates`, `request_bytes_mean` (protobuf body before compression), `wire_bytes_mean` (body on the wire) |
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

A summary has `count`, `p50`, `p90`, `p99`, `p99_9` and `max`, in microseconds, from an HDR histogram with three
significant digits.

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
