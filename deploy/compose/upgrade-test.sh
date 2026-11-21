#!/usr/bin/env bash
# Upgrade test, run by the compose-upgrade job of .github/workflows/docker.yml: an upgrade from the newest published
# release to the given image, as docs/self-hosting/operations.md describes it.
#
# 1. Starts compose.yaml with the release, creates the organization's owner and an API key, sends a trace, and sends
#    three MCP tool calls from two hours ago, then waits until the release reads them back.
# 2. Upgrades the stack to the given image and restarts the server twice. The first start of the new image applies
#    the migration steps the release lacks; the two restarts must apply none, which the server's log shows.
# 3. Reads the trace back, and the MCP tool calls of the last seven days. A read that long takes whole hours from the
#    hourly MCP rollup, which holds the release's calls only through the copy of migration step 0005, so it must
#    count each call once.
# 4. Removes step 0005 from the migrations ledger and starts the server without --migrate. It must stay not ready
#    and refuse ingest, saying to run `crucible migrate`, and turn ready once that has run.
#
#   deploy/compose/upgrade-test.sh <image> [release image] [http port] [otlp port]
#
# Like smoke-test.sh, it works in a temporary copy under its own project name and asks nothing.
set -euo pipefail

image=${1:?usage: upgrade-test.sh <image> [release image] [http port] [otlp port]}
release=${2:-ghcr.io/elm-os/crucible:latest}
http_port=${3:-18180}
otlp_port=${4:-14418}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
project="crucible-upgrade-$$"
overrides=(-f "$work/compose.yaml" -f "$work/compose.upgrade.yaml")

compose() {
  docker compose --project-directory "$work" "${overrides[@]}" -p "$project" "$@"
}
# shellcheck disable=SC2329 # run by the EXIT trap below
finish() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    compose ps --all || true
    compose logs --no-color || true
  fi
  compose down --volumes --remove-orphans >/dev/null 2>&1 || true
  rm -rf "$work"
  exit "$status"
}
trap finish EXIT

cp "$here/compose.yaml" "$work/"
# Memory ceilings for this test only, which runs on shared machines; compose.yaml itself sets none.
cat > "$work/compose.upgrade.yaml" <<'EOF'
services:
  clickhouse:
    mem_limit: 1500m
  postgres:
    mem_limit: 768m
  crucible:
    mem_limit: 1g
EOF
# Step 4's server: the same service without --migrate.
cat > "$work/compose.no-migrate.yaml" <<'EOF'
services:
  crucible:
    command:
      - serve
      - --data-dir=/var/lib/crucible/data
EOF
cd "$work"
secrets=$(printf 'CLICKHOUSE_PASSWORD=%s\nPOSTGRES_PASSWORD=%s\nCRUCIBLE_MASTER_KEY=%s' "$(openssl rand -hex 16)" "$(openssl rand -hex 16)" "$(openssl rand -hex 32)")
write_env() {
  printf '%s\nCRUCIBLE_IMAGE=%s\nCRUCIBLE_BIND_ADDRESS=127.0.0.1\nCRUCIBLE_HTTP_PORT=%s\nCRUCIBLE_OTLP_HTTP_PORT=%s\n' \
    "$secrets" "$1" "$http_port" "$otlp_port" > .env
}
base="http://127.0.0.1:$http_port"

# The server's log, from line $1 on.
server_log() {
  docker logs "$(compose ps --quiet crucible)" 2>&1 | tail -n "+$1"
}
log_lines() {
  docker logs "$(compose ps --quiet crucible)" 2>&1 | wc -l
}
# Waits until /readyz answers $1 with a body that contains $2.
wait_ready() {
  local body=""
  for _ in $(seq 1 120); do
    body=$(curl --silent --write-out ' %{http_code}' "$base/readyz" || true)
    case "$body" in
      *"$2"*" $1") return 0 ;;
    esac
    sleep 1
  done
  echo "/readyz did not answer $1 with '$2' within 120 s; last answer: $body" >&2
  return 1
}
# Waits until a GET of $1 with the API key answers a body that contains $2.
wait_body() {
  local body=""
  for _ in $(seq 1 60); do
    body=$(curl --silent --header "Authorization: Bearer $secret" "$1" || true)
    case "$body" in
      *"$2"*) return 0 ;;
    esac
    sleep 1
  done
  echo "GET $1 did not answer '$2' within 60 s; last response: $body" >&2
  return 1
}
# POSTs the OTLP JSON body $1 with the API key; prints the status.
send() {
  curl --silent --output /dev/null --write-out '%{http_code}' -X POST "http://127.0.0.1:$otlp_port/v1/traces" \
    --header "Authorization: Bearer $secret" --header 'Content-Type: application/json' --data "$1" || true
}

echo "starting the release, $release"
write_env "$release"
compose up --detach --wait --wait-timeout 240

echo "creating the organization and its owner"
printf '%s\n' "$(openssl rand -hex 16)" | compose exec -T crucible crucible admin init \
  --org-name Upgrade --email owner@upgrade.example --name Owner

echo "making an API key for the first project, default"
made=$(compose exec -T crucible crucible admin create-key --project default --note 'compose upgrade')
project_id=$(printf '%s\n' "$made" | sed -n 's/^project_id = "\(.*\)"$/\1/p')
secret=$(printf '%s\n' "$made" | sed -n 's/^secret_key = "\(.*\)"$/\1/p')
if [ -z "$project_id" ] || [ -z "$secret" ]; then
  echo "crucible admin create-key printed no key: $made" >&2
  exit 1
fi

trace_id=$(openssl rand -hex 16)
now=$(date +%s)
trace='{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"compose-upgrade"}}]},
  "scopeSpans":[{"spans":[{"traceId":"'"$trace_id"'","spanId":"'"$(openssl rand -hex 8)"'","name":"compose upgrade span",
  "kind":1,"startTimeUnixNano":"'"$now"'000000000","endTimeUnixNano":"'"$now"'250000000"}]}]}]}'
# Server-side MCP tool calls two hours ago, inside a whole hour of a seven-day window, so a seven-day read takes
# them from the hourly rollup.
called=$((now - 7200))
call() {
  printf '{"traceId":"%s","spanId":"%s","name":"tools/call upgrade_lookup","kind":2,"startTimeUnixNano":"%s000000000",
    "endTimeUnixNano":"%s010000000","attributes":[{"key":"mcp.method.name","value":{"stringValue":"tools/call"}},
    {"key":"gen_ai.tool.name","value":{"stringValue":"upgrade_lookup"}}]}' \
    "$(openssl rand -hex 16)" "$(openssl rand -hex 8)" "$called" "$called"
}
calls='{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"upgrade-mcp"}}]},
  "scopeSpans":[{"spans":['"$(call),$(call),$(call)"']}]}]}'

# A key made outside the server reaches it through the key feed within a few seconds (contract 11.8).
echo "sending trace $trace_id and three MCP tool calls"
for attempt in $(seq 1 20); do
  status=$(send "$trace")
  [ "$status" = 200 ] && break
  if [ "$attempt" = 20 ]; then
    echo "ingest answered $status for 20 s with the new key" >&2
    exit 1
  fi
  sleep 1
done
status=$(send "$calls")
[ "$status" = 200 ] || { echo "ingest answered $status to the MCP tool calls" >&2; exit 1; }

trace_url="$base/api/v1/projects/$project_id/traces/$trace_id"
week="$base/api/v1/projects/$project_id/mcp/tools?side=server&from=$(date -u -d "@$((now - 7 * 86400))" +%Y-%m-%dT%H:%M:%SZ)"
three='"tool_name":"upgrade_lookup","stats":{"calls":3,'
echo "reading them back from the release"
wait_body "$trace_url" '"name":"compose upgrade span"'
# The release has stored the calls in the minute rollup, so only the copy of step 0005 brings them to the hourly one.
wait_body "$week&to=$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$three"

echo "upgrading to $image"
write_env "$image"
compose up --detach --wait --wait-timeout 240
running=$(docker inspect --format '{{.Config.Image}}' "$(compose ps --quiet crucible)")
[ "$running" = "$image" ] || { echo "after the upgrade the server runs $running, expected $image" >&2; exit 1; }
if ! server_log 1 | grep -q 'migration step applied.*0005_mcp_tool_rollup_hourly'; then
  echo "the first start of $image did not log that it applied step 0005" >&2
  exit 1
fi

for restart in 1 2; do
  echo "restart $restart"
  from=$(( $(log_lines) + 1 ))
  compose restart crucible
  wait_ready 200 ready
  restart_log=$(server_log "$from")
  case "$restart_log" in
    *'ingest is open'*) ;;
    *) echo "restart $restart: the server's log shows no start" >&2; exit 1 ;;
  esac
  if applied=$(printf '%s\n' "$restart_log" | grep -E 'migration step applied|with other content'); then
    echo "restart $restart applied migration steps again:" >&2
    echo "$applied" >&2
    exit 1
  fi
done
echo "ok: the restarts applied no migration step"

wait_body "$trace_url" '"name":"compose upgrade span"'
echo "ok: the trace came back"
wait_body "$week&to=$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$three"
echo "ok: a seven-day MCP read counts each call once"

echo "starting the server without --migrate on a ledger that lacks step 0005"
compose exec -T postgres psql --username=crucible --dbname=crucible --quiet --command \
  "DELETE FROM schema_migrations WHERE id = '0005_mcp_tool_rollup_hourly'"
overrides+=(-f "$work/compose.no-migrate.yaml")
compose up --detach crucible
wait_ready 503 'run `crucible migrate`'
status=$(send "$trace")
[ "$status" = 503 ] || { echo "ingest answered $status before step 0005 was recorded, expected 503" >&2; exit 1; }
compose exec -T crucible crucible migrate
wait_ready 200 ready
echo "ok: without --migrate the server waited for step 0005 and opened once crucible migrate had run"
