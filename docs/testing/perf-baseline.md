# Throughput baseline: milestone 1

The first end-to-end throughput measurement of `crucible serve`: OTLP ingest to the durable acknowledgement
(burst), and ingest through the shipper into ClickHouse (sustained). Every number on this page was **measured on a
laptop, not reference hardware**: the milestone 1 runs on one day with other jobs sharing the machine, and
[the shipper after #168](#the-shipper-after-168) and [after #192](#the-shipper-after-192) on later days without
them. Use it to find bottlenecks and to compare builds on the same machine, never as a capacity claim. The rig, the
load generator and the fsync-honesty probe are described in [`perf.md`](perf.md); `tools/perf/baseline.sh` repeats
every run below.

**Status: checkpoint, incomplete.** The work was paused on request before the sustained runs finished. What is
missing is listed under [Incomplete: remaining runs](#incomplete-remaining-runs). The shipper's phase split was
measured again on the gate's small rig with #168, which put the phase metric on `main`:
[the shipper after #168](#the-shipper-after-168), and with stage A and encoding off the shipper task in #192:
[the shipper after #192](#the-shipper-after-192).

## Summary

| v0 target ([`perf.md`](perf.md)) | Measured here |
|---|---|
| At least 250,000 spans/s of 1 KiB spans | Ack path alone (burst): median of the highest passing rates 475,000 spans/s plain (range 304,000 to 625,000) and 400,000 gzip (360,000 to 400,000). At exactly 250,000: plain passed 3 of 3 runs, gzip 1 of 3 |
| At least 50,000 spans/s of 10 KiB spans | Plain: median 30,400 (24,300 to 30,400); at exactly 50,000 it passed 2 of 3 runs, with 100 and with 10 spans per request. Gzip: not measurable with 1 MiB bodies on this client (below); with 10 spans per request it failed 3 of 3 at 50,000 |
| Durable-ack p99 at most 20 ms, on a volume the probe classified `honest` | The probe classified the WAL volume `honest` twice. p99 stayed within 20 ms up to the rates above in burst; with the shipper and ClickHouse on the same disk it reached 56 to 663 ms in the 120 s runs |
| Freshness at most 2 s p95 | 1.15 to 1.84 s in the passing 45 s runs up to 71,200 spans/s of 1 KiB; no 120 s run passed (below) |
| Zero acknowledged loss | Zero lost and zero duplicated in every sustained run: 57,808,500 acknowledged spans in 10 runs, checked by run id and sequence number |

- **The ack path is bound by the WAL's flushes on this disk, not by CPU.** Crucible used at most 2.2 of its 6 cores
  in any burst run on the resized rig. Steps failed when a few `fdatasync` calls took 25 to 250 ms, mostly while other jobs wrote to
  the same virtual disk.
- **End to end, the shipper's single task is the bottleneck.** Above the sustained limit the task was 91 to 98%
  busy while Crucible's other cores and ClickHouse (0.8 of 8 cores) idled, and it never waited for ClickHouse.
  Encoding takes half of the task's time, stage A a fifth.
- **Recommendation:** take work off the shipper task, encoding first, then stage A
  ([Recommendation](#recommendation)). #192 moved both off the task:
  [the shipper after #192](#the-shipper-after-192).

## The fsync-honesty probe

The probe ran on the WAL's named volume (ext4 on the VM's virtual disk), from the Crucible cpuset, before any load
and again after the rig was resized. Both passes classified it **`honest`**.

| Pass (UTC) | Hop p50 | 4 KiB sync p50 (p99) | 1 MiB sync p50 (p99) | 8 MiB sync p50 | Directory sync p50 | Checks, net ns (small write, scaling, directory) |
|---|---|---|---|---|---|---|
| 06:10, other builds running | 39 µs | 3.76 ms (7.76 ms) | 6.70 ms (12.56 ms) | 12.60 ms | 2.55 ms | 3,721,274; 8,838,067; 2,511,029 |
| 06:20, rig resized | 26 µs | 1.22 ms (2.02 ms) | 1.45 ms (7.56 ms) | 3.81 ms | 1.12 ms | 1,190,707; 2,594,853; 1,091,429 |

The same volume flushed about three times slower in the first pass, while other jobs wrote to the shared disk. That
spread recurs below: the ack path's tail follows the disk's slowest flushes.

## Rig

Measured on 2026-11-18 (UTC) on a laptop with an Intel Core Ultra 9 275HX (8 performance and 16 efficient cores)
and 32 GB of memory, on AC power, running Windows 11 and Docker Desktop 29.8.2 with the WSL2 VM (24 vCPUs, 16 GB,
kernel 6.18.58.1-microsoft-standard-WSL2). The VM's vCPUs float over the host's cores, so a cpuset pins a container
only within the VM.

| Container | Image | cpuset (vCPUs) | Memory | Notes |
|---|---|---|---|---|
| Crucible | `crucible-dev:1` running the release binary | 0-5 (6) | 2 GB | WAL on a named Docker volume, recreated empty for every burst step and for the sustained lane; `--recent-max-bytes 268435456` |
| ClickHouse | `clickhouse/clickhouse-server:26.8` (26.8.39.19) | 6-13 (8) | 6 GB | Data on a named Docker volume on the same virtual disk as the WAL |
| Client | `crucible-dev:1` | 14-17 (4) | 1 GB | `crucible-loadgen`, the metric scraper and the checks |

- The sizes leave room for other jobs in the VM's 16 GB; they replace the sizes in `perf.md` (ClickHouse 10 vCPUs
  and 9 GB, Crucible 3 GB, client 6 vCPUs). Other tracks compiled in Docker throughout. Each run records
  `docker stats` and the VM's load average before and after it; the tables name the other containers.
- **The other jobs' containers had no cpuset**, so they ran on the rig's vCPUs as well. The rig's cpusets keep its
  own containers apart, not other work out.
- The binaries were built on their own, `cargo build --locked -p crucible --release` and then `-p crucible-loadgen`,
  from `4a6e68b` plus the metrics commit `7801d64` of branch `feat/shipper-phase-metrics` (below), sha256
  `50b14e3de580f108...` for `crucible` and `e3edafdbeb514444...` for `crucible-loadgen`. Since then `main` has
  changed none of the code these runs exercise, apart from a simulation-only flag in ingest; its other changes are on
  the API's read side.
- At the time of these runs the recent-span index defaulted to 10% of the host's memory, at most 1 GiB. The default
  read `/proc/meminfo`, which shows the VM's 16 GB rather than the container's limit, so inside 2 GB it would have
  left too little for the shipper's batches. The runs bound it at 256 MiB. A sample during a sustained run showed 1.35 GiB in use and no memory
  events. The default has since taken the container's limit into account (about 205 MiB in 2 GB), and
  `tools/perf/baseline.sh` passes `--recent-max-bytes` only when `RECENT_MAX_BYTES` is set.

## Method

### Load

`crucible-loadgen run` sends OTLP/HTTP protobuf requests of 100 spans each (100 KiB bodies for 1 KiB spans,
1 MiB for 10 KiB spans), open loop, with latency measured from each request's intended send time. The connection
cap is the larger of 128 and the arrivals of 100 ms (`rate / 10`), so it binds only after a stall five times the
20 ms limit: below that the generator stays open loop. A first pass with a fixed cap of 128 let the client queue
behind its connections during server stalls (send lag p99 19 ms at 830,000 spans/s) and was discarded.

### Burst: the ack path alone

- Crucible runs without a telemetry store (ingest only), so a step measures receive, admission, the WAL group
  commit and the acknowledgement, without the shipper's CPU or ClickHouse's writes on the same disk.
- Every step starts a fresh Crucible on an empty WAL volume, sends a 3 s warm-up at half the rate, then measures
  15 s.
- A step passes when every request is acknowledged with 200, no response is 429 or 503, no request fails, and
  latency p99 is at most 20 ms.
- A search starts at a fixed rate, multiplies it by 5/4 until a step fails (or by 4/5 until one passes), then
  bisects twice, which brackets the highest passing rate to about 6%. Each lane was searched three times
  independently. Verdicts near the limit are not monotone, so each gate rate also got three runs of its own.

### Sustained: end to end

- Crucible runs with ClickHouse, the shipper and the recent-span index, on one fresh WAL volume per lane. The four
  milestone 1 tables are emptied (`TRUNCATE`) before every run, so every run starts from empty tables.
- A run sends at a fixed rate, then waits until `span_core` holds every acknowledged span (the drain).
- It passes when every request is acknowledged with 200, no response is 429 or 503, no request fails, and:
  - **zero loss**: the stored `loadgen.seq` values of the run's `loadgen.run_id` in `span_payload` cover every
    acknowledged sequence number exactly once, and `span_core` holds one row per span;
  - **freshness**: p95 at most 2 s over every span, and over the spans acknowledged in the run's last 30 s, so a
    backlog that grows during the run fails it even when the earlier spans were fresh. Freshness is measured per
    span in ClickHouse: from `received_at`, which the WAL committer stamps just before the frame's append and sync,
    to the end of the `span_core` insert that made the span readable, which `system.query_log` records with the
    insert's deduplication token. It starts before the sync, so it overstates ack-to-readable by the sync time;
  - the ack latency is reported, not judged.
- The WAL gauges are sampled every second: `crucible_wal_unshipped_bytes` and the LSN distance between the durable
  and the checkpointed log show whether the backlog stayed flat.
- A search with 45 s runs brackets the rate; the highest passing rate is then to be confirmed by three 120 s runs.

### Where the time goes

- CPU per container comes from each container's cgroup `cpu.stat`, read before and after the measured load with
  that container's clock, and is given in cores (CPU seconds per second).
- Inside Crucible, the runs used three metrics of commit `7801d64`, built on `4a6e68b`:
  - `crucible_shipper_task_microseconds_total{phase}`: the shipper task's time, charged to the phase it was in:
    `read` (the next durable frame), `stage_a` (decode and normalize), `stage_b` (duplicate window, first-seen set,
    recent-index insert), `rows` (table rows into the builders), `encode` (sort, Native encoding and compression
    of a sealed batch), `wal_append` (durable `BatchSeal` and `Checkpoint` frames, garbage collection),
    `wait_inserts` (the open batch is full and the most batches are in flight) and `wait_frames` (caught up with
    the durable log). The phases add up to the task's running time, which a directed simulation test checks.
    `main` has this counter since #168, which also charges the task's waits for the control store, its retries
    and its frame quarantine copies to `wait_frames`;
  - `crucible_shipper_insert_duration_microseconds`: each insert, from the send to the answer;
  - `crucible_wal_append_duration_microseconds`: each commit group's append, beside the existing sync histogram.
    `main` has neither histogram.
- `system.query_log` gives the inserts per table as ClickHouse saw them. Without the two histograms the script still
  runs and leaves those fields out.

## Results

### Burst

The highest passing rate of each search, with the details of that passing step.

| Lane | Search | Highest passing spans/s | Lowest failing | p50 / p99 / p99.9 ms | Crucible cores | WAL MB/s | Frames per group | Sync mean ms | Other containers during the search |
|---|---|---|---|---|---|---|---|---|---|
| 1 KiB plain | 1 | 625,000 | 664,000 | 3.7 / 10.3 / 15.8 | 1.51 | 226 | 13.9 | 1.86 | 3 |
| 1 KiB plain | 2 | 304,000 | 320,000 | 2.8 / 5.9 / 10.3 | 0.84 | 111 | 5.0 | 1.45 | 3 |
| 1 KiB plain | 3 | 475,000 | 500,000 | 3.5 / 14.5 / 79.0 | 1.23 | 173 | 10.2 | 1.79 | 1 |
| 1 KiB gzip | 1 | 360,000 | 380,000 | 3.4 / 11.8 / 80.4 | 1.52 | 131 | 6.7 | 1.64 | none |
| 1 KiB gzip | 2 | 400,000 | 425,000 | 3.5 / 13.1 / 68.9 | 1.70 | 146 | 7.6 | 1.66 | 2 |
| 1 KiB gzip | 3 | 400,000 | 425,000 | 4.1 / 18.3 / 74.9 | 1.77 | 146 | 8.8 | 1.90 | 2 |
| 10 KiB plain | 1 | 30,400 | 32,000 | 3.8 / 10.8 / 26.5 | 0.54 | 121 | 1.0 | 1.96 | 1 |
| 10 KiB plain | 2 | 30,400 | 32,000 | 4.5 / 18.1 / 68.0 | 0.60 | 120 | 1.0 | 2.34 | 3 |
| 10 KiB plain | 3 | 24,300 | 25,600 | 3.6 / 8.2 / 21.2 | 0.42 | 97 | 1.0 | 1.88 | 4 |
| 10 KiB gzip (client-limited) | 1 | 7,800 | 8,300 | 5.8 / 14.3 / 129.7 | 0.26 | 31 | 1.0 | 2.20 | 3 |
| 10 KiB gzip (client-limited) | 2 | 8,300 | 8,800 | 6.1 / 19.8 / 147.6 | 0.29 | 33 | 1.0 | 3.05 | 3 |
| 10 KiB gzip (client-limited) | 3 | 5,200 | 5,500 | 6.0 / 14.1 / 122.0 | 0.18 | 21 | 1.0 | 2.54 | 9 |

| Lane | Median of the highest passing rates | Range |
|---|---|---|
| 1 KiB plain | 475,000 spans/s | 304,000 to 625,000 |
| 1 KiB gzip | 400,000 spans/s | 360,000 to 400,000 |
| 10 KiB plain | 30,400 spans/s | 24,300 to 30,400 |
| 10 KiB gzip | not a Crucible measurement | the client was the limit (below) |

### At the gate rates

Three 15 s burst runs at exactly each gate rate.

| Lane | Spans per request | p99 of the three runs, ms | Passed | Slow syncs (over 25 ms) in the failures |
|---|---|---|---|---|
| 1 KiB plain at 250,000 spans/s | 100 | 9.5, 12.5, 17.5 | 3 of 3 | |
| 1 KiB gzip at 250,000 spans/s | 100 | 46.4, 32.6, 12.8 | 1 of 3 | 3, 1 |
| 10 KiB plain at 50,000 spans/s | 100 | 17.8, 148.0, 10.7 | 2 of 3 | 7 |
| 10 KiB plain at 50,000 spans/s | 10 | 12.6, 3,547, 9.5 | 2 of 3 | 19 |
| 10 KiB gzip at 50,000 spans/s | 10 | 274.2, 88.0, 42.8 | 0 of 3 | 2, 0, 0 |

### The client on its own

Against the load generator's stub server, which runs on Crucible's cpuset, inflates and decodes every body, and
answers 200 at once:

| Lane | Spans per request | Spans/s | p50 / p99 / p99.9 ms | Send lag p99 ms |
|---|---|---|---|---|
| 10 KiB gzip | 100 | 8,300 | 2.2 / 3.3 / 123.0 | 0.2 |
| 10 KiB gzip | 100 | 16,300 | 2.2 / 80.0 / 140.7 | 57.6 |
| 10 KiB gzip | 100 | 25,600 | 2.9 / 12.0 / 131.5 | 3.3 |
| 10 KiB plain | 100 | 30,400 | 0.6 / 11.6 / 32.1 | 5.2 |
| 10 KiB gzip | 10 | 50,000 | 0.4 / 1.3 / 67.5 | 0.5 |
| 10 KiB plain | 10 | 50,000 | 0.1 / 0.9 / 7.3 | 0.1 |
| 1 KiB gzip | 100 | 250,000 | 0.3 / 1.5 / 108.0 | 0.5 |

The generator gzips each body on its runtime's worker threads. With 1 MiB bodies on 4 vCPUs that delays its own
sends, so the 10 KiB gzip burst numbers above measure the client, not Crucible. With 10 spans per request it is
accurate at the gate rate, and its 10 KiB plain lane with 100 spans per request spends about half the latency
budget itself at 30,400 spans/s.

**Re-run on 2026-11-19 with the generator that compresses off its runtime's workers** (on 2 blocking threads, its
runtime on the other 2 of the client's 4 vCPUs), against the generator of `main` at that time, 10 KiB gzip spans,
100 per request, 15 s each, with `tools/perf/baseline.sh stub`, on a laptop, not reference hardware, with other
jobs on the machine. `main`'s generator ran twice at each rate:

| Generator | Spans/s | Send lag p50 / p99 / p99.9 ms | Latency p50 / p99 / p99.9 ms |
|---|---|---|---|
| `main`, gzip on the runtime's workers | 25,600 | 0.47 / 98.7 / 138.2 (second run: p99 73.7) | 3.7 / 165.0 / 272.9 |
| `main`, gzip on the runtime's workers | 16,300 | 0.01 / 74.1 / 208.8 (second run: p99 26.7) | 2.6 / 108.0 / 232.2 |
| gzip on 2 blocking threads | 25,600 | 0.008 / 0.060 / 24.4 | 2.3 / 3.5 / 28.8 |
| gzip on 2 blocking threads | 16,300 | 0.008 / 0.058 / 31.0 | 2.4 / 3.6 / 44.7 |

Send lag p99 fell below 0.1 ms at both rates. The generator spent about 5.3 ms of CPU per request, against 4.1 to
4.3 ms before, the cost of handing bodies to the blocking threads.

### Sustained, 1 KiB spans

| Run | Seconds | Spans/s | Freshness p50 / p95 / p99 s | Last 30 s p95 s | Ack p50 / p99 ms | Shipper busy | ClickHouse cores | Load (1 min) after | Verdict |
|---|---|---|---|---|---|---|---|---|---|
| Search | 45 | 60,000 | 0.77 / 1.15 / 1.25 | 1.18 | 2.5 / 13.2 | 0.73 | 0.68 | 6.5 | pass |
| Search | 45 | 67,500 | 1.02 / 1.58 / 1.75 | 1.64 | 2.3 / 12.8 | 0.90 | 0.76 | 7.1 | pass |
| Search | 45 | 71,200 | 1.13 / 1.84 / 2.13 | 1.93 | 2.2 / 11.2 | 0.93 | 0.76 | 6.8 | pass |
| Search | 45 | 75,000 | 1.94 / 4.33 / 4.73 | 4.48 | 2.9 / 20.0 | 0.95 | 0.80 | 7.5 | fail: backlog grew |
| Calibration | 45 | 100,000 | 8.70 / 20.93 / 21.68 | 21.21 | 2.9 / 18.9 | 0.98 | 0.79 | 5.7 | fail: backlog grew |
| Confirmation | 120 | 71,200 | 2.10 / 4.21 / 4.62 | 4.57 | 2.9 / 83.5 | 0.98 | 0.82 | 9.5 | fail: backlog grew |
| Confirmation | 120 | 71,200 | 1.64 / 5.28 / 5.57 | 5.54 | 3.6 / 287.2 | 0.97 | 0.80 | 6.2 | fail: backlog grew |
| Confirmation | 120 | 71,200 | 2.11 / 11.31 / 11.64 | 11.62 | 2.6 / 606.7 | 0.98 | 0.85 | 6.8 | fail: backlog grew |
| Confirmation | 120 | 64,000 | 1.18 / 2.25 / 2.50 | 2.43 | 2.1 / 55.7 | 0.91 | 0.84 | 8.8 | fail: backlog grew |
| Confirmation | 120 | 64,000 | 4.33 / 6.15 / 6.39 | 6.15 | 3.5 / 662.5 | 0.95 | 0.78 | 10.2 | fail: backlog grew |

- Every run acknowledged every request with 200, and lost and duplicated nothing.
- No rate held for 120 s, so the sustained limit is not confirmed. The 45 s runs passed up to 71,200 spans/s, but
  45 s hides a backlog that grows slowly; under heavier load from other jobs (load average up to 10) even 64,000
  spans/s fell behind over 120 s. The next confirmation, at 57,600 spans/s, was not run before the pause.
- The drain after each run took 3.3 to 4.9 s, except after the overloaded runs (11.4 and 21.5 s).
- Ack p99 with the store attached was 11 to 20 ms in the 45 s runs and 56 to 663 ms in the 120 s runs, which saw 23
  to 79 syncs above 25 ms each: ClickHouse's inserts and merges and the other jobs write to the same virtual disk as
  the WAL.

## Where the time goes

### The ack path: the WAL's flushes, not CPU

- At the highest passing burst rates Crucible used 0.4 to 1.8 of its 6 cores in the lanes the client did not
  limit. No burst run saturated its CPU; in the discarded first pass it acknowledged 1.3 million spans/s (the
  generator's limit) on 2.6 cores.
- The WAL committer was busy (append plus `fdatasync`) about 90% of the time at the 1 KiB limits, gathering 5 to
  14 frames of about 39 KB per group (LZ4 shrinks the 100 KiB bodies 2.6 times), at 1.5 to 1.9 ms per sync. Group
  commit keeps the median latency at 2.8 to 4.1 ms by growing the groups as the rate rises. With 10 KiB spans each
  1 MiB request is one frame of about 435 KB and nearly every group holds one frame; the committer was 48 to 71%
  busy at the passing rates.
- What failed a step was nearly always a handful of slow flushes: one 50 to 100 ms sync and three of 25 to 50 ms
  failed the 15 s step at 664,000 spans/s, because every request that arrives during a stall waits for it. The
  searches that ran while other jobs wrote to the disk found the lowest rates, which is why the three 1 KiB plain
  searches range from 304,000 to 625,000 spans/s.
- The gzip lanes spend more CPU per request (inflate) and show the largest tails that no slow sync explains, which
  fits the other jobs running on Crucible's vCPUs: at 50,000 spans/s of 10 KiB gzip spans in 10-span requests,
  1.2% of requests took more than 25 ms in the server's own handler histogram, with no sync above 25 ms.

### End to end: the shipper task

The evidence comes from the runs above the sustained limit, where the shipper falls behind; in a passing run it
keeps up by definition and shows idle time.

| 1 KiB, 120 s at 71,200 spans/s (fails) | Share of the shipper task's time |
|---|---|
| `encode`: sort, Native encoding, LZ4 and freeing a sealed batch's rows | 50.6% |
| `stage_a`: decode and normalize | 22.0% |
| `rows`: table rows into the builders | 14.3% |
| `stage_b`: duplicate window, first-seen set, recent-index insert | 8.1% |
| `read`, `wal_append` | 1.3%, 1.9% |
| `wait_frames` (caught up), `wait_inserts` (blocked on ClickHouse) | 1.4%, 0.0% |

- **The task is saturated** (busy 98.2%), and it is the only saturated resource: Crucible used 1.27 of its 6
  cores, about one of them this task's, and ClickHouse used 0.82 of its 8.
- **ClickHouse keeps up.** `wait_inserts` stayed at zero in every run: the open batch never filled while two
  batches were in flight. Inserts took 76 ms (`span_core`) and 224 ms (`span_payload`, with its text indexes) on
  average per block of about 72,000 rows, against a 500 ms linger.
- **Saturated, the task moves about 69,000 spans/s** (8,544,000 spans in the 120 s run and its 3.9 s drain), about
  14 µs of its time per span: encode 7.4 µs, stage A 3.2 µs, rows 2.1 µs, stage B 1.2 µs. Every saturated run gives
  the same split, for example the 100,000 spans/s calibration run (busy 98.4%; encode 50.7%, stage A 21.9%, rows
  14.1%, stage B 8.5%).
- The other jobs ran on Crucible's vCPUs too (load average up to 10), which slows this one task; on a quiet machine
  it would move somewhat more. The split between phases is the stable result.
- What taking work off the task could buy, at most, before the next resource limits (#192 measured what it
  bought: [the shipper after #192](#the-shipper-after-192)):
  - parallel stage A alone: 1 / (1 - 0.224) = 1.29 times, about 89,000 spans/s;
  - encoding off the task alone: 1 / (1 - 0.515) = 2.1 times, about 142,000 spans/s;
  - both: 1 / (1 - 0.739) = 3.8 times, about 265,000 spans/s, if Crucible's other cores and ClickHouse keep up (at
    69,000 spans/s they used about 1.3 and 0.8 cores).

  Shares are of the task's busy time, and the ceilings leave out the cost of handing work between tasks.

## Recommendation

The milestone 1 recommendation was to **take work off the shipper task, starting with encoding rather than stage
A**. #192 did both; [the shipper after #192](#the-shipper-after-192) has what it measured. The reasons, as
measured then:

- The single task capped a node at about 69,000 spans/s of 1 KiB spans end to end on this rig, a seventh of what its
  ack path took in a burst. A node acknowledging at its burst rate fills the WAL's high-water mark (90% of 16 GiB)
  within about two minutes, then answers 503: the shipper, not the ack path, set what a node sustained.
- Encoding was half of the task's time, and the pairs of a sealed batch are independent: each pair's block can be
  sorted, encoded and compressed by its own blocking job and kept for byte-identical retries, as contract 5.2
  requires, with stage B's order and the batch identities unchanged.
- Parallel stage A came next. Contract 3.2 defines stage A as pure and parallel per frame; alone it could buy at
  most 1.29 times, with encoding moved as well up to 3.8 times.
- Milestone 2 made the task's work per span larger: the full span model, GenAI mapping and compatibility mappers
  added stage A work, and the rollup and fact tables added rows and blocks per span.
- The rest was left to milestone 6: builders that write columns directly instead of rows of cells (`rows` and
  `encode` together were 65% of the task), the io_uring WAL backend, and the ack path's flush tail on real disks.

## The shipper after #168

Measured on 2026-11-21 (UTC) on the same laptop, **measured on a laptop, not reference hardware**, with no other
track's containers on the machine except where noted. #168 puts `crucible_shipper_task_microseconds_total{phase}` on `main`, finds
a frame's provisional heads in one pass, and wakes the shipper when the durable LSN moves. Its build is `870828b`;
the base is `main` at `c9e1758`. Every image is the gate's (`crucible-gate:<commit>`).

### The phase split on the small rig

`tools/perf/baseline.sh` at the gate's small-rig limits: Crucible CPUs 0-1 and 1 GB, ClickHouse CPUs 2-3 and
1.5 GB, the client CPUs 5-6 and 512 MB, the recent-span index bound at 64 MiB; 1 KiB spans, 100 per request in one
trace, 30 s of load, then the drain (`sustained-step 1k plain <spans/s> 30`).

| Run | Offered spans/s | Shipped spans/s, run and drain | Drain after the run | Freshness p95 | Ack p50 / p99 ms | Shipper busy | Lost, duplicated |
|---|---|---|---|---|---|---|---|
| Saturated | 100,000 | about 59,600 | 20.3 s | 19.5 s: the backlog grew | 2.7 / 24.0 | 95.4% | 0, 0 |
| Below capacity | 10,000 | 10,000 | 3.6 s | 0.57 s | 2.1 / 6.1 | 16.4% | 0, 0 |

| Phase | Saturated | Below capacity |
|---|---|---|
| `encode` | 46.1% | 5.4% |
| `stage_a` | 21.1% | 3.9% |
| `rows` | 16.5% | 2.7% |
| `stage_b` | 8.5% | 1.1% |
| `read` | 1.9% | 2.2% |
| `wal_append` | 1.3% | 1.0% |
| `wait_inserts` | 0.0% | 0.0% |
| `wait_frames` | 1.8% | 83.6% |

- The shares are of the window between the metric scrapes around the load (31.4 and 31.5 s). A column adds up to
  97 to 100%: the lap in progress at the second scrape is charged at its end.
- **On 2 CPUs the saturated split is milestone 1's on 6** (encode 50.6%, stage A 22.0%, rows 14.3%, stage B
  8.1%): the one shipper task is the limit, and nothing else is. ClickHouse kept up (`wait_inserts` zero; inserts
  of 132 ms for `span_core` and 255 ms for `span_payload` on average, in blocks of 60,000 rows, against the 500 ms
  linger), Crucible used 1.19 of its 2 CPUs and ClickHouse 0.70 of its 2.
- Of the task's busy time, encode is 48%, stage A 22%, rows 17% and stage B 9%. On this rig, encoding off the task
  could buy at most 1 / (1 - 0.483) = 1.9 times, and encoding with stage A 1 / (1 - 0.704) = 3.4 times, before the
  cost of handing work between tasks and the next limit.

### The deciding gates

`cargo xtask perf gate --base c9e1758 --candidate 870828b --rig small --pairs 7 --lane 1kb`, with `--path ack`
and with `--path e2e`, through `tools/perf/xtask.sh`, in three sessions. The probe classified the WAL volume
`honest` in each, no arm was invalid, and every verdict is `pass`:

| Session (UTC) | Path | Metric | Base median | Candidate median | Change | One-sided p | Second vs first |
|---|---|---|---|---|---|---|---|
| 06:48 | ack | Throughput, acknowledged spans/s | 594,588 | 584,396 | -1.7% | 0.53 | -2.0% |
| 06:48 | ack | Durable-ack p99, ms | 5.50 | 5.92 | +7.7% | 0.016 | +2.6% |
| 06:55 | e2e | Sustained, shipped spans/s | 61,472 | 62,803 | +2.2% | 0.59 | -0.3% |
| 06:55 | e2e | Freshness p95, ms | 570.3 | 568.4 | -0.3% | 0.96 | +0.1% |
| 18:56 | ack | Throughput, acknowledged spans/s | 565,460 | 599,348 | +6.0% | 0.96 | +1.7% |
| 18:56 | ack | Durable-ack p99, ms | 5.85 | 5.32 | -9.0% | 0.71 | +5.1% |

- The ack path's latency arm offers 10,000 spans/s, below the shipper's capacity, so the shipper is caught up and
  waits on the wake there; of #168's changes, the wake is the one most likely to reach the acknowledgement. In the
  first session durable-ack p99 rose in 6 of 7 pairs, by 0.4 ms in the median, below the 10% that blocks. The
  third session repeated the ack path with the same images to see whether the rise holds, and p99 fell in 5 of 7
  pairs. The small rig's arm-to-arm spread ([A/A calibration](perf.md#aa-calibration)) is larger than either
  change; the full rig decides.
- In the first two sessions the only container outside the session was the gate's own driver. In the third,
  another container also ran during 8 of its 28 runs.
- Freshness p95 is the 500 ms linger plus the insert; the wake's milliseconds do not show in it.

### Ack to readable

The wake ends the shipper's wait when a frame becomes durable, where before it looked every 5 ms. Three
measurements, each at a low rate, of the base and the candidate:

| Measurement | Base | Candidate |
|---|---|---|
| Read-barrier wait of a trace read by id right after its acknowledgement (100 reads, the server's `crucible_api_barrier_wait_microseconds{class="trace_by_id"}`) | mean 3.84 ms; 94 of 100 between 1 and 10 ms | mean 0.11 ms; all 100 at most 0.5 ms |
| The read bench's ack-to-API probe on the small rig (100 probes; lists per probe and ack to list p50, empty store, then loaded) | 2 lists, 18.2 ms; 2 lists, 18.8 ms | 2 lists, 18.0 ms; 1 list, 12.5 ms |
| `crates/crucible/tests/e2e.rs`, two runs, `ack_to_api_list` of its four traces, protobuf, JSON, gRPC, compat route (ms) | 13.6, 34.3, 43.7, 25.6; 15.2, 41.1, 44.3, 28.7 | 13.5, 33.1, 40.5, 25.1; 14.4, 34.4, 42.5, 25.0 |

- The read barrier isolates the shipper's part: a read by id waits until stage B has applied the durable LSN it
  saw. Each probe sent one trace over OTLP/JSON and read it by id in the same `curl` process on the baseline rig.
- The list probes wait for the list's own round trip (5 to 12 ms per list) and poll every 2 ms, so they show the
  change only as fewer lists; `e2e.rs` has four samples per run.

### Provisional heads in one pass

`cargo test --release -p crucible-shipper --bench heads -- --ignored --nocapture` derives the rows of a frame of
50,000 rootless traces of one span and of a frame of one rootless trace of 50,000 spans, medians of 7 alternating
rounds: 281.0 ms against 91.1 ms (3.09 times) before, 101.2 ms against 87.7 ms (1.15 times) after, and 96.1 ms
against 75.1 ms (1.28 times) in a second run after, on the branch merged with `main` at `2a48f47`. The request
shape of the gate's lanes, one trace per request, never exercised it.

## The shipper after #192

Measured on 2026-11-25 (UTC) on the same laptop, **measured on a laptop, not reference hardware**, at the gate's
small-rig limits (Crucible on 2 CPUs and 1 GB), with no other track's containers on the machine during the deciding
runs except the gate's own driver. #192 takes stage A and encoding off the shipper task: stage A runs as a blocking
job on the frames the shipper reads ahead, each block of a sealed batch is encoded by a blocking job, each under a
share of the CPU budget, stage B still takes the frames in LSN order, and while ingest holds more requests than its
prepare share runs at once the shipper does that work on its task. The base is `main` at `8134c64`; `main` since then
changes conformance tests and docs only. Every image is the gate's (`crucible-gate:<commit>`), and the `baseline.sh`
steps run the `crucible` binary of those images.

| Build | Stage A as a job | Encoding as a job |
|---|---|---|
| `fd9d0ac` | every frame over 16 KiB | every block |
| `f21827a` | a frame over 16 KiB that durable frames follow | the blocks of a batch that durable frames follow when it seals |
| `45daf03` | a frame over 16 KiB that durable frames follow | every block |

The branch ends with `fd9d0ac`'s behaviour: [where this leaves the shipper](#where-this-leaves-the-shipper).

### A batch seals once the batch before it is encoded

The first smoke run of the e2e path killed the candidate for memory: one sustained arm ended with Crucible
OOM-killed at 1 GB, and the other shipped 38,535 spans/s against about 60,400 for the base while its loss check
reported 102,400 spans missing from `span_payload` with `span_core` complete (the check reads `span_payload` once
`span_core` holds every span). A sealed batch kept its rows until a job encoded them, and while both batches in
flight waited for encoding the open batch grew past its linger toward its size limits. A batch now seals only once
every batch in flight has its blocks encoded, and a due batch takes no further frame meanwhile, so besides the open
batch only the batch being encoded holds rows. No arm of any later run was killed or lost a span.

### `fd9d0ac`: the deciding gates

`cargo xtask perf gate --base 8134c64 --candidate fd9d0ac --rig small --pairs 7 --lane 1kb`, with `--path ack` and
`--path e2e`, from 22:25 to 22:42 UTC. The probe classified the WAL volume `honest`.

| Path | Metric | Base median | Candidate median | Change | One-sided p | Second vs first | Result |
|---|---|---|---|---|---|---|---|
| ack | Throughput, acknowledged spans/s | 591,073 | 594,940 | +0.7% | 0.34 | +1.0% | no regression |
| ack | Durable-ack p99, ms | 4.73 | 6.23 | +31.6% | 0.11 | +5.9% | no regression, higher in 6 of 7 pairs |
| e2e | Sustained, shipped spans/s | 62,238 | 95,374 | +53.2% | | +2.2% | invalid |
| e2e | Freshness p95, ms | 565.84 | 516.90 | -8.6% | 1.00 | -0.1% | no regression, lower in 7 of 7 pairs |

- **The sustained comparison is invalid on the small rig**: its sustained schedule offers 100,000 spans/s, and 6 of
  the 7 candidate arms shipped 94,491 to 96,689 of them, past the 90% at which the gate takes the store to have kept
  up. The candidate kept up with the whole offered rate, so this rig cannot show its capacity.
- **Durable-ack p99 rose at 10,000 spans/s**, the rate of the ack path's latency arm and of the e2e path's freshness
  arm: in 6 of 7 pairs of the first, and in 5 of 7 of the second (median 5.06 against 6.23 ms). At that rate the
  shipper keeps up, so its jobs buy nothing and run beside ingest on the same 2 CPUs.

### Ack latency at 10,000 spans/s

`f21827a` runs the jobs only while the shipper is behind the log. To tell a shift from the small rig's p99 noise,
three images ran `baseline.sh sustained-step 1k plain 10000 60` in turn (base, `fd9d0ac`, `f21827a`, then each order
rotated), 6,000 requests per step against the gate's 500, from 23:15 to 23:29 UTC:

| Build | Ack p99 per step, ms | Median p99 | Median p99.9 | Freshness p95 | Jobs per step, stage A and encode |
|---|---|---|---|---|---|
| base `8134c64` | 4.79, 4.99, 5.01 | 4.99 | 8.8 | 564 to 567 ms | none |
| `fd9d0ac` | 7.21, 6.06, 6.62 | 6.62 | 15.6 | 516 ms | 6,000 and 590 |
| `f21827a` | 5.28, 4.60, 5.36 | 5.28 | 9.5 | 547 to 550 ms | 207 to 222 and 175 to 220 |

Every `fd9d0ac` step is above every base step; `f21827a`'s range overlaps base's. Ack p50 was 2.0 ms in every step.

### `f21827a`: the deciding gates and the shipper under overload

The same gates from 23:29 to 23:46 UTC, probe `honest`:

| Path | Metric | Base median | Candidate median | Change | One-sided p | Second vs first | Result |
|---|---|---|---|---|---|---|---|
| ack | Throughput, acknowledged spans/s | 611,825 | 600,461 | -1.9% | 0.23 | +0.6% | no regression |
| ack | Durable-ack p99, ms | 4.93 | 5.26 | +6.7% | 0.47 | -12.4% | no regression |
| e2e | Sustained, shipped spans/s | 63,116 | 64,187 | +1.7% | 0.66 | -4.2% | no regression |
| e2e | Freshness p95, ms | 564.47 | 547.85 | -2.9% | 1.00 | +0.1% | no regression |

The candidate's sustained arms ranged from 49,773 to 82,381 spans/s, three of them below their base arm: offered
100,000 spans/s, the shipper catches up often, and a batch that seals with it caught up is encoded on the task, about
300 ms of encoding for a batch of 40,000 spans, during which the backlog grows again.

`baseline.sh sustained-step 1k plain 150000 30` keeps the shipper behind for the whole run, so `f21827a` runs
every frame's stage A and every block's encoding as jobs. Two steps each, from 23:46 to 23:54 UTC; the shares are of
the window between the metric scrapes around the load (31.3 to 31.6 s):

| | Base | Base | `f21827a` | `f21827a` |
|---|---|---|---|---|
| Shipped spans/s, run and drain | 57,376 | 56,166 | 66,225 | 59,492 |
| Drain after the run | 48.4 s | 50.1 s | 38.0 s | 45.6 s |
| Ack p50 / p99 ms | 2.8 / 20.8 | 2.7 / 26.6 | 4.3 / 31.7 | 4.0 / 32.2 |
| Crucible CPUs used of 2 | 1.28 | 1.22 | 1.55 | 1.49 |
| Task: `encode` | 46.4% | 50.6% | 0.1% | 0.8% |
| Task: `stage_a` | 21.5% | 20.4% | 9.5% | 11.1% |
| Task: `rows` | 18.4% | 17.1% | 26.3% | 24.7% |
| Task: `stage_b` | 8.1% | 7.6% | 29.7% | 21.6% |
| Task: `read`, `wal_append` | 3.3% | 2.9% | 6.2% | 6.7% |
| Task: `wait_inserts`, `wait_stage_a`, `wait_frames` | 0.0%, 0.0%, 1.4% | 0.0%, 0.0%, 1.3% | 14.8%, 12.2%, 1.3% | 20.8%, 12.7%, 1.6% |
| Encoding jobs' hold, share of the window | | | 87.7% | 85.4% |
| Stage A jobs' hold, share of the window | | | 30.6% | 28.7% |

- Nothing was lost or duplicated in any step.
- **Under overload the parallel shipper moves 4 to 18% more than base on 2 CPUs**, against a ceiling of 3.4 times for
  both stages off the task. The encode share has one permit on 2 CPUs (a sixteenth of 16 permits, at least one), and
  its jobs hold it 85 to 88% of the time, at about 13 to 14 µs of wall time per span where the task spent 8 to 9 µs
  encoding in base; the task waits for encoding (`wait_inserts`) 15 to 21% of its time under the seal rule.
- **Stage B and the rows cost about twice as much per span on the task** with stage A in jobs: about 8 µs against
  about 4.6 µs in base. The records reach the task from another thread, and the rows leave it to another thread to
  be encoded and freed.
- Offered 100,000 spans/s, `fd9d0ac` kept up (above); offered 150,000, ingest takes more of the 2 CPUs and the
  shipper shares them with it.

### `45daf03`: encoding by jobs moves the ack p99

`45daf03` keeps `f21827a`'s gate on stage A and encodes every block by a job, as `fd9d0ac` does. The rule for its
rotation was written before the run: shifted when every one of its steps has a higher ack p99 than every base step.
From 00:16 to 00:30 UTC on 2026-11-26:

| Build | Ack p99 per step, ms | Median p99 | Median p99.9 | Freshness p95 | Jobs per step, stage A and encode |
|---|---|---|---|---|---|
| base `8134c64` | 5.14, 5.60, 8.90 | 5.60 | 13.5 | 564 to 568 ms | none |
| `fd9d0ac` | 6.88, 7.01, 7.41 | 7.01 | 16.7 | 516 ms | 6,000 and 590 |
| `45daf03` | 7.32, 7.54, 6.78 | 7.32 | 14.0 | 515 to 516 ms | 48 to 59 and 590 |

The third base step held a stall (p99.9 93.7 ms), so the ranges overlap and the rule read "level", and the gates ran,
from 00:30 to 00:47 UTC, probe `honest`:

| Path | Metric | Base median | Candidate median | Change | One-sided p | Second vs first | Result |
|---|---|---|---|---|---|---|---|
| ack | Throughput, acknowledged spans/s | 609,373 | 593,570 | -2.6% | 0.078 | +0.7% | no regression |
| ack | Durable-ack p99, ms | 5.23 | 6.36 | +21.6% | 0.039 | +10.6% | **regression**, higher in 6 of 7 pairs |
| e2e | Sustained, shipped spans/s | 63,576 | 95,423 | +50.1% | | -1.1% | invalid: 5 of 7 candidate arms kept up |
| e2e | Freshness p95, ms | 565.29 | 516.25 | -8.7% | 1.00 | +0.4% | no regression, lower in 7 of 7 pairs |

- The ack p99 of the e2e path's freshness arms, at the same 10,000 spans/s, was higher in 7 of 7 pairs (4.1 to 5.3
  ms against 6.4 to 7.6 ms).
- With 48 to 59 stage A jobs per step against `fd9d0ac`'s 6,000, the ack p99 rose as much: encoding by jobs is what
  moves it, and the gate on stage A buys nothing.

`baseline.sh sustained-step 1k plain 150000 30`, two steps each, from 00:47 to 00:58 UTC:

| | Base | Base | `45daf03` | `45daf03` | `fd9d0ac` | `fd9d0ac` |
|---|---|---|---|---|---|---|
| Shipped spans/s, run and drain | 57,288 | 58,901 | 65,208 | 61,551 | 62,665 | 60,918 |
| Ack p50 / p99 ms | 2.9 / 29.6 | 2.7 / 21.3 | 4.0 / 42.7 | 4.1 / 33.8 | 4.2 / 46.1 | 4.6 / 32.9 |
| Crucible CPUs used of 2 | 1.24 | 1.26 | 1.59 | 1.45 | 1.52 | 1.56 |
| Encoding jobs' hold, share of the window | | | 83.5% | 86.9% | 84.9% | 86.0% |

Nothing was lost or duplicated in any step.

### Where this leaves the shipper

The branch keeps `fd9d0ac`'s behaviour: stage A as a job on every frame over 16 KiB, encoding by a job for every
block, the seal rule, and the work on the task while ingest is busy. On the gate's small rig, 2 CPUs:

- **Offered 100,000 spans/s it keeps up**: 6 of the 7 arms of `fd9d0ac`'s gate shipped 94,491 to 96,689 spans/s,
  the seventh 81,721, where base ships about 62,000. The gate cannot compare the two: its small rig offers 100,000
  spans/s, and an arm that ships 90% of that counts as not saturated, so a build that gains more than about 45% reads
  `invalid` there.
- **Offered 150,000 spans/s**, ingest takes more of the 2 CPUs: about 61,000 to 63,000 against 56,000 to 59,000.
- **Freshness p95 at 10,000 spans/s** falls from about 565 to about 516 ms: a pair's insert starts once its own block
  is encoded.
- **Durable-ack p99 at 10,000 spans/s** rises by about 1 to 2.5 ms, from about 5 ms to 6 to 7.5 ms: every rotation
  step of a build that encodes by jobs is above the base median, `45daf03`'s gate reads regression, and the
  freshness arms rose in 12 of 14 pairs. Gating the encoding on the log's state took the rise away, and the gain
  with it (`f21827a`). The full rig, with 6 CPUs, is where the durable-ack p99 decides ([`perf.md`](perf.md)).
- Untested lead for the next round: stage B and the rows cost about twice as much per span on the task, and
  encoding 1.5 to 1.8 times as much, when records and rows are built on one thread and freed on another.

## Incomplete: remaining runs

- Sustained 1 KiB: no rate is confirmed. The next step was three 120 s runs at 57,600 spans/s, then 10% lower until
  three pass, preferably with no other jobs on the machine.
- Sustained 10 KiB: not run. The search was to start at 8,000 spans/s.
- Burst 10 KiB gzip with 100 spans per request: the generator now gzips off its runtime's workers and stays
  accurate at 25,600 spans/s (above), but the burst searches of that lane are still to repeat; only the 10-span
  gate runs measure Crucible in it.
- Burst searches for 10 KiB spans with 10 spans per request: only the gate runs were done.
- A repeat on a quiet machine: other jobs' containers had no cpuset, and the burst spread follows them.

## Reproducing

From the repository root, with release builds of `crucible` and `crucible-loadgen` in a cargo target volume:

```sh
TARGET_VOLUME=crucible-target tools/perf/baseline.sh all
```

`up`, `probe`, `burst`, `gate`, `sustained`, `summarize` and `down` run the pieces one at a time; the script's
header lists their arguments, and its variables set the cpusets, memory limits and run lengths. Results land in
`perf/results/local/baseline/` (ignored by git): every run's load generator report, CPU and metric snapshots,
`docker stats` and load average, its verdict and analysis, and `results.md` with the tables.
