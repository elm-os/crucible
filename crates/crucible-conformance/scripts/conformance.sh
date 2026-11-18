#!/usr/bin/env bash
# Runs the conformance suite of crucible-conformance against real ClickHouse servers in Docker.
# The suite includes the differential check of the simulation's store model (the "ChModel differential" rows).
#
#   scripts/conformance.sh single [tag ...]    one server per tag (default tags: 26.3 26.8 latest)
#   scripts/conformance.sh cluster [tag ...]   1 shard x 2 replicas with Keeper, per tag
#
# The servers run one after another, each limited to 1500 MiB, and the suite runs in the dev container on the
# same Docker network. Every server hides rows from users without a row policy (the strict value of
# users_without_row_policies_can_read_rows), so the service-identity check is meaningful.
#
# Environment: CRUCIBLE_DEV_IMAGE (default crucible-dev:1), CRUCIBLE_TARGET_VOLUME (default crucible-target).
set -euo pipefail
export MSYS_NO_PATHCONV=1

cd "$(dirname "$0")/../../.."
# Docker needs the Windows form of the path under Git Bash.
ROOT=$(pwd -W 2>/dev/null || pwd)
IMAGE=${CRUCIBLE_DEV_IMAGE:-crucible-dev:1}
TARGET_VOLUME=${CRUCIBLE_TARGET_VOLUME:-crucible-target}
NETWORK=crucible-conformance
MEMORY=1500m
ADMIN=crucible_admin
ADMIN_PW=conformance-admin
# Docker cannot copy from the POSIX temp directory under Git Bash, so the server config lives beside the script.
CONFIG=crates/crucible-conformance/scripts/.server-config.xml

cargo_in_container() {
  docker run --rm --cpus=3 --memory=2g --network "$NETWORK" -e CARGO_BUILD_JOBS=3 \
    -v "$ROOT:/work" -w /work -v "$TARGET_VOLUME:/target" -v crucible-cargo:/usr/local/cargo/registry \
    "$IMAGE" cargo "$@"
}

suite() {
  cargo_in_container run --quiet --locked -p crucible-conformance -- --user "$ADMIN" --password "$ADMIN_PW" "$@"
}

start() {
  local name=$1 tag=$2
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker create --name "$name" --network "$NETWORK" --memory="$MEMORY" \
    -e CLICKHOUSE_USER="$ADMIN" -e CLICKHOUSE_PASSWORD="$ADMIN_PW" \
    -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 "clickhouse/clickhouse-server:$tag" >/dev/null
  docker cp "$CONFIG" "$name:/etc/clickhouse-server/config.d/conformance.xml"
  docker start "$name" >/dev/null
  for _ in $(seq 1 60); do
    if docker exec "$name" clickhouse-client --user "$ADMIN" --password "$ADMIN_PW" -q "SELECT 1" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "server $name did not start" >&2
  docker logs "$name" 2>&1 | tail -20 >&2
  return 1
}

single_config() {
  cat >"$CONFIG" <<XML
<clickhouse>
  <access_control_improvements>
    <users_without_row_policies_can_read_rows>false</users_without_row_policies_can_read_rows>
  </access_control_improvements>
</clickhouse>
XML
  chmod 644 "$CONFIG"
}

cluster_config() {
  cat >"$CONFIG" <<XML
<clickhouse>
  <listen_host>0.0.0.0</listen_host>
  <distributed_ddl_use_initial_user_and_roles>1</distributed_ddl_use_initial_user_and_roles>
  <macros><shard>01</shard><replica>$1</replica></macros>
  <remote_servers replace="replace">
    <crucible>
      <shard>
        <internal_replication>true</internal_replication>
        <replica><host>crucible-conformance-r1</host><port>9000</port></replica>
        <replica><host>crucible-conformance-r2</host><port>9000</port></replica>
      </shard>
    </crucible>
  </remote_servers>
  <zookeeper><node><host>crucible-conformance-r1</host><port>9181</port></node></zookeeper>
  <access_control_improvements>
    <users_without_row_policies_can_read_rows>false</users_without_row_policies_can_read_rows>
  </access_control_improvements>
XML
  if [ "$1" = r1 ]; then
    # Keeper is embedded in replica 1.
    cat >>"$CONFIG" <<XML
  <keeper_server>
    <tcp_port>9181</tcp_port>
    <server_id>1</server_id>
    <log_storage_path>/var/lib/clickhouse/coordination/log</log_storage_path>
    <snapshot_storage_path>/var/lib/clickhouse/coordination/snapshots</snapshot_storage_path>
    <raft_configuration>
      <server><id>1</id><hostname>crucible-conformance-r1</hostname><port>9234</port></server>
    </raft_configuration>
  </keeper_server>
XML
  fi
  echo "</clickhouse>" >>"$CONFIG"
  chmod 644 "$CONFIG"
}

single() {
  local tag=$1 name=crucible-conformance-single
  single_config
  start "$name" "$tag"
  suite --url "http://$name:8123" --label "$tag" || failed=1
  docker rm -f "$name" >/dev/null
}

cluster() {
  local tag=$1
  cluster_config r1
  start crucible-conformance-r1 "$tag"
  cluster_config r2
  start crucible-conformance-r2 "$tag"
  suite --url "http://crucible-conformance-r1:8123" --cluster crucible --label "$tag cluster" || failed=1
  docker rm -f crucible-conformance-r1 crucible-conformance-r2 >/dev/null
}

cleanup() {
  docker rm -f crucible-conformance-single crucible-conformance-r1 crucible-conformance-r2 >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  rm -f "$CONFIG"
}
trap cleanup EXIT

mode=${1:-single}
shift || true
[ $# -gt 0 ] || set -- 26.3 26.8 latest
docker network create "$NETWORK" >/dev/null 2>&1 || true
# Build once, so the per-server runs print only their rows.
cargo_in_container build --quiet --locked -p crucible-conformance
failed=0
for tag in "$@"; do
  "$mode" "$tag"
done
exit "$failed"
