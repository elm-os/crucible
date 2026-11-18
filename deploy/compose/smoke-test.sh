#!/usr/bin/env bash
# Compose smoke test, run by the compose-smoke job of .github/workflows/docker.yml: starts compose.yaml with the
# given image, checks that the migrations are recorded in Postgres, sends one trace over OTLP/HTTP with curl and
# reads it back through the REST API.
#
#   deploy/compose/smoke-test.sh <image> [http port] [otlp port]
#
# It works in a temporary copy under its own project name, so a stack or a .env already in deploy/compose is not
# touched. The first-run commands are the ones in docs/self-hosting/quickstart.md.
set -euo pipefail

image=${1:?usage: smoke-test.sh <image> [http port] [otlp port]}
http_port=${2:-18080}
otlp_port=${3:-14318}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
project="crucible-smoke-$$"

compose() {
  docker compose --project-directory "$work" -f "$work/compose.yaml" -f "$work/compose.smoke.yaml" -p "$project" "$@"
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
cat > "$work/compose.smoke.yaml" <<'EOF'
services:
  clickhouse:
    mem_limit: 1500m
  postgres:
    mem_limit: 768m
  crucible:
    mem_limit: 1g
EOF
cd "$work"
printf 'CLICKHOUSE_PASSWORD=%s\nPOSTGRES_PASSWORD=%s\nCRUCIBLE_MASTER_KEY=%s\n' "$(openssl rand -hex 16)" "$(openssl rand -hex 16)" "$(openssl rand -hex 32)" > .env
key() { printf '%s0%s' "$1" "$(openssl rand -base64 96 | tr -dc '0-9A-Za-z' | head -c 42)"; }
printf '[[project_key]]\nproject_id = "%s"\npublic_key = "%s"\nsecret = "%s"\n' \
  "$(openssl rand -hex 16 | sed -E 's/(.{8})(.{4})(.{4})(.{4})(.{12})/\1-\2-\3-\4-\5/')" \
  "$(key cpk_)" "$(key csk_)" > keys.toml
printf 'CRUCIBLE_IMAGE=%s\nCRUCIBLE_BIND_ADDRESS=127.0.0.1\nCRUCIBLE_HTTP_PORT=%s\nCRUCIBLE_OTLP_HTTP_PORT=%s\n' \
  "$image" "$http_port" "$otlp_port" >> .env

project_id=$(sed -n 's/^project_id = "\(.*\)"$/\1/p' keys.toml)
secret=$(sed -n 's/^secret = "\(.*\)"$/\1/p' keys.toml)

echo "starting $image"
compose up --detach --wait --wait-timeout 240

echo "the migrations ledger in Postgres holds the ClickHouse steps"
recorded=$(compose exec -T postgres psql --username=crucible --dbname=crucible --tuples-only --no-align --command \
  "SELECT count(*) FROM schema_migrations WHERE id IN ('0001_schema_base', '0002_access_service')")
[ "$recorded" = 2 ] || { echo "schema_migrations holds $recorded of the 2 ClickHouse steps" >&2; exit 1; }

echo "the UI is embedded and served precompressed"
encoding=$(curl --fail --silent --show-error --output /dev/null --write-out '%header{content-encoding}' \
  --header 'Accept-Encoding: br' "http://127.0.0.1:$http_port/traces/")
[ "$encoding" = br ] || { echo "GET /traces/ came back with content-encoding '$encoding', expected br" >&2; exit 1; }

trace_id=$(openssl rand -hex 16)
span_id=$(openssl rand -hex 8)
now=$(date +%s)
echo "sending trace $trace_id"
curl --fail --silent --show-error -X POST "http://127.0.0.1:$otlp_port/v1/traces" \
  --header "Authorization: Bearer $secret" --header 'Content-Type: application/json' \
  --data '{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"compose-smoke"}}]},
    "scopeSpans":[{"spans":[{"traceId":"'"$trace_id"'","spanId":"'"$span_id"'","name":"compose smoke span","kind":1,
    "startTimeUnixNano":"'"$now"'000000000","endTimeUnixNano":"'"$now"'250000000"}]}]}]}'
echo

echo "reading it back through the API"
url="http://127.0.0.1:$http_port/api/v1/projects/$project_id/traces/$trace_id"
for _ in $(seq 1 60); do
  body=$(curl --silent --header "Authorization: Bearer $secret" "$url" || true)
  case "$body" in
    *'"name":"compose smoke span"'*)
      echo "ok: the trace came back"
      exit 0
      ;;
  esac
  sleep 1
done
echo "the trace did not come back within 60 s; last response: $body" >&2
exit 1
