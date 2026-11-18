#!/usr/bin/env bash
# Live verification of crucible-chfmt against real ClickHouse servers in Docker.
#
#   tests/clickhouse/verify.sh [--generate] [tag ...]      (tags default to: 26.3 26.8 latest)
#
# For each tag the script starts clickhouse/clickhouse-server:<tag> and
#   1. runs every tests/fixtures/*.sql and compares the Native output byte for byte with the committed .native file;
#   2. regenerates the decoder inputs in tests/fixtures/compressed and compares them with the committed files:
#      compress=1 responses, named after the method byte of their frames (26.3 and 26.8 answer with LZ4, 26.10 with
#      ZSTD), and `clickhouse compressor --zstd` output;
#   3. inserts the committed encoder snapshots (tests/snapshots/*.snap.lz4 and *.snap.zstd) with decompress=1 and the
#      per-insert settings of contract section 5.4 into tables of matching column types, and compares what SELECT
#      returns: each column of the all-types block against its fixture, the 262,144-row block by count, aggregates
#      and part count, and the two aggregating blocks against tests/clickhouse/sum_map.tsv before and after a merge;
#   4. inserts each SimpleAggregateFunction column again with a block header that names the underlying type instead
#      (the form crucible-schema's Ty::wire() renders) and compares the read-back with the fixture of the SAF type.
# With --generate, steps 1 and 2 write each file from the first tag that produces it and compare it on the later
# tags, and steps 3 and 4 are skipped.
#
# Requires docker and curl. The server listens on localhost:$CHFMT_PORT (default 18123). Exits non-zero on any
# difference; any HTTP error or ClickHouse exception aborts the run.
set -euo pipefail

cd "$(dirname "$0")/../.."

generate=0
if [ "${1:-}" = --generate ]; then
  generate=1
  shift
fi
tags=("$@")
if [ ${#tags[@]} -eq 0 ]; then
  tags=(26.3 26.8 latest)
fi

port=${CHFMT_PORT:-18123}
url=http://localhost:$port
container=chfmt-verify
work=$(mktemp -d)
mkdir "$work/written"
trap 'docker rm -f "$container" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

failures=0
ok() { printf 'ok    %s\n' "$*"; }
bad() {
  printf 'FAIL  %s\n' "$*"
  failures=$((failures + 1))
}

# post <output file> <curl arguments...>: one request; aborts on an HTTP error or a ClickHouse exception.
post() {
  local out=$1 status
  shift
  status=$(curl -sS -o "$out" -D "$work/headers" -w '%{http_code}' "$@")
  if [ "$status" != 200 ] || grep -qi '^x-clickhouse-exception-code' "$work/headers"; then
    printf 'ClickHouse request failed with HTTP %s:\n' "$status" >&2
    head -c 4000 "$out" >&2
    echo >&2
    exit 1
  fi
}

# sql <output file> <statement> [extra URL parameters]
sql() { post "$1" --data-binary "$2" "$url/?wait_end_of_query=1${3:+&$3}"; }

# sql_file <output file> <file holding the statement> [extra URL parameters]
sql_file() { post "$1" --data-binary @- "$url/?wait_end_of_query=1${3:+&$3}" < "$2"; }

# The settings every insert sends (contract section 5.4; insert_quorum applies to the clustered variant only).
settings='async_insert=0&insert_deduplicate=1&max_insert_threads=1&max_partitions_per_insert_block=1'
settings+='&throw_on_max_partitions_per_insert_block=1&input_format_native_allow_types_conversion=1'
settings+='&max_execution_time=60'

# insert <table> <column list> <block file> <deduplication token>
insert() {
  post "$work/insert" --url-query "query=INSERT INTO $1 ($2) FORMAT Native" \
    --url-query "insert_deduplication_token=$4" --data-binary @- \
    "$url/?decompress=1&wait_end_of_query=1&$settings" < "$3"
}

start() {
  docker run -d --rm --name "$container" --memory=1500m -e CLICKHOUSE_SKIP_USER_SETUP=1 -p "$port:8123" \
    "clickhouse/clickhouse-server:$1" >/dev/null
  for _ in $(seq 1 120); do
    if curl -sf "$url/ping" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "clickhouse-server:$1 did not become ready" >&2
  exit 1
}

# keep <generated file> <committed file> <label>: compare with the committed file; in generate mode, write it first
# unless an earlier tag of this run already did.
keep() {
  local mark
  mark="$work/written/$(printf '%s' "$2" | tr / _)"
  if [ $generate = 1 ] && [ ! -e "$mark" ]; then
    cp "$1" "$2"
    touch "$mark"
    ok "generated $3"
  elif cmp -s "$1" "$2"; then
    ok "$3"
  else
    bad "$3 differs from $2"
  fi
}

fixtures() {
  local file name
  for file in tests/fixtures/*.sql; do
    name=$(basename "$file" .sql)
    sql_file "$work/$name.native" "$file"
    keep "$work/$name.native" "tests/fixtures/$name.native" "fixture $name"
  done
}

# method <framed file>: the codec of its first frame, from the method byte after the 16-byte checksum.
method() {
  case $(od -An -tx1 -j16 -N1 "$1" | tr -d ' 
') in
    82) echo lz4 ;;
    90) echo zstd ;;
    *) echo unknown ;;
  esac
}

decoder_inputs() {
  local dir=tests/fixtures/compressed query name
  for query in tests/fixtures/uint64.sql "$dir/large.sql"; do
    sql_file "$work/response" "$query" compress=1
    name=response-$(basename "$query" .sql).$(method "$work/response")
    keep "$work/response" "$dir/$name" "decoder input $name"
  done
  sql_file "$work/large.native" "$dir/large.sql"
  docker exec -i "$container" clickhouse compressor --zstd < tests/fixtures/string.native > "$work/string.zstd"
  docker exec -i "$container" clickhouse compressor --zstd < "$work/large.native" > "$work/large.zstd"
  keep "$work/string.zstd" "$dir/compressor-string.zstd" "decoder input compressor-string.zstd"
  keep "$work/large.zstd" "$dir/compressor-large.zstd" "decoder input compressor-large.zstd"
}

all_types() {
  local columns ddl method table name expr fixture
  columns=$(cut -f1 tests/clickhouse/all_types.tsv | paste -sd, -)
  ddl=$(awk -F'\t' '{ printf "%s%s %s", (NR > 1 ? ", " : ""), $1, $2 }' tests/clickhouse/all_types.tsv)
  for method in lz4 zstd; do
    table=chfmt_all_types_$method
    sql "$work/out" "CREATE TABLE $table ($ddl) ENGINE = MergeTree ORDER BY (lc_string, ord)"
    insert "$table" "$columns" "tests/snapshots/golden__all_types_$method.snap.$method" "all_types_$method"
    sql "$work/count" "SELECT count() FROM $table"
    if [ "$(cat "$work/count")" = 8 ]; then
      ok "$method all_types block: 8 rows"
    else
      bad "$method all_types block: $(cat "$work/count") rows"
    fi
    while IFS=$'\t' read -r name _ expr fixture; do
      if [ "$expr" = - ]; then
        continue
      fi
      sql "$work/column" "SELECT $expr FROM $table ORDER BY ord FORMAT Native"
      if cmp -s "$work/column" "tests/fixtures/$fixture.native"; then
        ok "$method insert $name"
      else
        bad "$method insert $name: read-back differs from $fixture.native"
      fi
    done < tests/clickhouse/all_types.tsv
  done
}

max_rows() {
  local method table
  sql "$work/want" "SELECT count(), sum(toUInt32(number % 16)), sum(length(repeat('ab', number % 16))),
    sum(cityHash64(toUInt32(number % 16), repeat('ab', number % 16))) FROM numbers(262144) FORMAT TSV"
  for method in lz4 zstd; do
    table=chfmt_max_rows_$method
    sql "$work/out" "CREATE TABLE $table (n UInt32, s LowCardinality(String)) ENGINE = MergeTree ORDER BY n"
    insert "$table" "n, s" "tests/snapshots/golden__max_rows_$method.snap.$method" "max_rows_$method"
    sql "$work/got" "SELECT count(), sum(n), sum(length(s)), sum(cityHash64(n, CAST(s AS String))) FROM $table FORMAT TSV"
    sql "$work/parts" "SELECT count() FROM system.parts WHERE database = currentDatabase() AND table = '$table' AND active"
    if cmp -s "$work/got" "$work/want" && [ "$(cat "$work/parts")" = 1 ]; then
      ok "$method max_rows block: $(cut -f1 "$work/got") rows in one part, aggregates match"
    else
      bad "$method max_rows block: got $(cat "$work/got"), want $(cat "$work/want"), parts $(cat "$work/parts")"
    fi
  done
}

sum_map() {
  local key='project_id, minute, environment, marker_epoch, marker_batch_seq'
  local columns="$key, marker_at, count, total_cost_nusd, dur_hist, semconv_hist, capture_sources, last_seen"
  local ddl="project_id UUID, minute DateTime('UTC'), environment LowCardinality(String), marker_epoch UUID,"
  ddl+=" marker_batch_seq UInt64, marker_at SimpleAggregateFunction(max, DateTime('UTC')),"
  ddl+=" count SimpleAggregateFunction(sum, UInt64), total_cost_nusd SimpleAggregateFunction(sum, Int64),"
  ddl+=" dur_hist SimpleAggregateFunction(sumMap, Tuple(Array(UInt16), Array(UInt64))),"
  ddl+=" semconv_hist SimpleAggregateFunction(sumMap, Tuple(Array(UInt8), Array(UInt64))),"
  ddl+=" capture_sources SimpleAggregateFunction(groupUniqArrayArray(4), Array(UInt8)),"
  ddl+=" last_seen SimpleAggregateFunction(max, DateTime64(6, 'UTC'))"
  sql "$work/out" "CREATE TABLE chfmt_sum_map ($ddl) ENGINE = AggregatingMergeTree PARTITION BY toYYYYMM(minute) ORDER BY ($key)"
  insert chfmt_sum_map "$columns" tests/snapshots/golden__sum_map_1.snap.lz4 sum_map_1
  insert chfmt_sum_map "$columns" tests/snapshots/golden__sum_map_2.snap.zstd sum_map_2
  sql "$work/got" "SELECT $key, max(marker_at), sum(count), sum(total_cost_nusd), sumMap(dur_hist), sumMap(semconv_hist),
    arraySort(groupUniqArrayArray(capture_sources)), max(last_seen) FROM chfmt_sum_map GROUP BY $key ORDER BY $key FORMAT TSV"
  if cmp -s "$work/got" tests/clickhouse/sum_map.tsv; then
    ok "sum_map blocks aggregated at read time"
  else
    bad "sum_map blocks aggregated at read time:"
    cat "$work/got"
  fi
  sql "$work/out" "OPTIMIZE TABLE chfmt_sum_map FINAL"
  sql "$work/got" "SELECT $key, marker_at, count, total_cost_nusd, dur_hist, semconv_hist, arraySort(capture_sources),
    last_seen FROM chfmt_sum_map ORDER BY $key FORMAT TSV"
  if cmp -s "$work/got" tests/clickhouse/sum_map.tsv; then
    ok "sum_map blocks merged by OPTIMIZE FINAL"
  else
    bad "sum_map blocks merged by OPTIMIZE FINAL:"
    cat "$work/got"
  fi
}

saf_headers() {
  local name base ty
  while read -r name base; do
    ty=$(awk -F'	' -v name="$name" '$1 == name { print $2 }' tests/clickhouse/all_types.tsv)
    sql "$work/out" "CREATE TABLE chfmt_header_$name ($base $ty) ENGINE = MergeTree ORDER BY tuple()"
    post "$work/insert" --url-query "query=INSERT INTO chfmt_header_$name ($base) FORMAT Native" --data-binary @-       "$url/?wait_end_of_query=1&$settings" < "tests/fixtures/$base.native"
    sql "$work/column" "SELECT $base AS $name FROM chfmt_header_$name FORMAT Native"
    if cmp -s "$work/column" "tests/fixtures/$name.native"; then
      ok "underlying-type header into $name"
    else
      bad "underlying-type header into $name: read-back differs from $name.native"
    fi
  done <<'COLUMNS'
saf_max_datetime datetime
saf_min_datetime64 datetime64
saf_max_datetime64 datetime64
saf_max_uint8 uint8
saf_sum_uint64 uint64
saf_sum_int64 int64
saf_max_string string
saf_group_uniq_array_string_16 array_string
saf_group_uniq_array_string_8 array_string
saf_group_uniq_array_uint8_4 array_uint8
saf_sum_map_u16 tuple_u16_u64
saf_sum_map_u8 tuple_u8_u64
COLUMNS
}

for tag in "${tags[@]}"; do
  start "$tag"
  sql "$work/version" 'SELECT version()'
  echo "== clickhouse/clickhouse-server:$tag, version $(cat "$work/version")"
  fixtures
  decoder_inputs
  if [ $generate = 0 ]; then
    all_types
    max_rows
    sum_map
    saf_headers
  fi
  docker rm -f "$container" >/dev/null
done

echo "$failures failure(s)"
[ $failures -eq 0 ]
