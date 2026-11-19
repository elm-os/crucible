#!/usr/bin/env bash
# The throughput baseline of `crucible serve` on one machine (docs/testing/perf-baseline.md).
#
# Starts ClickHouse, Crucible and a client container on disjoint cpusets, runs the fsync-honesty probe on the WAL
# volume, and searches for the highest burst rate (ack path) and the highest sustained rate (end to end) of each
# lane. Every run keeps its load generator report, CPU and metric snapshots, `docker stats`, the load average and
# its verdict under $OUT/runs/.
#
# Usage, from the repository root:
#   tools/perf/baseline.sh all                         up, probe, every burst lane, every sustained lane, down
#   tools/perf/baseline.sh up                          start ClickHouse and the client; copy the binaries
#   tools/perf/baseline.sh probe                       the fsync-honesty probe on the WAL volume
#   tools/perf/baseline.sh burst <1k|10k> <plain|gzip> <start spans/s> [searches]
#   tools/perf/baseline.sh gate <1k|10k> <plain|gzip> <spans/s> [runs]
#   tools/perf/baseline.sh sustained <1k|10k> <start spans/s> [confirmations]
#   tools/perf/baseline.sh summarize                   results.json and results.md from every run
#   tools/perf/baseline.sh down                        stop and remove what this script started
#
# Inputs: release builds of `crucible` and `crucible-loadgen` in the cargo target volume $TARGET_VOLUME, each
# built on its own (`cargo build --locked -p crucible --release`, then `-p crucible-loadgen`), and the images
# $IMAGE and $CH_IMAGE.
set -euo pipefail
# Git Bash on Windows: pass container paths through unchanged.
export MSYS_NO_PATHCONV=1

host_path() { (cd "$1" && (pwd -W 2>/dev/null || pwd)); }

ROOT=$(host_path "$(dirname "${BASH_SOURCE[0]}")/../..")
OUT=${OUT:-$ROOT/perf/results/local/baseline}
mkdir -p "$OUT/rig" "$OUT/runs" "$OUT/logs"
OUT=$(host_path "$OUT")

IMAGE=${IMAGE:-crucible-dev:1}
CH_IMAGE=${CH_IMAGE:-clickhouse/clickhouse-server:26.8}
TARGET_VOLUME=${TARGET_VOLUME:-crucible-target}
# Disjoint cpusets on the 24-CPU VM, sized to leave room for other jobs in its 16 GB: Crucible 6 CPUs and 2 GB,
# ClickHouse 8 CPUs and 6 GB, the client 4 CPUs and 1 GB.
CRUCIBLE_CPUS=${CRUCIBLE_CPUS:-0-5}
CRUCIBLE_MEM=${CRUCIBLE_MEM:-2g}
CH_CPUS=${CH_CPUS:-6-13}
CH_MEM=${CH_MEM:-6g}
CLIENT_CPUS=${CLIENT_CPUS:-14-17}
CLIENT_MEM=${CLIENT_MEM:-1g}
# The recent-span index defaults to 10% of the container's memory limit (at most 1 GiB), about 205 MiB in 2 GB. Set
# RECENT_MAX_BYTES to bound it by hand instead.
RECENT_MAX_BYTES=${RECENT_MAX_BYTES:-}
SPANS_PER_REQUEST=${SPANS_PER_REQUEST:-100}
# The connection cap is at least CONCURRENCY and holds 100 ms of arrivals, so that it binds only after a stall
# far longer than the 20 ms latency limit: below that the generator stays open loop.
CONCURRENCY=${CONCURRENCY:-128}
BURST_SECS=${BURST_SECS:-15}
WARMUP_SECS=${WARMUP_SECS:-3}
SEARCH_SECS=${SEARCH_SECS:-45}
SUSTAIN_SECS=${SUSTAIN_SECS:-120}
NET=crucible-perf
BIN=crucible-perf-bin
WAL=crucible-perf-wal
CHDATA=crucible-perf-ch
VERDICT=

log() { echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$OUT/log.txt" >&2; }
py() { docker exec perf-client python3 /tools/baseline.py "$@"; }
rel() { echo "${1#"$OUT"/}"; }

cpus() { local a=${1%-*} b=${1#*-}; echo $(( b - a + 1 )); }

start_client() {
  docker run -d --name perf-client --network "$NET" --cpuset-cpus "$CLIENT_CPUS" --cpus "$(cpus "$CLIENT_CPUS")" \
    --memory "$CLIENT_MEM" -v "$BIN:/opt/crucible:ro" -v "$ROOT/tools/perf:/tools:ro" -v "$OUT:/out" "$IMAGE" \
    sleep infinity >/dev/null
}

start_clickhouse() {
  docker run -d --name perf-clickhouse --network "$NET" --cpuset-cpus "$CH_CPUS" --cpus "$(cpus "$CH_CPUS")" \
    --memory "$CH_MEM" --ulimit nofile=262144:262144 -v "$CHDATA:/var/lib/clickhouse" \
    -e CLICKHOUSE_USER=crucible_admin -e CLICKHOUSE_PASSWORD="$(tr -d '\n' < "$OUT/rig/clickhouse-password")" \
    -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 "$CH_IMAGE" >/dev/null
  for _ in $(seq 1 60); do
    if docker exec perf-client curl -sf http://perf-clickhouse:8123/ping >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  log "ClickHouse did not start"; return 1
}

save_crucible_log() {
  if docker inspect perf-crucible >/dev/null 2>&1; then
    docker logs perf-crucible > "$OUT/logs/crucible-$(date -u +%Y%m%dT%H%M%S).log" 2>&1 || true
    docker rm -f perf-crucible >/dev/null
  fi
}

# Starts Crucible on a fresh WAL volume: `ingest` without a store (the ack path alone), `store` with ClickHouse.
start_crucible() {
  save_crucible_log
  docker volume rm -f "$WAL" >/dev/null 2>&1 || true
  docker volume create "$WAL" >/dev/null
  local store=()
  if [ "$1" = store ]; then
    store=(--clickhouse-url http://perf-clickhouse:8123 --clickhouse-password-file /etc/crucible/clickhouse-password
      --master-key-file /etc/crucible/master-key --migrate)
  fi
  if [ -n "$RECENT_MAX_BYTES" ]; then
    store+=(--recent-max-bytes "$RECENT_MAX_BYTES")
  fi
  docker run -d --name perf-crucible --network "$NET" --cpuset-cpus "$CRUCIBLE_CPUS" \
    --cpus "$(cpus "$CRUCIBLE_CPUS")" --memory "$CRUCIBLE_MEM" \
    -v "$BIN:/opt/crucible:ro" -v "$WAL:/data" -v "$OUT/rig:/etc/crucible:ro" "$IMAGE" \
    /opt/crucible/crucible serve --data-dir /data --keys-file /etc/crucible/keys.toml \
    --metrics-listen 0.0.0.0:9464 ${store[@]+"${store[@]}"} >/dev/null
  for _ in $(seq 1 120); do
    if docker exec perf-client curl -sf http://perf-crucible:8080/readyz >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  docker logs perf-crucible 2>&1 | tail -20 >&2
  log "Crucible was not ready"; return 1
}

up() {
  docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null
  docker volume create "$BIN" >/dev/null
  # Copy the binaries, so that a rebuild in the target volume cannot change them during a run.
  docker run --rm -v "$TARGET_VOLUME:/target:ro" -v "$BIN:/bin-out" "$IMAGE" sh -c \
    'cp /target/release/crucible /target/release/crucible-loadgen /bin-out/ && cd /bin-out && sha256sum crucible crucible-loadgen && ./crucible --version' \
    > "$OUT/rig/binaries.txt"
  docker inspect perf-client >/dev/null 2>&1 || start_client
  [ -f "$OUT/rig/keys.toml" ] || py keys
  docker inspect perf-clickhouse >/dev/null 2>&1 || start_clickhouse
  {
    echo "date: $(date -u +%FT%TZ)"
    docker info --format 'docker: {{.ServerVersion}}, {{.NCPU}} CPUs, {{.MemTotal}} bytes, kernel {{.KernelVersion}}, {{.OperatingSystem}}'
    echo "clickhouse: $(docker exec perf-clickhouse clickhouse-server --version)"
    echo "image: $IMAGE $(docker image inspect --format '{{.Id}}' "$IMAGE")"
    echo "crucible: cpuset $CRUCIBLE_CPUS, memory $CRUCIBLE_MEM, recent index bound ${RECENT_MAX_BYTES:-the default}"
    echo "clickhouse: cpuset $CH_CPUS, memory $CH_MEM"
    echo "client: cpuset $CLIENT_CPUS, memory $CLIENT_MEM"
  } > "$OUT/rig/versions.txt"
  log "rig up: $(tr '\n' ' ' < "$OUT/rig/binaries.txt")"
}

down() {
  save_crucible_log
  docker rm -f perf-clickhouse perf-client >/dev/null 2>&1 || true
  docker volume rm -f "$WAL" "$CHDATA" "$BIN" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  log "rig down"
}

probe() {
  docker volume rm -f "$WAL" >/dev/null 2>&1 || true
  docker volume create "$WAL" >/dev/null
  docker run --rm --cpuset-cpus "$CRUCIBLE_CPUS" --cpus "$(cpus "$CRUCIBLE_CPUS")" --memory "$CRUCIBLE_MEM" \
    -v "$BIN:/opt/crucible:ro" -v "$WAL:/probe" \
    "$IMAGE" /opt/crucible/crucible-loadgen fsync-probe --dir /probe > "$OUT/probe.json"
  log "probe: $(grep -m1 '"verdict"' "$OUT/probe.json")"
}

# The context of a run: other containers, the load average, cgroup CPU counters of the three containers with each
# container's clock, the Crucible threads, and Crucible's metrics. `docker stats` is slow, so it goes first before
# a run and last after it.
snapshot() {
  local dir=$1 tag=$2
  if [ "$tag" = before ]; then
    docker stats --no-stream --format '{{json .}}' > "$dir/$tag-docker-stats.jsonl" 2>&1 || true
    docker exec perf-client cat /proc/loadavg > "$dir/$tag-loadavg.txt"
  fi
  : > "$dir/$tag-cgroup.txt"
  for c in perf-crucible perf-clickhouse perf-client; do
    { echo "== $c"; docker exec "$c" sh -c 'echo time_ns $(date +%s%N); cat /sys/fs/cgroup/cpu.stat'; } \
      >> "$dir/$tag-cgroup.txt"
  done
  docker exec perf-crucible sh -c 'date +%s%N; cat /proc/1/task/*/stat' > "$dir/$tag-threads.txt"
  py scrape --out "/out/$(rel "$dir")/$tag.prom"
  if [ "$tag" = after ]; then
    docker exec perf-client cat /proc/loadavg > "$dir/$tag-loadavg.txt"
    docker stats --no-stream --format '{{json .}}' > "$dir/$tag-docker-stats.jsonl" 2>&1 || true
  fi
}

# loadgen <dir> <name> <requests/s> <seconds> <1k|10k> <plain|gzip>
loadgen() {
  local dir; dir=$(rel "$1")
  local gzip="" connections=$(( $3 / 10 > CONCURRENCY ? $3 / 10 : CONCURRENCY ))
  [ "$6" = gzip ] && gzip=--gzip
  docker exec -e "SECRET=$(tr -d '\n' < "$OUT/rig/secret")" perf-client sh -c "
    date +%s%N > /out/$dir/$2-start.ns
    /opt/crucible/crucible-loadgen run --target perf-crucible:4318 --rate $3 --duration-secs $4 --span-size $5 \
      --spans-per-request $SPANS_PER_REQUEST --concurrency $connections $gzip \
      --header \"Authorization: Bearer \$SECRET\" --out /out/$dir/$2.json
    rc=\$?
    date +%s%N > /out/$dir/$2-end.ns
    exit \$rc"
}

meta() {  # dir key=value...
  local dir=$1; shift
  local body=""
  for kv in "$@"; do body+="\"${kv%%=*}\": \"${kv#*=}\", "; done
  echo "{${body%, }}" > "$dir/meta.json"
}

# One burst step: a fresh Crucible without a store, a short warm-up at half the rate, then the measured run.
burst_step() {  # size encoding spans/s search-dir
  local rps=$(( $3 / SPANS_PER_REQUEST )) dir="$4/rate-$3"
  mkdir -p "$dir"
  start_crucible ingest
  loadgen "$dir" warmup $(( rps / 2 > 0 ? rps / 2 : 1 )) "$WARMUP_SECS" "$1" "$2" || true
  snapshot "$dir" before
  loadgen "$dir" loadgen "$rps" "$BURST_SECS" "$1" "$2" || true
  snapshot "$dir" after
  meta "$dir" mode=ingest-only phase=burst
  py analyze --run "/out/$(rel "$dir")"
  VERDICT=$(py verdict-burst --run "/out/$(rel "$dir")")
  log "burst $1 $2 $3 spans/s: $VERDICT"
}

# Ramps up by 5/4 (or down by 4/5) from the start rate until the verdict flips, then bisects twice.
search() {  # step-function size encoding start dir
  local step=$1 size=$2 enc=$3 rate=$4 dir=$5 lo=0 hi=0
  while [ "$lo" -eq 0 ] || [ "$hi" -eq 0 ]; do
    "$step" "$size" "$enc" "$rate" "$dir"
    if [ "$VERDICT" = PASS ]; then lo=$rate; rate=$(( rate * 5 / 4 )); else hi=$rate; rate=$(( rate * 4 / 5 )); fi
    rate=$(( rate / SPANS_PER_REQUEST * SPANS_PER_REQUEST ))
    [ "$rate" -ge "$SPANS_PER_REQUEST" ] || break
  done
  for _ in 1 2; do
    local mid=$(( (lo + hi) / 2 / SPANS_PER_REQUEST * SPANS_PER_REQUEST ))
    [ "$mid" -gt "$lo" ] && [ "$mid" -lt "$hi" ] || break
    "$step" "$size" "$enc" "$mid" "$dir"
    if [ "$VERDICT" = PASS ]; then lo=$mid; else hi=$mid; fi
  done
  echo "$lo $hi" > "$dir/result.txt"
  log "search $dir: highest passing $lo spans/s, lowest failing $hi spans/s"
  SEARCH_LO=$lo
}

# The request shape, in result directory names when it is not the default of 100 spans per request.
shape() { [ "$SPANS_PER_REQUEST" -eq 100 ] || echo "-spr$SPANS_PER_REQUEST"; }

burst() {  # size encoding start [searches]
  local start=$3
  for n in $(seq 1 "${4:-3}"); do
    search burst_step "$1" "$2" "$start" "$OUT/runs/burst-$1-$2$(shape)/search-$n"
  done
}

# Burst steps at exactly a gate rate (250,000 spans/s for 1 KiB, 50,000 for 10 KiB), so that the report answers
# the gate directly; the search verdicts near the limit are not monotone.
gate() {  # size encoding spans/s [runs]
  for n in $(seq 1 "${4:-3}"); do
    burst_step "$1" "$2" "$3" "$OUT/runs/gate-$1-$2$(shape)/run-$n"
  done
}

# One sustained run against the store: the gauges sampled every second, then the drain, zero-loss and
# freshness checks, then the tables emptied for the next run.
sustained_step() {  # size encoding spans/s dir [seconds]
  local rps=$(( $3 / SPANS_PER_REQUEST )) secs=${5:-$SEARCH_SECS}
  local dir="$4/rate-$3"
  [ -d "$dir" ] && dir="$dir-$(date -u +%H%M%S)"
  mkdir -p "$dir"
  snapshot "$dir" before
  docker exec -d perf-client python3 /tools/baseline.py sample --out "/out/$(rel "$dir")/samples.jsonl" \
    --seconds $(( secs + 60 ))
  loadgen "$dir" loadgen "$rps" "$secs" "$1" "$2" || true
  snapshot "$dir" after
  meta "$dir" mode=store phase=sustained seconds="$secs"
  VERDICT=$(py check-sustained --run "/out/$(rel "$dir")")
  py analyze --run "/out/$(rel "$dir")"
  if [ -f "$dir/drain-timed-out" ]; then
    # The shipper is still behind; a fresh Crucible keeps the backlog out of the next run.
    start_crucible store
  fi
  py truncate
  sleep 10
  log "sustained $1 $3 spans/s for $secs s: $VERDICT"
}

sustained_search_step() { sustained_step "$1" "$2" "$3" "$4" "$SEARCH_SECS"; }

sustained() {  # size start [confirmations]
  local size=$1 lane="$OUT/runs/sustained-$1"
  start_crucible store
  py truncate
  search sustained_search_step "$size" plain "$2" "$lane/search"
  local rate=$SEARCH_LO
  for attempt in 1 2 3; do
    local passed=0
    for n in $(seq 1 "${3:-3}"); do
      sustained_step "$size" plain "$rate" "$lane/confirm-$attempt" "$SUSTAIN_SECS"
      [ "$VERDICT" = PASS ] && passed=$(( passed + 1 ))
    done
    if [ "$passed" -eq "${3:-3}" ]; then
      log "sustained $size: confirmed $rate spans/s"
      return 0
    fi
    rate=$(( rate * 9 / 10 / SPANS_PER_REQUEST * SPANS_PER_REQUEST ))
  done
  log "sustained $size: no rate confirmed after three attempts"
}

all() {
  up
  probe
  for lane in "1k plain 400000" "1k gzip 400000" "10k plain 40000" "10k gzip 40000"; do
    # shellcheck disable=SC2086
    burst $lane
  done
  for lane in "1k plain 250000" "1k gzip 250000" "10k plain 50000"; do
    # shellcheck disable=SC2086
    gate $lane
  done
  # With 100 spans of 10 KiB (1 MiB bodies) the generator's own gzip stalls its sends; 10 spans per request keep
  # it accurate at this rate.
  SPANS_PER_REQUEST=10 gate 10k plain 50000
  SPANS_PER_REQUEST=10 gate 10k gzip 50000
  sustained 1k 60000
  sustained 10k 8000
  py summarize
  down
}

command=${1:-}
[ -n "$command" ] && shift
case "$command" in
  all) all ;;
  up) up ;;
  probe) probe ;;
  burst) burst "$@" ;;
  gate) gate "$@" ;;
  sustained) sustained "$@" ;;
  sustained-step) sustained_step "$1" "$2" "$3" "$OUT/runs/manual" "${4:-$SUSTAIN_SECS}" ;;
  burst-step) burst_step "$1" "$2" "$3" "$OUT/runs/manual" ;;
  start-crucible) start_crucible "$1" ;;
  summarize) py summarize ;;
  down) down ;;
  *) sed -n '2,21p' "$0"; exit 2 ;;
esac
