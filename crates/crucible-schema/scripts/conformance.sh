#!/usr/bin/env bash
# Live conformance of crucible-schema against real ClickHouse servers in Docker.
#
#   scripts/conformance.sh single [tag ...]    single-node checks (default tags: 26.3 26.8 latest)
#   scripts/conformance.sh cluster [tag ...]   1 shard x 2 replicas with Keeper
#
# The statements applied are the committed golden snapshots under tests/snapshots (the rendered migration
# steps), so the servers see exactly what the renderer emits. The base schema creates the tables in their
# current shape, so before a later schema step is applied the column it adds is dropped: the step must then
# bring the table back to the shape the base schema creates, which is what SHOW CREATE TABLE records. The outputs land under
# tests/conformance/expected/<tag>/ and are committed; tests/conformance.rs compares them with the TableDefs
# and the routers, so CI needs no ClickHouse. Rerun this script when a snapshot changes.
#
# Per tag, single mode records:
#   version.txt             the server's version()
#   single_day.sql          SHOW CREATE TABLE of every table, day granularity
#   single_week.sql         the same at week granularity
#   partition_ids.tsv       _partition_id of sample rows (table, granularity, time in microseconds, id)
#   service_identities.tsv  rows each identity counts in each table after access step A1, under both values of
#                           users_without_row_policies_can_read_rows (a server-config switch, so two servers)
#   checks.tsv              further access checks (probe, constraints, maintenance privileges)
set -euo pipefail
export MSYS_NO_PATHCONV=1

cd "$(dirname "$0")/.."
SNAPSHOTS=tests/snapshots
EXPECTED=tests/conformance/expected
MEMORY=1500m
ADMIN=crucible_admin
ADMIN_PW=conformance-admin
PROJECT=0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b
IDENTITIES="crucible_admin crucible_ingest crucible_read_ui crucible_read_agent crucible_maint conformance_control"
# Sample instants in microseconds: the first week of the epoch (toMonday saturates there), a leap day, a
# Tuesday, both sides of a year boundary, and the last valid microsecond.
SAMPLES="0 86399999999 172800000000 259200000000 345600000000 951782400000000 1794907800000000 1798761599999999 1798761600000000 4102444799999999"

tables() {
  sed -n 's/^CREATE TABLE IF NOT EXISTS crucible\.\([a-z_]*\).*/\1/p' \
    "$SNAPSHOTS/golden__step__single__day__0001_schema_base.snap"
}

# The body of an insta snapshot: everything after the second `---` line.
snapshot() {
  awk 'seen >= 2 { print } /^---$/ { seen++ }' "$SNAPSHOTS/golden__$1.snap"
}

password() {
  case "$1" in
    crucible_admin) echo "$ADMIN_PW" ;;
    conformance_control) echo conformance-control ;;
    *) echo "conformance-${1#crucible_}" ;;
  esac
}

# client <container> <user> [clickhouse-client args]; SQL on stdin.
client() {
  local container=$1 user=$2
  shift 2
  docker exec -i "$container" clickhouse-client --user "$user" --password "$(password "$user")" "$@"
}

# The query's output, or `error <code>` when the server refuses it.
outcome() {
  local container=$1 user=$2 sql=$3 out
  if out=$(client "$container" "$user" -q "$sql" 2>&1); then
    echo "${out:-ok}"
  else
    echo "error $(echo "$out" | sed -n 's/.*Code: \([0-9]*\).*/\1/p' | head -1)"
  fi
}

# apply <container> <snapshot> [clickhouse-client args]
apply() {
  local container=$1 name=$2
  shift 2
  snapshot "$name" | client "$container" "$ADMIN" --multiquery "$@"
}

# later_steps <container> <variant> <on cluster clause> [clickhouse-client args]: the schema steps after the
# base schema, each from the shape before it, each twice (they must be idempotent).
later_steps() {
  local container=$1 variant=$2 on_cluster=$3
  shift 3
  client "$container" "$ADMIN" "$@" -q "ALTER TABLE crucible.span_core$on_cluster DROP COLUMN usage_aggregate"
  apply "$container" "step__${variant}__0004_span_usage_aggregate" "$@"
  apply "$container" "step__${variant}__0004_span_usage_aggregate" "$@"
}

# hourly_step <container> <variant> <on cluster clause> [clickhouse-client args]: step 0005 from the shape before
# it (no hourly table), twice. It needs the service identities of access step A1, so it runs after A1. The minute
# rollup holds one known row and one marker row by then, and the hourly table must hold the hour of the first,
# the marker row, and the records of the copy's start and end, after either apply.
hourly_step() {
  local container=$1 variant=$2 on_cluster=$3
  shift 3
  client "$container" "$ADMIN" "$@" -q "DROP TABLE crucible.mcp_tool_rollup_hourly$on_cluster SYNC"
  client "$container" "$ADMIN" "$@" --multiquery <<SQL
INSERT INTO crucible.mcp_tool_rollup (project_id, side, minute, marker_epoch, marker_batch_seq, count)
VALUES ('00000000-0000-0000-0000-000000000000', 'none', '2026-11-01 00:00:00', '0193a1b2-0000-7000-8000-000000000001', 7, 0);
SQL
  local twice
  for twice in 1 2; do
    apply "$container" "step__${variant}__0005_mcp_tool_rollup_hourly" "$@"
    local held
    held=$(client "$container" "$ADMIN" "$@" -q "SELECT count(), sum(count), countIf(marker_batch_seq = 7) FROM crucible.mcp_tool_rollup_hourly FORMAT TSV")
    [ "$held" = "$(printf '4	0	1')" ] || { echo "step 0005 left $held in the hourly table" >&2; return 1; }
  done
}

start() {
  local name=$1 tag=$2 config=$3
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker create --name "$name" --memory="$MEMORY" ${NETWORK:+--network "$NETWORK"} \
    -e CLICKHOUSE_USER="$ADMIN" -e CLICKHOUSE_PASSWORD="$ADMIN_PW" \
    -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 "clickhouse/clickhouse-server:$tag" >/dev/null
  docker cp "$config" "$name:/etc/clickhouse-server/config.d/conformance.xml"
  docker start "$name" >/dev/null
  for _ in $(seq 1 60); do
    if client "$name" "$ADMIN" -q "SELECT 1" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "server $name did not start" >&2
  docker logs "$name" 2>&1 | tail -20 >&2
  return 1
}

show_creates() {
  local container=$1 table
  for table in $(tables); do
    echo "-- table: $table"
    client "$container" "$ADMIN" -q "SHOW CREATE TABLE crucible.$table FORMAT TSVRaw"
  done
}

# Inserts the sample instants into one table per partition rule and prints each row's partition id.
partition_ids() {
  local container=$1 granularity=$2 values="" us
  for us in $SAMPLES; do values="$values${values:+, }$us"; done
  local rows="SELECT toUUID('$PROJECT') AS project_id, fromUnixTimestamp64Micro(arrayJoin([$values]), 'UTC') AS t"
  client "$container" "$ADMIN" --multiquery <<SQL
INSERT INTO crucible.span_core (project_id, start_time) SELECT project_id, t FROM ($rows);
INSERT INTO crucible.fact_tags (project_id, target_time) SELECT project_id, t FROM ($rows);
INSERT INTO crucible.trace_stats (project_id, row_day, min_start) SELECT project_id, toDate(t), t FROM ($rows);
INSERT INTO crucible.mcp_deploys (project_id, last_seen) SELECT project_id, t FROM ($rows);
SELECT 'span_core', '$granularity', toUnixTimestamp64Micro(start_time), _partition_id FROM crucible.span_core ORDER BY start_time;
SELECT 'fact_tags', '$granularity', toUnixTimestamp64Micro(target_time), _partition_id FROM crucible.fact_tags ORDER BY target_time;
SELECT 'trace_stats', '$granularity', toUnixTimestamp64Micro(min_start), _partition_id FROM crucible.trace_stats ORDER BY min_start;
SELECT 'mcp_deploys', '$granularity', toUnixTimestamp64Micro(last_seen), _partition_id FROM crucible.mcp_deploys ORDER BY last_seen;
TRUNCATE TABLE crucible.span_core;
TRUNCATE TABLE crucible.fact_tags;
TRUNCATE TABLE crucible.trace_stats;
TRUNCATE TABLE crucible.mcp_deploys;
SQL
}

# Access step A1 (twice: it must be idempotent), a control user with a grant but no row policy, one known row
# per table, then the identity-by-table matrix and the remaining access checks.
access_checks() {
  local container=$1 switch=$2 out=$3 table identity
  apply "$container" step__single__0002_access_service
  apply "$container" step__single__0002_access_service
  client "$container" "$ADMIN" --multiquery <<SQL
CREATE USER IF NOT EXISTS conformance_control IDENTIFIED BY 'conformance-control';
GRANT SELECT ON crucible.* TO conformance_control;
SQL
  for table in $(tables); do
    if [ "$table" = dedup_selftest ]; then
      echo "INSERT INTO crucible.$table (id) VALUES ('$PROJECT');"
    else
      echo "INSERT INTO crucible.$table (project_id) VALUES ('$PROJECT');"
    fi
  done | client "$container" "$ADMIN" --multiquery

  for identity in $IDENTITIES; do
    for table in $(tables); do
      printf '%s\t%s\t%s\t%s\n' "$switch" "$identity" "$table" \
        "$(outcome "$container" "$identity" "SELECT count() FROM crucible.$table")"
    done
  done >>"$out/service_identities.tsv"

  check() {
    printf '%s\t%s\t%s\n' "$switch" "$1" "$(outcome "$container" "$2" "$3")" >>"$out/checks.tsv"
  }
  check service_policies_present "$ADMIN" "$(snapshot service_policy_check)"
  check ingest_probe_plain crucible_ingest \
    "SELECT count() FROM crucible.span_core WHERE _partition_id = '19700101' AND wal_epoch = toUUID('00000000-0000-0000-0000-000000000000') AND batch_seq = 0"
  check ingest_probe_marker crucible_ingest \
    "SELECT count() FROM crucible.span_metrics WHERE _partition_id = '197001' AND project_id = toUUID('$PROJECT') AND marker_epoch = toUUID('00000000-0000-0000-0000-000000000000') AND marker_batch_seq = 0"
  check ingest_reads_no_other_column crucible_ingest "SELECT name FROM crucible.span_core"
  check ingest_execution_time_ceiling crucible_ingest "SELECT 1 SETTINGS max_execution_time = 61"
  check read_ui_within_ceiling crucible_read_ui "SELECT count() FROM crucible.span_core SETTINGS max_execution_time = 300, max_threads = 8"
  check read_ui_execution_time_ceiling crucible_read_ui "SELECT 1 SETTINGS max_execution_time = 301"
  check read_ui_unlimited_time_refused crucible_read_ui "SELECT 1 SETTINGS max_execution_time = 0"
  check read_ui_cannot_insert crucible_read_ui "INSERT INTO crucible.sql_probe (project_id) VALUES ('$PROJECT')"
  check read_agent_within_ceiling crucible_read_agent "SELECT count() FROM crucible.span_core SETTINGS max_execution_time = 5, max_rows_to_read = 100000000"
  check read_agent_rows_ceiling crucible_read_agent "SELECT 1 SETTINGS max_rows_to_read = 100000001"
  check read_agent_concurrency_is_const crucible_read_agent "SELECT 1 SETTINGS max_concurrent_queries_for_user = 100"
  check maint_reads_mutations crucible_maint "SELECT count() >= 0 FROM system.mutations"
  check maint_reads_engines crucible_maint "SELECT engine FROM system.tables WHERE database = 'crucible' AND name = 'dedup_selftest'"
  check maint_reads_server_settings crucible_maint "SELECT count() FROM system.server_settings WHERE name = 'distributed_ddl_use_initial_user_and_roles'"
  check maint_lightweight_delete crucible_maint \
    "DELETE FROM crucible.span_core IN PARTITION '19700101' WHERE project_id = toUUID('$PROJECT') SETTINGS lightweight_deletes_sync = 1"
  check maint_rows_after_delete crucible_maint "SELECT count() FROM crucible.span_core"
  check maint_apply_deleted_mask crucible_maint "ALTER TABLE crucible.span_core APPLY DELETED MASK IN PARTITION '19700101'"
  check maint_drop_partition crucible_maint "ALTER TABLE crucible.trace_heads DROP PARTITION '19700101'"
  check maint_rows_after_drop crucible_maint "SELECT count() FROM crucible.trace_heads"
  check maint_cannot_create crucible_maint "CREATE TABLE crucible.conformance_x (a UInt8) ENGINE = Memory"
}

switch_config() {
  cat >"$1" <<XML
<clickhouse>
  <access_control_improvements>
    <users_without_row_policies_can_read_rows>$2</users_without_row_policies_can_read_rows>
  </access_control_improvements>
</clickhouse>
XML
}

single() {
  local tag=$1 out="$EXPECTED/$1" name="crucible-conf-$1" config=tests/conformance/.server-config.xml
  rm -f "$out"/version.txt "$out"/single_*.sql "$out"/partition_ids.tsv "$out"/service_identities.tsv "$out"/checks.tsv
  mkdir -p "$out"

  # Server 1: the permissive value of the switch. Schema checks run here.
  switch_config "$config" true
  start "$name" "$tag" "$config"
  client "$name" "$ADMIN" -q "SELECT version()" >"$out/version.txt"
  # The Cloud rendering must at least parse and apply on an open-source server.
  apply "$name" step__cloud__day__0001_schema_base
  later_steps "$name" cloud ""
  client "$name" "$ADMIN" -q "DROP DATABASE crucible SYNC"
  apply "$name" step__single__week__0001_schema_base
  later_steps "$name" single ""
  show_creates "$name" >"$out/single_week.sql"
  partition_ids "$name" week >"$out/partition_ids.tsv"
  client "$name" "$ADMIN" -q "DROP DATABASE crucible SYNC"
  apply "$name" step__single__day__0001_schema_base
  apply "$name" step__single__day__0001_schema_base
  later_steps "$name" single ""
  show_creates "$name" >"$out/single_day.sql"
  partition_ids "$name" day >>"$out/partition_ids.tsv"
  access_checks "$name" permissive "$out"
  hourly_step "$name" single ""
  docker rm -f "$name" >/dev/null

  # Server 2: the strict value.
  switch_config "$config" false
  start "$name" "$tag" "$config"
  apply "$name" step__single__day__0001_schema_base
  apply "$name" step__single__0004_span_usage_aggregate
  access_checks "$name" strict "$out"
  docker rm -f "$name" >/dev/null
  echo "single $tag: $(cat "$out/version.txt")"
}

cluster_config() {
  cat >"$1" <<XML
<clickhouse>
  <listen_host>0.0.0.0</listen_host>
  <distributed_ddl_use_initial_user_and_roles>1</distributed_ddl_use_initial_user_and_roles>
  <macros><shard>01</shard><replica>$2</replica></macros>
  <remote_servers replace="replace">
    <crucible>
      <shard>
        <internal_replication>true</internal_replication>
        <replica><host>crucible-conf-r1</host><port>9000</port></replica>
        <replica><host>crucible-conf-r2</host><port>9000</port></replica>
      </shard>
    </crucible>
  </remote_servers>
  <zookeeper><node><host>crucible-conf-r1</host><port>9181</port></node></zookeeper>
  <keeper_server>
    <tcp_port>9181</tcp_port>
    <server_id>1</server_id>
    <log_storage_path>/var/lib/clickhouse/coordination/log</log_storage_path>
    <snapshot_storage_path>/var/lib/clickhouse/coordination/snapshots</snapshot_storage_path>
    <raft_configuration>
      <server><id>1</id><hostname>crucible-conf-r1</hostname><port>9234</port></server>
    </raft_configuration>
  </keeper_server>
  <access_control_improvements>
    <users_without_row_policies_can_read_rows>false</users_without_row_policies_can_read_rows>
  </access_control_improvements>
</clickhouse>
XML
}

# One shard, two replicas; Keeper is embedded in replica 1. The clustered rendering is applied on replica 1 and
# everything is read back on replica 2. Recorded under the strict value of the row-policy switch.
#
# ON CLUSTER statements that carry a text index with a preprocessor are refused unless every host sets the server
# setting distributed_ddl_use_initial_user_and_roles and the statement is sent with
# distributed_ddl_entry_format_version >= 8, so the cluster config and the client settings below set both.
cluster() {
  local tag=$1 out="$EXPECTED/$1" config=tests/conformance/.server-config.xml table identity
  mkdir -p "$out"
  NETWORK=crucible-conf
  docker network create "$NETWORK" >/dev/null 2>&1 || true
  cluster_config "$config" r1
  start crucible-conf-r1 "$tag" "$config"
  # Replica 2 runs no Keeper of its own.
  cluster_config "$config" r2
  sed -i '/<keeper_server>/,/<\/keeper_server>/d' "$config"
  start crucible-conf-r2 "$tag" "$config"

  # The settings of Variant::ddl_settings for the clustered variant (wait_end_of_query is HTTP-only).
  local settings="--alter_sync 2 --distributed_ddl_entry_format_version 8"
  # shellcheck disable=SC2086
  {
    apply crucible-conf-r1 step__cluster__day__0001_schema_base $settings >/dev/null
    apply crucible-conf-r1 step__cluster__day__0001_schema_base $settings >/dev/null
    later_steps crucible-conf-r1 cluster " ON CLUSTER crucible" $settings >/dev/null
    apply crucible-conf-r1 step__cluster__0002_access_service $settings >/dev/null
    apply crucible-conf-r1 step__cluster__0002_access_service $settings >/dev/null
  }
  show_creates crucible-conf-r2 >"$out/cluster_day.sql"
  for table in $(tables); do
    if [ "$table" = dedup_selftest ]; then
      echo "INSERT INTO crucible.$table (id) VALUES ('$PROJECT');"
    else
      echo "INSERT INTO crucible.$table (project_id) VALUES ('$PROJECT');"
    fi
  done | client crucible-conf-r1 "$ADMIN" --multiquery
  : >"$out/cluster_identities.tsv"
  for table in $(tables); do
    client crucible-conf-r2 "$ADMIN" -q "SYSTEM SYNC REPLICA crucible.$table"
  done
  for identity in ${IDENTITIES% conformance_control}; do
    for table in $(tables); do
      printf '%s\t%s\t%s\n' "$identity" "$table" \
        "$(outcome crucible-conf-r2 "$identity" "SELECT count() FROM crucible.$table")"
    done
  done >>"$out/cluster_identities.tsv"
  # shellcheck disable=SC2086
  hourly_step crucible-conf-r1 cluster " ON CLUSTER crucible" $settings >/dev/null
  echo "cluster $tag: done"
}

cleanup() {
  docker rm -f crucible-conf-r1 crucible-conf-r2 >/dev/null 2>&1 || true
  docker network rm crucible-conf >/dev/null 2>&1 || true
  rm -f tests/conformance/.server-config.xml
}
trap cleanup EXIT

mode=${1:-single}
shift || true
[ $# -gt 0 ] || set -- 26.3 26.8 latest
for tag in "$@"; do
  "$mode" "$tag"
done
