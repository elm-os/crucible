#!/usr/bin/env bash
# Compose smoke test, run by the compose-smoke job of .github/workflows/docker.yml: starts compose.yaml with the
# given image, checks that the migrations are recorded in Postgres, creates the organization and its owner with
# `crucible admin init`, signs in, makes an API key with `crucible admin create-key`, sends one trace over OTLP/HTTP
# with curl and reads it back through the REST API. It asks nothing: every step runs from this script.
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
printf 'CRUCIBLE_IMAGE=%s\nCRUCIBLE_BIND_ADDRESS=127.0.0.1\nCRUCIBLE_HTTP_PORT=%s\nCRUCIBLE_OTLP_HTTP_PORT=%s\n' \
  "$image" "$http_port" "$otlp_port" >> .env

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

echo "creating the organization and its owner"
owner_email=owner@smoke.example
owner_password=$(openssl rand -hex 16)
printf '%s\n' "$owner_password" | compose exec -T crucible crucible admin init \
  --org-name Smoke --email "$owner_email" --name Owner

echo "signing in from the server's own origin"
base="http://127.0.0.1:$http_port"
session=$(curl --fail --silent --show-error --output /dev/null --write-out '%header{set-cookie}' \
  -X POST "$base/api/v1/auth/login" --header 'Content-Type: application/json' --header 'X-Crucible-CSRF: prelogin' \
  --header "Origin: $base" --data '{"email":"'"$owner_email"'","password":"'"$owner_password"'"}')
case "$session" in
  crucible_session=*) ;;
  *) echo "sign-in set no session cookie: '$session'" >&2; exit 1 ;;
esac

echo "making an API key for the first project, default"
made=$(compose exec -T crucible crucible admin create-key --project default --note 'compose smoke')
project_id=$(printf '%s\n' "$made" | sed -n 's/^project_id = "\(.*\)"$/\1/p')
secret=$(printf '%s\n' "$made" | sed -n 's/^secret_key = "\(.*\)"$/\1/p')
if [ -z "$project_id" ] || [ -z "$secret" ]; then
  echo "crucible admin create-key printed no key: $made" >&2
  exit 1
fi

trace_id=$(openssl rand -hex 16)
span_id=$(openssl rand -hex 8)
now=$(date +%s)
# A key made outside the server reaches it through the key feed within a few seconds (contract 11.8).
echo "sending trace $trace_id"
for attempt in $(seq 1 20); do
  status=$(curl --silent --output /dev/null --write-out '%{http_code}' -X POST "http://127.0.0.1:$otlp_port/v1/traces" \
    --header "Authorization: Bearer $secret" --header 'Content-Type: application/json' \
    --data '{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name",
      "value":{"stringValue":"compose-smoke"}}]},
      "scopeSpans":[{"spans":[{"traceId":"'"$trace_id"'","spanId":"'"$span_id"'","name":"compose smoke span","kind":1,
      "startTimeUnixNano":"'"$now"'000000000","endTimeUnixNano":"'"$now"'250000000"}]}]}]}' || true)
  if [ "$status" = 200 ]; then
    break
  fi
  if [ "$attempt" = 20 ]; then
    echo "ingest answered $status for 20 s with the new key" >&2
    exit 1
  fi
  sleep 1
done

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
