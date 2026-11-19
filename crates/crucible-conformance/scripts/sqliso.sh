#!/usr/bin/env bash
# Runs sqliso, the isolation suite of the SQL workspace (contract 7.5), against real ClickHouse servers in Docker.
#
#   scripts/sqliso.sh [tag ...]        default tags: 26.3 26.8 latest
#
# For each tag the suite runs twice, once per value of the server switch
# users_without_row_policies_can_read_rows (strict, then permissive): the access scheme must behave the same
# under both (contract 7.2). Layers A, B and D run under both values, with a shorter layer B under the
# permissive one: there a table without its policies shows every row, so the canary oracle itself must catch
# that mutant. Then the last mutant of layer D, the switch flipped: the permissive server is checked
# against the strict declaration, and the suite must fail.
#
# The servers run one after another, each limited to 1500 MiB, and the suite runs in the dev container on the
# same Docker network.
#
# Environment: CRUCIBLE_DEV_IMAGE (default crucible-dev:1), CRUCIBLE_TARGET_VOLUME (default crucible-target),
# SQLISO_FUZZ (statements of layer B, default 5000), SQLISO_SEED (default 1), SQLISO_IDENTITIES (default 20).
set -euo pipefail
export MSYS_NO_PATHCONV=1

cd "$(dirname "$0")/../../.."
# Docker needs the Windows form of the path under Git Bash.
ROOT=$(pwd -W 2>/dev/null || pwd)
IMAGE=${CRUCIBLE_DEV_IMAGE:-crucible-dev:1}
TARGET_VOLUME=${CRUCIBLE_TARGET_VOLUME:-crucible-target}
NETWORK=crucible-sqliso
SERVER=crucible-sqliso-ch
MEMORY=1500m
ADMIN=crucible_admin
ADMIN_PW=conformance-admin
FUZZ=${SQLISO_FUZZ:-5000}
SEED=${SQLISO_SEED:-1}
IDENTITIES=${SQLISO_IDENTITIES:-20}
# Docker cannot copy from the POSIX temp directory under Git Bash, so the server config lives beside the script.
CONFIG=crates/crucible-conformance/scripts/.sqliso-config.xml

in_container() {
  docker run --rm --cpus=3 --memory=2g --network "$NETWORK" -e CARGO_BUILD_JOBS=3 \
    -e CARGO_INCREMENTAL=0 -e CARGO_PROFILE_DEV_DEBUG=line-tables-only -e CARGO_PROFILE_TEST_DEBUG=line-tables-only \
    -v "$ROOT:/work" -w /work -v "$TARGET_VOLUME:/target" -v crucible-cargo:/usr/local/cargo/registry \
    "$IMAGE" "$@"
}

suite() {
  in_container /target/debug/crucible-sqliso --url "http://$SERVER:8123" --user "$ADMIN" --password "$ADMIN_PW" "$@"
}

start() {
  local tag=$1 switch=$2 value=false
  [ "$switch" = permissive ] && value=true
  cat >"$CONFIG" <<XML
<clickhouse>
  <access_control_improvements>
    <users_without_row_policies_can_read_rows>$value</users_without_row_policies_can_read_rows>
  </access_control_improvements>
</clickhouse>
XML
  chmod 644 "$CONFIG"
  docker rm -f "$SERVER" >/dev/null 2>&1 || true
  docker create --name "$SERVER" --network "$NETWORK" --memory="$MEMORY" \
    -e CLICKHOUSE_USER="$ADMIN" -e CLICKHOUSE_PASSWORD="$ADMIN_PW" \
    -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1 "clickhouse/clickhouse-server:$tag" >/dev/null
  docker cp "$CONFIG" "$SERVER:/etc/clickhouse-server/config.d/sqliso.xml"
  docker start "$SERVER" >/dev/null
  for _ in $(seq 1 60); do
    if docker exec "$SERVER" clickhouse-client --user "$ADMIN" --password "$ADMIN_PW" -q "SELECT 1" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "server $SERVER did not start" >&2
  docker logs "$SERVER" 2>&1 | tail -20 >&2
  return 1
}

cleanup() {
  docker rm -f "$SERVER" >/dev/null 2>&1 || true
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  rm -f "$CONFIG"
}
trap cleanup EXIT

docker network create "$NETWORK" >/dev/null 2>&1 || true
in_container cargo build --quiet --locked -p crucible-conformance --bin crucible-sqliso

tags=("$@")
[ ${#tags[@]} -eq 0 ] && tags=(26.3 26.8 latest)
status=0
for tag in "${tags[@]}"; do
  start "$tag" strict
  suite --label "$tag" --switch strict --fuzz "$FUZZ" --seed "$SEED" --identities "$IDENTITIES" || status=1

  start "$tag" permissive
  suite --label "$tag" --switch permissive --layers a,b,d --fuzz $((FUZZ / 5)) --seed "$SEED" \
    --identities "$IDENTITIES" || status=1

  # Layer D, the switch flipped: this server is permissive and the deployment is declared strict.
  if suite --label "$tag" --switch strict --layers none >/dev/null; then
    echo "FAIL	$tag	D8 mutant: the server switch flipped	the suite passed a server whose switch is not the declared one"
    status=1
  else
    echo "PASS	$tag	D8 mutant: the server switch flipped	caught by W0 deployment as declared"
  fi
done
exit $status
