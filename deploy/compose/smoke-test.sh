#!/usr/bin/env bash
# Compose smoke test, run by the compose-smoke job of .github/workflows/docker.yml: starts compose.yaml with the
# given image, checks that the migrations are recorded in Postgres, creates the organization and its owner with
# `crucible admin init`, signs in, makes an API key with `crucible admin create-key`, sends one trace over OTLP/HTTP
# with curl and reads it back through the REST API. Then it upgrades the stack to a second build of the image with
# `docker compose up -d --wait`, which must return once the new server is ready, and reads the trace again. Last it
# turns media on with compose.media.yaml, uploads bytes through the presigned flow (ask, PUT, confirm), downloads
# them through a presigned link and compares them, and exports the trace, whose file then downloads from the object
# store. It asks nothing: every step runs from this script. The media steps read JSON with jq.
#
#   deploy/compose/smoke-test.sh <image> [http port] [otlp port] [media port]
#
# It works in a temporary copy under its own project name, so a stack or a .env already in deploy/compose is not
# touched. The first-run commands are the ones in docs/self-hosting/quickstart.md.
set -euo pipefail

image=${1:?usage: smoke-test.sh <image> [http port] [otlp port] [media port]}
http_port=${2:-18080}
otlp_port=${3:-14318}
media_port=${4:-17070}
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
project="crucible-smoke-$$"
# The second build of the image that the upgrade step starts: the same files under another image id.
upgrade="$project-upgrade:latest"
# The Compose files of the stack; the media step adds the media overlay.
files=(-f "$work/compose.yaml" -f "$work/compose.smoke.yaml")

compose() {
  docker compose --project-directory "$work" "${files[@]}" -p "$project" "$@"
}
# shellcheck disable=SC2329 # run by the EXIT trap below
finish() {
  local status=$?
  if [ "$status" -ne 0 ]; then
    compose ps --all || true
    compose logs --no-color || true
  fi
  compose down --volumes --remove-orphans >/dev/null 2>&1 || true
  docker image rm "$upgrade" >/dev/null 2>&1 || true
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
secrets=$(printf 'CLICKHOUSE_PASSWORD=%s\nPOSTGRES_PASSWORD=%s\nCRUCIBLE_MASTER_KEY=%s' "$(openssl rand -hex 16)" "$(openssl rand -hex 16)" "$(openssl rand -hex 32)")
# .env: the secrets of this run and the image to start.
write_env() {
  printf '%s\nCRUCIBLE_IMAGE=%s\nCRUCIBLE_BIND_ADDRESS=127.0.0.1\nCRUCIBLE_HTTP_PORT=%s\nCRUCIBLE_OTLP_HTTP_PORT=%s\n' \
    "$secrets" "$1" "$http_port" "$otlp_port" > .env
}
write_env "$image"

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
read_back() {
  for _ in $(seq 1 60); do
    body=$(curl --silent --header "Authorization: Bearer $secret" "$url" || true)
    case "$body" in
      *'"name":"compose smoke span"'*) return 0 ;;
    esac
    sleep 1
  done
  echo "the trace did not come back within 60 s; last response: $body" >&2
  return 1
}
read_back
echo "ok: the trace came back"

# An upgrade as docs/self-hosting/operations.md describes it, to an image with another id: Compose stops the old
# server, starts the new one and restarts the ready check, and `up --wait` returns once the new server is ready.
echo "upgrading to a second build of the image"
printf 'FROM %s\nLABEL org.opencontainers.image.revision=compose-smoke-upgrade\n' "$image" \
  | docker build --quiet --tag "$upgrade" - >/dev/null
write_env "$upgrade"
started=$(date +%s)
compose up --detach --wait --wait-timeout 240
running=$(docker inspect --format '{{.Config.Image}}' "$(compose ps --quiet crucible)")
[ "$running" = "$upgrade" ] || { echo "after the upgrade the server runs $running, expected $upgrade" >&2; exit 1; }
ready=$(curl --silent --output /dev/null --write-out '%{http_code}' "http://127.0.0.1:$http_port/readyz" || true)
[ "$ready" = 200 ] || { echo "up --wait returned before the new server was ready: /readyz answered $ready" >&2; exit 1; }
echo "the upgraded stack was ready after $(( $(date +%s) - started )) s"
read_back
echo "ok: the trace came back after the upgrade"

# Media as docs/self-hosting/media.md turns them on: the media overlay and its two keys in .env. The links Crucible
# hands out name CRUCIBLE_MEDIA_PUBLIC_URL, which must be the address curl reaches the gateway at, because each
# signature covers the host.
echo "turning media on with compose.media.yaml"
command -v jq >/dev/null || { echo "the media steps read JSON with jq; install jq" >&2; exit 1; }
cp "$here/compose.media.yaml" "$work/"
cat > "$work/compose.smoke-media.yaml" <<'EOF'
services:
  versitygw:
    mem_limit: 256m
EOF
printf 'MEDIA_ACCESS_KEY=%s\nMEDIA_SECRET_KEY=%s\nCRUCIBLE_MEDIA_PORT=%s\nCRUCIBLE_MEDIA_PUBLIC_URL=http://127.0.0.1:%s\n' \
  "$(openssl rand -hex 8)" "$(openssl rand -hex 24)" "$media_port" "$media_port" >> .env
files+=(-f "$work/compose.media.yaml" -f "$work/compose.smoke-media.yaml")
compose up --detach --wait --wait-timeout 240

api="http://127.0.0.1:$http_port/api/v1/projects/$project_id"
auth="Authorization: Bearer $secret"
echo "uploading media through the presigned flow"
head -c 70000 /dev/urandom > "$work/media.bin"
media_len=$(wc -c < "$work/media.bin" | tr -d ' ')
media_sha=$(sha256sum "$work/media.bin" | cut -d' ' -f1)
asked=$(curl --fail --silent --show-error -X POST "$api/media" --header "$auth" --header 'Content-Type: application/json' \
  --data '{"content_type":"application/octet-stream","len":'"$media_len"',"sha256":"'"$media_sha"'",
    "trace_id":"'"$trace_id"'","span_id":"'"$span_id"'","field":"input"}')
media_id=$(jq -r '.media_id' <<<"$asked")
upload_url=$(jq -r '.upload.url' <<<"$asked")
case "$upload_url" in
  "http://127.0.0.1:$media_port/"*) ;;
  *) echo "the upload link is not on the gateway's public address: $asked" >&2; exit 1 ;;
esac
signed=()
while IFS= read -r line; do
  signed+=(--header "$line")
done < <(jq -r '.upload.headers[] | "\(.name): \(.value)"' <<<"$asked")
status=$(curl --silent --output /dev/null --write-out '%{http_code}' -X PUT "${signed[@]}" \
  --data-binary "@$work/media.bin" "$upload_url")
[ "$status" = 200 ] || { echo "the presigned upload answered $status" >&2; exit 1; }
confirmed=$(curl --fail --silent --show-error -X PATCH "$api/media/$media_id" --header "$auth")
[ "$(jq -r '.state' <<<"$confirmed")" = uploaded ] || { echo "the confirm answered $confirmed" >&2; exit 1; }
download_url=$(curl --fail --silent --show-error "$api/media/$media_id" --header "$auth" | jq -r '.url')
curl --fail --silent --show-error --output "$work/media.down" "$download_url"
cmp "$work/media.bin" "$work/media.down" || { echo "the downloaded media differs from the upload" >&2; exit 1; }
echo "ok: the media came back byte for byte"

# With object storage an export's file goes to the store, and its download is a 303 to a presigned link there.
echo "exporting the trace to the object store"
from=$(date -u -d "@$((now - 3600))" +%Y-%m-%dT%H:%M:%SZ)
to=$(date -u -d "@$((now + 3600))" +%Y-%m-%dT%H:%M:%SZ)
export_id=$(curl --fail --silent --show-error -X POST "$api/exports" --header "$auth" \
  --header 'Content-Type: application/json' \
  --data '{"format":"crucible-jsonl","from":"'"$from"'","to":"'"$to"'","trace_ids":["'"$trace_id"'"]}' | jq -r '.id')
state=queued
for _ in $(seq 1 90); do
  state=$(curl --silent "$api/exports/$export_id" --header "$auth" | jq -r '.state')
  case "$state" in
    ready) break ;;
    failed | expired) break ;;
  esac
  sleep 1
done
[ "$state" = ready ] || { echo "the export is $state after 90 s" >&2; exit 1; }
answer=$(curl --silent --output /dev/null --write-out '%{http_code} %{redirect_url}' \
  "$api/exports/$export_id/download" --header "$auth")
case "$answer" in
  "303 http://127.0.0.1:$media_port/"*) ;;
  *) echo "the export download answered '$answer', not a 303 to the gateway" >&2; exit 1 ;;
esac
curl --fail --silent --show-error --output "$work/export.jsonl" "${answer#303 }"
checksum=$(curl --fail --silent --show-error "$api/exports/$export_id" --header "$auth" | jq -r '.checksum')
[ "$(sha256sum "$work/export.jsonl" | cut -d' ' -f1)" = "$checksum" ] \
  || { echo "the exported file does not match its checksum" >&2; exit 1; }
grep -q "$span_id" "$work/export.jsonl" || { echo "the exported file does not hold the span" >&2; exit 1; }
echo "ok: the export downloaded from the object store"
