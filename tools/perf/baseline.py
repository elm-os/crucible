#!/usr/bin/env python3
"""Helpers for tools/perf/baseline.sh. They run inside the rig's client container, use only the standard
library, and read and write the results directory mounted at /out.

Subcommands:
  keys             write the project key file and the ClickHouse and master secrets
  scrape           save one /metrics page
  sample           save the WAL and shipper gauges once a second while a run lasts
  verdict-burst    judge a burst run against the ack-path criteria
  check-sustained  wait for a sustained run to drain, then check zero loss and freshness in ClickHouse
  analyze          CPU per container and the time split inside Crucible, from the snapshots of a run
  truncate         empty the telemetry tables between sustained runs
  summarize        collect every run into results.json and results.md
"""

import argparse
import json
import os
import re
import secrets
import statistics
import string
import sys
import time
import urllib.error
import urllib.request
import uuid

OUT = "/out"
CH_URL = "http://perf-clickhouse:8123"
METRICS_URL = "http://perf-crucible:9464/metrics"
CH_USER = "crucible_admin"
TABLES = ("span_core", "span_payload", "trace_heads", "trace_stats")
# The v0 targets of docs/testing/perf.md.
ACK_P99_LIMIT_US = 20_000
FRESHNESS_P95_LIMIT_S = 2.0
# A sustained run passes only if the spans acknowledged in its last TAIL_S seconds are fresh as well, so that a
# backlog that grows through the run cannot hide behind the earlier spans.
TAIL_S = 30
DRAIN_LIMIT_S = 180
PHASES = ("read", "stage_a", "stage_b", "rows", "encode", "wal_append", "wait_inserts", "wait_frames")


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def dump(path, value):
    write(path, json.dumps(value, indent=1, sort_keys=True) + "\n")


# ClickHouse over HTTP, as the admin user.


def password():
    with open(os.path.join(OUT, "rig", "clickhouse-password"), encoding="utf-8") as f:
        return f.read().strip()


def ch(sql, timeout=600):
    request = urllib.request.Request(
        CH_URL + "/",
        data=sql.encode(),
        headers={"X-ClickHouse-User": CH_USER, "X-ClickHouse-Key": password()},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.read().decode()
    except urllib.error.HTTPError as e:
        raise SystemExit(f"clickhouse refused: {e.read().decode()[:2000]}\n{sql}") from e


def ch_rows(sql):
    text = ch(sql + "\nFORMAT JSONEachRow")
    return [json.loads(line) for line in text.splitlines() if line.strip()]


# Prometheus text.

SAMPLE = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+(\S+)$")
LABEL = re.compile(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"')


def parse_metrics(text):
    """{(name, ((label, value), ...)): float}"""
    out = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        m = SAMPLE.match(line)
        if not m:
            continue
        labels = tuple(sorted(LABEL.findall(m.group(2) or "")))
        out[(m.group(1), labels)] = float(m.group(3))
    return out


def metric(series, name, **labels):
    return series.get((name, tuple(sorted(labels.items()))), 0.0)


def fetch_metrics():
    with urllib.request.urlopen(METRICS_URL, timeout=10) as response:
        return response.read().decode()


def cmd_keys(a):
    rig = os.path.join(OUT, "rig")
    alphabet = string.digits + string.ascii_uppercase + string.ascii_lowercase
    # 43 base62 digits encode 32 bytes only below 2^256; a leading zero keeps the value in range.
    secret = "csk_0" + "".join(secrets.choice(alphabet) for _ in range(42))
    public = "cpk_0" + "".join(secrets.choice(alphabet) for _ in range(42))
    project = str(uuid.uuid4())
    write(
        os.path.join(rig, "keys.toml"),
        f'[[project_key]]\nproject_id = "{project}"\npublic_key = "{public}"\nsecret = "{secret}"\n',
    )
    dump(os.path.join(rig, "project.json"), {"project_id": project})
    write(os.path.join(rig, "secret"), secret + "\n")
    write(os.path.join(rig, "clickhouse-password"), secrets.token_hex(16) + "\n")
    write(os.path.join(rig, "master-key"), secrets.token_hex(32) + "\n")


def cmd_scrape(a):
    text = fetch_metrics()
    write(a.out, f"# scraped_at {time.time()}\n{text}")


def scraped_at(text):
    return float(text.split("\n", 1)[0].split()[-1])


def run_times(run):
    """Wall-clock seconds of the run's load, from the client container's clock (the VM clock that the server's
    `received_at` also comes from)."""

    def ns(name):
        with open(os.path.join(run, name), encoding="utf-8") as f:
            return int(f.read().strip()) / 1e9

    return {"loadgen_start": ns("loadgen-start.ns"), "loadgen_end": ns("loadgen-end.ns")}


GAUGES = (
    "crucible_wal_durable_lsn",
    "crucible_wal_checkpoint_lsn",
    "crucible_wal_unshipped_bytes",
    "crucible_wal_segment_bytes",
    "crucible_ingest_inflight_bytes",
)


def cmd_sample(a):
    end = time.time() + a.seconds
    with open(a.out, "w", encoding="utf-8") as f:
        while time.time() < end:
            t = time.time()
            try:
                series = parse_metrics(fetch_metrics())
            except OSError as e:
                f.write(json.dumps({"t": t, "error": str(e)}) + "\n")
            else:
                row = {"t": t}
                for name in GAUGES:
                    row[name] = metric(series, name)
                for phase in PHASES:
                    row[phase] = metric(series, "crucible_shipper_task_microseconds_total", phase=phase)
                f.write(json.dumps(row) + "\n")
            f.flush()
            time.sleep(max(0.0, a.interval - (time.time() - t)))


def loadgen_summary(report):
    o = report["outcome"]
    statuses = {int(k): v for k, v in o["status_counts"].items()}
    errors = sum(o["errors"].values())
    return {
        "run_id": report["run_id"],
        "span_bytes": report["lane"]["span_bytes"],
        "gzip": report["lane"]["gzip"],
        "spans_per_request": report["lane"]["spans_per_request"],
        "request_bytes_mean": report["lane"]["request_bytes_mean"],
        "wire_bytes_mean": report["lane"]["wire_bytes_mean"],
        "target_rps": report["schedule"]["target_rps"],
        "target_spans_per_s": report["schedule"]["target_rps"] * report["lane"]["spans_per_request"],
        "duration_s": report["schedule"]["duration_s"],
        "requests": report["schedule"]["requests"],
        "sent": o["sent"],
        "acked": o["acked"],
        "acked_spans": o["acked_spans"],
        "achieved_rps": o["achieved_rps"],
        "acked_spans_per_s": o["acked_spans_per_s"],
        "statuses": statuses,
        "errors": o["errors"],
        "error_count": errors,
        "rejected_spans": o["rejected_spans"],
        "latency_us": report["latency_us"],
        "service_time_us": report["service_time_us"],
        "send_lag_us": report["send_lag_us"],
        "not_acked_requests": report["not_acked_requests"],
        "generator_cpu": report["generator_cpu"],
    }


def ack_reasons(s, latency=True):
    """Why a run fails the ack-path criteria; empty when it passes. A sustained run is judged by its backlog,
    freshness and loss instead of the ack latency, which it reports."""
    reasons = []
    throttled = s["statuses"].get(429, 0) + s["statuses"].get(503, 0)
    if throttled:
        reasons.append(f"{throttled} responses were 429 or 503")
    others = {k: v for k, v in s["statuses"].items() if k not in (200, 429, 503)}
    if others:
        reasons.append(f"other statuses {others}")
    if s["error_count"]:
        reasons.append(f"errors {s['errors']}")
    if s["acked"] != s["requests"]:
        reasons.append(f"{s['requests'] - s['acked']} of {s['requests']} requests were not acknowledged")
    if latency and s["latency_us"]["p99"] > ACK_P99_LIMIT_US:
        reasons.append(f"p99 {s['latency_us']['p99'] / 1000:.1f} ms above {ACK_P99_LIMIT_US / 1000:.0f} ms")
    return reasons


def no_report(run, kind):
    """A run whose generator wrote no report: it exits that way when the target refuses the initial
    connections."""
    if os.path.exists(os.path.join(run, "loadgen.json")):
        return False
    dump(os.path.join(run, "verdict.json"), {"kind": kind, "pass": False, "reasons": ["no load generator report"]})
    print("FAIL")
    return True


def cmd_verdict_burst(a):
    if no_report(a.run, "burst"):
        return
    s = loadgen_summary(load(os.path.join(a.run, "loadgen.json")))
    reasons = ack_reasons(s)
    verdict = {"kind": "burst", "pass": not reasons, "reasons": reasons, **s}
    dump(os.path.join(a.run, "verdict.json"), verdict)
    print("PASS" if verdict["pass"] else "FAIL")


def loss_sql(project, run_id, s):
    """Stored sequence numbers of the run in span_payload, against the acknowledged requests."""
    spans = s["spans_per_request"]
    excluded = " OR ".join(
        f"(intDiv(seq, {spans}) >= {lo} AND intDiv(seq, {spans}) < {hi})" for lo, hi in s["not_acked_requests"]
    )
    acked = f"NOT ({excluded})" if excluded else "1"
    total = s["requests"] * spans
    return f"""
SELECT count() AS rows, uniqExact(seq) AS distinct_seqs, uniqExactIf(seq, {acked}) AS acked_seqs_stored,
       countIf(seq >= {total}) AS outside_schedule
FROM (
  SELECT toUInt64(JSONExtractString(attributes, 'loadgen.seq')) AS seq
  FROM crucible.span_payload
  WHERE project_id = toUUID('{project}') AND JSONExtractString(attributes, 'loadgen.run_id') = '{run_id}'
)"""


def freshness_sql(project, tail_from_micros):
    """Freshness of every span of the project: from `received_at`, which the WAL committer stamps just before the
    frame's append and sync (so the ack follows it by the sync), to the end of the span_core insert that made it
    readable, from the insert's deduplication token in system.query_log."""
    return f"""
SELECT count() AS spans,
       quantileExact(0.5)(f) AS p50_s, quantileExact(0.95)(f) AS p95_s, quantileExact(0.99)(f) AS p99_s,
       max(f) AS max_s,
       countIf(r >= {tail_from_micros}) AS tail_spans,
       quantileExactIf(0.95)(f, r >= {tail_from_micros}) AS tail_p95_s,
       uniqExact(b) AS batches
FROM (
  SELECT dateDiff('microsecond', s.received_at, i.visible_at) / 1e6 AS f,
         toUnixTimestamp64Micro(s.received_at) AS r, s.batch_seq AS b
  FROM crucible.span_core AS s
  INNER JOIN (
    SELECT toUUID(t[1]) AS epoch, toUInt64(splitByChar('-', t[2])[1]) AS seq,
           min(event_time_microseconds) AS visible_at
    FROM (
      SELECT splitByChar(':', Settings['insert_deduplication_token']) AS t, event_time_microseconds
      FROM system.query_log
      WHERE type = 'QueryFinish' AND query_kind = 'Insert' AND event_date >= yesterday()
        AND Settings['insert_deduplication_token'] != ''
    )
    WHERE t[3] = '1'
    GROUP BY epoch, seq
  ) AS i ON s.wal_epoch = i.epoch AND s.batch_seq = i.seq
  WHERE s.project_id = toUUID('{project}')
)"""


def insert_stats_sql(start, end):
    return f"""
SELECT tables[1] AS tbl, count() AS inserts, sum(written_rows) AS rows,
       round(avg(query_duration_ms), 1) AS mean_ms, quantileExact(0.5)(query_duration_ms) AS p50_ms,
       quantileExact(0.95)(query_duration_ms) AS p95_ms, max(query_duration_ms) AS max_ms
FROM system.query_log
WHERE type = 'QueryFinish' AND query_kind = 'Insert' AND event_date >= yesterday()
  AND event_time_microseconds >= fromUnixTimestamp64Micro({int(start * 1e6)})
  AND event_time_microseconds <= fromUnixTimestamp64Micro({int(end * 1e6)})
  AND Settings['insert_deduplication_token'] != ''
GROUP BY tbl ORDER BY tbl"""


def cmd_check_sustained(a):
    if no_report(a.run, "sustained"):
        return
    s = loadgen_summary(load(os.path.join(a.run, "loadgen.json")))
    times = run_times(a.run)
    project = load(os.path.join(OUT, "rig", "project.json"))["project_id"]
    reasons = ack_reasons(s, latency=False)
    # Drain: wait until span_core holds every acknowledged span of the run.
    count_sql = f"SELECT count() FROM crucible.span_core WHERE project_id = toUUID('{project}')"
    drain_start = time.time()
    stored = 0
    while True:
        stored = int(ch(count_sql).strip() or 0)
        if stored >= s["acked_spans"] or time.time() - drain_start > DRAIN_LIMIT_S:
            break
        time.sleep(0.25)
    drained_after_s = time.time() - times["loadgen_end"]
    if stored < s["acked_spans"]:
        # The shipper is still behind: baseline.sh restarts Crucible on a fresh WAL before the next run, so that
        # the next run does not inherit this backlog.
        write(os.path.join(a.run, "drain-timed-out"), f"{stored} of {s['acked_spans']}\n")
        reasons.append(f"not drained after {DRAIN_LIMIT_S} s: {stored} of {s['acked_spans']} spans stored")
    ch("SYSTEM FLUSH LOGS")
    loss = ch_rows(loss_sql(project, s["run_id"], s))[0]
    core = ch_rows(
        f"SELECT count() AS rows, uniqExact(trace_id, span_id) AS distinct_spans FROM crucible.span_core "
        f"WHERE project_id = toUUID('{project}')"
    )[0]
    lost = s["acked_spans"] - int(loss["acked_seqs_stored"])
    duplicates = int(loss["rows"]) - int(loss["distinct_seqs"])
    core_duplicates = int(core["rows"]) - int(core["distinct_spans"])
    tail_from = int((times["loadgen_start"] + s["duration_s"] - TAIL_S) * 1e6)
    fresh = ch_rows(freshness_sql(project, tail_from))[0]
    inserts = ch_rows(insert_stats_sql(times["loadgen_start"], time.time()))
    if lost:
        reasons.append(f"{lost} acknowledged spans are missing")
    if duplicates or core_duplicates:
        reasons.append(f"duplicates: {duplicates} in span_payload, {core_duplicates} in span_core")
    if int(fresh["spans"]) < s["acked_spans"]:
        reasons.append(f"freshness covers {fresh['spans']} of {s['acked_spans']} spans")
    if float(fresh["p95_s"]) > FRESHNESS_P95_LIMIT_S:
        reasons.append(f"freshness p95 {float(fresh['p95_s']):.2f} s above {FRESHNESS_P95_LIMIT_S} s")
    if int(fresh["tail_spans"]) and float(fresh["tail_p95_s"]) > FRESHNESS_P95_LIMIT_S:
        reasons.append(f"freshness p95 of the last {TAIL_S} s {float(fresh['tail_p95_s']):.2f} s: the backlog grew")
    check = {
        "kind": "sustained",
        "pass": not reasons,
        "reasons": reasons,
        "drained_after_s": round(drained_after_s, 2),
        "stored_span_core": stored,
        "loss": {
            "acked_spans": s["acked_spans"],
            "lost_spans": lost,
            "duplicate_spans": duplicates,
            "span_core_duplicates": core_duplicates,
            **loss,
        },
        "freshness": fresh,
        "inserts": inserts,
        **s,
    }
    dump(os.path.join(a.run, "verdict.json"), check)
    print("PASS" if check["pass"] else "FAIL")


def cgroup(path):
    """cpu.stat of every container, as baseline.sh writes it: a `== name` header, `time_ns <ns>` from that
    container's clock, then the `key value` lines."""
    out, name = {}, None
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if line.startswith("== "):
                name = line[3:]
                out[name] = {}
            elif name and line:
                key, _, value = line.partition(" ")
                if value.isdigit():
                    out[name][key] = int(value)
    return out


def threads(path):
    """The snapshot's time and the CPU seconds per thread name of the Crucible process: the first line is the
    time in nanoseconds, then one /proc/1/task/*/stat line per thread."""
    out, at = {}, None
    with open(path, encoding="utf-8") as f:
        for line in f:
            if at is None:
                at = int(line.strip()) / 1e9
                continue
            if ")" not in line:
                continue
            name = line[line.index("(") + 1 : line.rindex(")")]
            fields = line[line.rindex(")") + 2 :].split()
            ticks = int(fields[11]) + int(fields[12])
            out[name] = out.get(name, 0) + ticks / 100.0
    return at, out


def histogram(series, name):
    return metric(series, name + "_count"), metric(series, name + "_sum")


def context(run):
    """What else ran: the containers of `docker stats` other than the rig's, and the VM's load average, before
    and after the run."""
    out = {}
    for tag in ("before", "after"):
        others = []
        path = os.path.join(run, f"{tag}-docker-stats.jsonl")
        if os.path.exists(path):
            for line in open(path, encoding="utf-8"):
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if not row.get("Name", "").startswith("perf-"):
                    others.append(f"{row['Name']} {row['CPUPerc']} {row['MemUsage'].split(' / ')[0]}")
        loadavg = os.path.join(run, f"{tag}-loadavg.txt")
        load1 = open(loadavg, encoding="utf-8").read().split()[0] if os.path.exists(loadavg) else None
        out[tag] = {"load1": load1, "other_containers": others}
    return out


def cmd_analyze(a):
    times = run_times(a.run)
    wall = times["loadgen_end"] - times["loadgen_start"]
    before = cgroup(os.path.join(a.run, "before-cgroup.txt"))
    after = cgroup(os.path.join(a.run, "after-cgroup.txt"))
    cpu = {}
    for name in after:
        b, e = before.get(name, {}), after[name]
        if "usage_usec" in b and "usage_usec" in e:
            window = (e["time_ns"] - b["time_ns"]) / 1e9
            cpu[name] = {
                "window_s": round(window, 2),
                "cores": round((e["usage_usec"] - b["usage_usec"]) / 1e6 / window, 2),
                "user_cores": round((e["user_usec"] - b["user_usec"]) / 1e6 / window, 2),
                "system_cores": round((e["system_usec"] - b["system_usec"]) / 1e6 / window, 2),
                "throttled_s": round((e.get("throttled_usec", 0) - b.get("throttled_usec", 0)) / 1e6, 2),
            }
    analysis = {"loadgen_s": round(wall, 2), "cpu": cpu, "context": context(a.run)}
    tb, ta = os.path.join(a.run, "before-threads.txt"), os.path.join(a.run, "after-threads.txt")
    if os.path.exists(tb) and os.path.exists(ta):
        (at0, t0), (at1, t1) = threads(tb), threads(ta)
        window = at1 - at0
        analysis["crucible_threads_cores"] = {
            k: round((t1[k] - t0.get(k, 0)) / window, 2) for k in sorted(t1) if t1[k] - t0.get(k, 0) > 0.05 * window
        }
    text0 = open(os.path.join(a.run, "before.prom"), encoding="utf-8").read()
    text1 = open(os.path.join(a.run, "after.prom"), encoding="utf-8").read()
    m0, m1 = parse_metrics(text0), parse_metrics(text1)
    scrape_wall = scraped_at(text1) - scraped_at(text0)
    analysis["scrape_window_s"] = round(scrape_wall, 2)

    def delta(name, **labels):
        return metric(m1, name, **labels) - metric(m0, name, **labels)

    groups = delta("crucible_wal_sync_duration_microseconds_count")
    sync_s = delta("crucible_wal_sync_duration_microseconds_sum") / 1e6
    append_s = delta("crucible_wal_append_duration_microseconds_sum") / 1e6
    frames = delta("crucible_wal_appended_frames_total")
    wal_bytes = delta("crucible_wal_appended_bytes_total")
    def slower(name, bound):
        """Observations above `bound` during the run, from the cumulative buckets."""
        above = lambda m: metric(m, name + "_count") - metric(m, name + "_bucket", le=str(bound))
        return int(above(m1) - above(m0))

    analysis["wal"] = {
        "syncs_over_25ms": slower("crucible_wal_sync_duration_microseconds", 25000),
        "syncs_over_100ms": slower("crucible_wal_sync_duration_microseconds", 100000),
        "groups": int(groups),
        "frames_per_group": round(frames / groups, 1) if groups else None,
        "bytes_per_group": round(wal_bytes / groups) if groups else None,
        "wal_mb_per_s": round(wal_bytes / 1e6 / scrape_wall, 1),
        "sync_mean_ms": round(sync_s * 1e3 / groups, 2) if groups else None,
        "append_mean_ms": round(append_s * 1e3 / groups, 2) if groups else None,
        "committer_busy": round((sync_s + append_s) / scrape_wall, 3),
    }
    phases = {p: delta("crucible_shipper_task_microseconds_total", phase=p) / 1e6 for p in PHASES}
    if sum(phases.values()) > 0:
        analysis["shipper_task_share"] = {p: round(v / scrape_wall, 3) for p, v in phases.items()}
        analysis["shipper_task_busy"] = round(
            sum(v for p, v in phases.items() if not p.startswith("wait_")) / scrape_wall, 3
        )
        count, total = histogram(m1, "crucible_shipper_insert_duration_microseconds")
        count0, total0 = histogram(m0, "crucible_shipper_insert_duration_microseconds")
        inserts = count - count0
        analysis["inserts"] = {
            "count": int(inserts),
            "mean_ms": round((total - total0) / 1e3 / inserts, 1) if inserts else None,
        }
    http_count = delta("crucible_http_request_duration_microseconds_count", listener="ingest")
    http_sum = delta("crucible_http_request_duration_microseconds_sum", listener="ingest")
    if http_count:
        analysis["ingest_handler_mean_ms"] = round(http_sum / 1e3 / http_count, 2)
    samples = os.path.join(a.run, "samples.jsonl")
    if os.path.exists(samples):
        rows = [json.loads(line) for line in open(samples, encoding="utf-8") if line.strip()]
        rows = [r for r in rows if "error" not in r and times["loadgen_start"] <= r["t"] <= times["loadgen_end"]]
        if rows:
            lag = [r["crucible_wal_durable_lsn"] - r["crucible_wal_checkpoint_lsn"] for r in rows]
            third = max(1, len(rows) // 3)
            analysis["backlog"] = {
                "unshipped_mib_max": round(max(r["crucible_wal_unshipped_bytes"] for r in rows) / 2**20),
                "unshipped_mib_last_third_max": round(
                    max(r["crucible_wal_unshipped_bytes"] for r in rows[-third:]) / 2**20
                ),
                "lsn_lag_first_third_mean": round(statistics.mean(lag[:third])),
                "lsn_lag_last_third_mean": round(statistics.mean(lag[-third:])),
                "lsn_lag_max": max(lag),
            }
    dump(os.path.join(a.run, "analysis.json"), analysis)


def cmd_truncate(a):
    for table in TABLES:
        ch(f"TRUNCATE TABLE crucible.{table}")


def spread(values):
    values = sorted(values)
    return {"median": statistics.median(values), "min": values[0], "max": values[-1], "n": len(values)}


def read_run(root):
    run = load(os.path.join(root, "verdict.json"))
    for name in ("analysis", "meta"):
        path = os.path.join(root, name + ".json")
        if os.path.exists(path):
            run[name] = load(path)
    run["dir"] = os.path.relpath(root, OUT)
    return run


def ms(us):
    return f"{us / 1000:.1f}"


def burst_rows(lane_dir):
    """One row per search: the highest passing and the lowest failing rate, the passing step's details, and what
    else ran during any step of the search."""
    rows = []
    for search in sorted(os.listdir(lane_dir)):
        result = os.path.join(lane_dir, search, "result.txt")
        if not os.path.exists(result):
            continue
        lo, hi = (int(x) for x in open(result, encoding="utf-8").read().split())
        best = os.path.join(lane_dir, search, f"rate-{lo}")
        run = read_run(best) if os.path.exists(os.path.join(best, "verdict.json")) else None
        others, loads = set(), []
        for step in os.listdir(os.path.join(lane_dir, search)):
            path = os.path.join(lane_dir, search, step)
            if os.path.isdir(path):
                for tag, seen in context(path).items():
                    others.update(entry.split(" ")[0] for entry in seen["other_containers"])
                    if seen["load1"]:
                        loads.append(float(seen["load1"]))
        rows.append((search, lo, hi, run, sorted(others), max(loads, default=None)))
    return rows


def cmd_summarize(a):
    runs = []
    for root, _, files in os.walk(os.path.join(OUT, "runs")):
        if "verdict.json" in files:
            runs.append(read_run(root))
    runs.sort(key=lambda r: r["dir"])
    dump(os.path.join(OUT, "results.json"), runs)
    lines = ["## Burst", "",
             "| Lane | Search | Highest passing spans/s | Lowest failing | p50 / p99 / p99.9 ms | Crucible cores | "
             "Client cores | WAL MB/s | Frames per group | Sync mean ms | Syncs over 25 ms | Highest load (1 min) | "
             "Other containers during the search |",
             "|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    medians = []
    for lane in sorted(d for d in os.listdir(os.path.join(OUT, "runs")) if d.startswith("burst-")):
        rows = burst_rows(os.path.join(OUT, "runs", lane))
        for search, lo, hi, run, others, load1 in rows:
            seen = f"{len(others)}: {', '.join(others)}" if others else "none"
            if run is None:
                lines.append(f"| {lane} | {search} | {lo:,} | {hi:,} | | | | | | | | {load1} | {seen} |")
                continue
            an, lat = run.get("analysis", {}), run["latency_us"]
            wal, cpu = an.get("wal", {}), an.get("cpu", {})
            lines.append(
                f"| {lane} | {search} | {lo:,} | {hi:,} | {ms(lat['p50'])} / {ms(lat['p99'])} / "
                f"{ms(lat['p99_9'])} | {cpu.get('perf-crucible', {}).get('cores')} | "
                f"{cpu.get('perf-client', {}).get('cores')} | {wal.get('wal_mb_per_s')} | "
                f"{wal.get('frames_per_group')} | {wal.get('sync_mean_ms')} | {wal.get('syncs_over_25ms')} | "
                f"{load1} | {seen} |"
            )
        if rows:
            s = spread([row[1] for row in rows])
            medians.append(f"| {lane} | {s['median']:,} | {s['min']:,} to {s['max']:,} | {s['n']} |")
    lines += ["", "| Lane | Median of the highest passing rates | Range | Searches |", "|---|---|---|---|", *medians]
    lines += ["", "## At the gate rates", "",
              "| Lane | Run | Pass | Spans/s | p50 / p99 / p99.9 ms | Syncs over 25 ms | Crucible cores | Reasons |",
              "|---|---|---|---|---|---|---|---|"]
    for r in runs:
        if not r["dir"].startswith("runs/gate-") or "latency_us" not in r:
            continue
        lat, an = r["latency_us"], r.get("analysis", {})
        lines.append(
            f"| {r['dir'].split('/')[1]} | {r['dir'].split('/')[2]} | {r['pass']} | {r['target_spans_per_s']:,} | "
            f"{ms(lat['p50'])} / {ms(lat['p99'])} / {ms(lat['p99_9'])} | {an.get('wal', {}).get('syncs_over_25ms')} | "
            f"{an.get('cpu', {}).get('perf-crucible', {}).get('cores')} | {'; '.join(r['reasons'])} |"
        )
    lines += ["", "## Sustained", "",
              "| Run | Pass | Target spans/s | Acked spans/s | Ack p50 / p99 ms | Freshness p50 / p95 / p99 s | "
              "Last 30 s p95 s | Drained after s | Lost / duplicate | Crucible cores | ClickHouse cores | "
              "Shipper busy | Unshipped MiB max | Reasons |",
              "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for r in runs:
        if r.get("kind") != "sustained" or "freshness" not in r:
            continue
        f, an = r["freshness"], r.get("analysis", {})
        cpu = an.get("cpu", {})
        lines.append(
            f"| {r['dir']} | {r['pass']} | {r['target_spans_per_s']:,} | {r['acked_spans_per_s']:,.0f} | "
            f"{ms(r['latency_us']['p50'])} / {ms(r['latency_us']['p99'])} | "
            f"{float(f['p50_s']):.2f} / {float(f['p95_s']):.2f} / {float(f['p99_s']):.2f} | "
            f"{float(f['tail_p95_s']):.2f} | {r['drained_after_s']} | "
            f"{r['loss']['lost_spans']} / {r['loss']['duplicate_spans']} | "
            f"{cpu.get('perf-crucible', {}).get('cores')} | {cpu.get('perf-clickhouse', {}).get('cores')} | "
            f"{an.get('shipper_task_busy')} | {an.get('backlog', {}).get('unshipped_mib_max')} | "
            f"{'; '.join(r['reasons'])} |"
        )
    write(os.path.join(OUT, "results.md"), "\n".join(lines) + "\n")
    print("\n".join(lines))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("keys")
    p = sub.add_parser("scrape")
    p.add_argument("--out", required=True)
    p = sub.add_parser("sample")
    p.add_argument("--out", required=True)
    p.add_argument("--seconds", type=float, required=True)
    p.add_argument("--interval", type=float, default=1.0)
    for name in ("verdict-burst", "check-sustained", "analyze"):
        sub.add_parser(name).add_argument("--run", required=True)
    sub.add_parser("truncate")
    sub.add_parser("summarize")
    a = parser.parse_args()
    {
        "keys": cmd_keys,
        "scrape": cmd_scrape,
        "sample": cmd_sample,
        "verdict-burst": cmd_verdict_burst,
        "check-sustained": cmd_check_sustained,
        "analyze": cmd_analyze,
        "truncate": cmd_truncate,
        "summarize": cmd_summarize,
    }[a.command](a)


if __name__ == "__main__":
    sys.exit(main())
