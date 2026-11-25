#!/usr/bin/env bash
# Runs the conformance suite of crucible-conformance against real ClickHouse servers in Docker.
# The suite includes the differential check of the simulation's store model (the "ChModel differential" rows).
#
#   scripts/conformance.sh single [tag ...]    one server per tag (default tags: 26.3 26.8 latest)
#   scripts/conformance.sh cluster [tag ...]   1 shard x 2 replicas with Keeper, per tag
#
# The servers run one after another, each limited to 1500 MiB, and the suite runs in the dev container on the
# same Docker network. Every server hides rows from users without a row policy (the strict value of
# users_without_row_policies_can_read_rows), so the service-identity check is meaningful. The cluster is the one
# that deploy/compose/compose.cluster.yaml runs (Keeper and two replicas), with the tag's server image.
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
# The cluster mode's changes to the operator's Compose file: the tag's image, the memory limits and the switch.
CLUSTER_OVERRIDE=crates/crucible-conformance/scripts/.cluster-override.yaml
CLUSTER_PROJECT=crucible-conformance-cluster

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

single() {
  local tag=$1 name=crucible-conformance-single
  single_config
  start "$name" "$tag"
  suite --url "http://$name:8123" --label "$tag" || failed=1
  docker rm -f "$name" >/dev/null
}

# compose <args>: the operator's cluster file with this script's override. Compose reads every variable the file
# names, also for the services it does not start.
compose() {
  CLICKHOUSE_PASSWORD="$ADMIN_PW" CLICKHOUSE_CLUSTER_SECRET=conformance-cluster-secret \
    POSTGRES_PASSWORD=conformance-postgres CRUCIBLE_MASTER_KEY="$(printf '5c%.0s' $(seq 1 32))" \
    docker compose -p "$CLUSTER_PROJECT" -f deploy/compose/compose.cluster.yaml -f "$CLUSTER_OVERRIDE" "$@"
}

cluster_override() {
  local tag=$1 replica
  {
    echo "services:"
    echo "  keeper:"
    echo "    mem_limit: 512m"
    for replica in clickhouse-1 clickhouse-2; do
      cat <<YAML
  $replica:
    image: clickhouse/clickhouse-server:$tag
    mem_limit: $MEMORY
    volumes:
      - type: bind
        source: $ROOT/$CONFIG
        target: /etc/clickhouse-server/config.d/conformance.xml
        read_only: true
YAML
    done
  } >"$CLUSTER_OVERRIDE"
}

cluster() {
  local tag=$1
  single_config
  cluster_override "$tag"
  compose up -d --wait keeper clickhouse-1 clickhouse-2
  NETWORK="${CLUSTER_PROJECT}_default" suite --url http://clickhouse-1:8123 --replica-url http://clickhouse-2:8123 \
    --cluster crucible --label "$tag cluster" || failed=1
  compose down -v >/dev/null 2>&1
}

cleanup() {
  docker rm -f crucible-conformance-single >/dev/null 2>&1 || true
  if [ -f "$CLUSTER_OVERRIDE" ]; then compose down -v >/dev/null 2>&1 || true; fi
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  rm -f "$CONFIG" "$CLUSTER_OVERRIDE"
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
