# Operations

Upgrades, backups, health checks, metrics and logs of a running deployment. The commands assume the Compose setup
of the [quickstart](quickstart.md), run from `deploy/compose/` in a POSIX shell (Linux, macOS, or WSL on Windows).

## Upgrades

An upgrade is a new image and a restart:

1. Build the image of the new version under a new tag, from a checkout of that version:

   ```sh
   docker build -f deploy/docker/Dockerfile -t crucible:<new tag> .
   ```

2. Set `CRUCIBLE_IMAGE=crucible:<new tag>` in `.env`.
3. Restart the server on it:

   ```sh
   docker compose up -d --wait
   ```

On start, `--migrate` applies the Postgres migrations and ClickHouse schema steps that the new version adds and
skips the ones the ledger in Postgres already holds. The write-ahead log is recovered and whatever it still holds
is shipped. Stopping the old server lets it finish its open requests first: Compose sends SIGTERM and allows
35 seconds.

- While the old server stops and the new one starts, the port answers nothing, so `--wait` can report
  `container crucible-ready-1 is unhealthy`. That is the `ready` service catching the restart, and the upgrade goes
  on: `curl -s http://127.0.0.1:8080/readyz` answers `ready` once the new server is up, and the `ready` service
  turns healthy at its next check, within 10 seconds (`docker compose ps`).
- Keep `CRUCIBLE_MASTER_KEY` across upgrades. The ClickHouse service users were created with passwords derived
  from it, and the API keys and sealed secrets in Postgres depend on it.
- Migrations only move forward. An older image started on a database that a newer one migrated stays not ready,
  and `/readyz` names the migration it does not know, such as `migration V5__export_jobs is missing from the
  filesystem`. Take a [backup](#backups) before an upgrade so that you can return to it.
- Changes to the storage format are recorded in the change log of the
  [contract](../architecture/contracts.md#15-change-control), with the migration each one needs.

## Backups

A backup is four things:

1. `.env`, for the master key. Restored ClickHouse data cannot be read without it, and neither can the API keys and
   sealed secrets in Postgres. Keep `.env` with the archives it belongs to.
2. The volume `clickhouse-data`: the telemetry.
3. The volume `postgres-data`: the control plane, from the organization and its users to keys, alert rules, saved
   queries, deletion requests and the migrations ledger.
4. The volume `crucible-data`: the write-ahead log, which holds requests that were acknowledged but are not yet in
   ClickHouse, and export files.

Stop the stack first, so that the server drains its open requests and the databases write nothing while the
volumes are copied, then archive each volume. With the project name `crucible` from `compose.yaml`, the volumes are
`crucible_clickhouse-data`, `crucible_postgres-data` and `crucible_crucible-data`:

```sh
docker compose stop
for v in clickhouse-data postgres-data crucible-data; do
  docker run --rm -v "crucible_$v:/data:ro" -v "$PWD:/backup" busybox:1.37 tar -czf "/backup/$v.tar.gz" -C /data .
done
docker compose start
```

To restore, put `.env` back beside the Compose file, let Compose create empty volumes, unpack the archives into
them and start:

```sh
docker compose create
for v in clickhouse-data postgres-data crucible-data; do
  docker run --rm -v "crucible_$v:/data" -v "$PWD:/backup:ro" busybox:1.37 tar -xzf "/backup/$v.tar.gz" -C /data
done
docker compose up -d --wait
```

For a ClickHouse and a Postgres that you run yourself (`compose.cloud.yaml`), back them up with their own tools, and
archive the `crucible-data` volume of the Crucible service in the same window.

## Health

| Path | Answer |
|---|---|
| `GET /healthz` | `ok` while the process runs |
| `GET /readyz` | `ready` once the write-ahead log accepts writes, the startup checks against ClickHouse have passed and, with Postgres, the API keys have loaded from it. Before that, 503 with `not ready: <reason>` |

Both are on the main listener (port 8080). While `/readyz` answers 503, ingest answers 503 too, with
`Retry-After`. The startup checks retry with a pause that grows from 1 s to 15 s, so the server may start before
ClickHouse does. `docker compose logs crucible` shows the same reason as `/readyz`; the
[troubleshooting](troubleshooting.md#the-server-is-not-ready) page lists the common ones.

Once ready, a server stays ready through outages of its databases:

- **ClickHouse unreachable.** Ingest keeps acknowledging into the write-ahead log, and the shipper sends the backlog
  when ClickHouse is back. Trace lists answer from the recent-span index with `meta.partial: true` and
  `partial_reason: store_unavailable`; search, which reads ClickHouse only, answers 503
  `telemetry_store_unavailable`.
- **Postgres unreachable.** Keys the server already knows keep working for ingest and reads
  (`auth.pg_outage_mode = fail_open`), and signed-in sessions keep working for `auth.session_outage_grace`
  (10 minutes). Sign-in and changes to the control plane answer 503 `control_plane_unavailable` until Postgres is
  back ([configuration](configuration.md#security-and-auth)).

## Metrics

The server exports Prometheus metrics at `GET /metrics` on `server.metrics_listen`, by default `127.0.0.1:9464`:
the loopback interface of the server's own host, or of its container. To scrape it from another host or container,
listen on every interface and publish the port, for example in the `crucible` service of `compose.yaml`:

```yaml
    command:
      - serve
      - --migrate
      - --data-dir=/var/lib/crucible/data
      - --metrics-listen=0.0.0.0:9464
    ports:
      - "127.0.0.1:9464:9464"
```

To read it once without changing anything, run curl in the server container's network namespace:

```sh
docker run --rm --network container:crucible-crucible-1 curlimages/curl:8.23.0 -s http://127.0.0.1:9464/metrics
```

| Metric | Type | What it shows |
|---|---|---|
| `crucible_build_info{version}` | gauge | The server's version; always 1 |
| `crucible_http_requests_total{listener,status}` | counter | HTTP requests answered, by listener (`api`, `ingest`) and status |
| `crucible_http_request_duration_microseconds` | histogram | Time from receipt to the response head |
| `crucible_ingest_requests_total{outcome}` | counter | OTLP export requests, by outcome: `accepted`, `partial`, `malformed`, `unauthenticated`, `payload_too_large`, `rate_limited`, `internal` and the rest |
| `crucible_ingest_spans_total{outcome}` | counter | Spans of the acknowledged requests |
| `crucible_ingest_inflight_bytes` | gauge | Request bytes held by ingest, against its budget |
| `crucible_ingest_facts_total`, `crucible_ingest_fact_writes_total` | counter | Fact writes (tags, flags, scores, corrections) made durable, and their outcomes |
| `crucible_wal_durable_lsn`, `crucible_wal_checkpoint_lsn` | gauge | The last durable position of the write-ahead log, and the position below which everything reached ClickHouse |
| `crucible_wal_unshipped_bytes` | gauge | Bytes of the log segments that hold something not yet in ClickHouse. Growth means ClickHouse is not keeping up or not reachable |
| `crucible_wal_segment_bytes`, `crucible_wal_free_bytes` | gauge | The log's size on disk, and the free space of its disk |
| `crucible_wal_sync_duration_microseconds` | histogram | Duration of each disk sync |
| `crucible_wal_failed` | gauge | 1 while the log is in the failed state and ingest is refused |
| `crucible_wal_group_failures_total`, `crucible_wal_appended_frames_total`, `crucible_wal_appended_bytes_total`, `crucible_wal_gc_removed_segments_total` | counter | Commit groups whose write or sync failed; frames and bytes appended; segments removed after shipping |
| `crucible_wal_backend_info{backend,reason}` | gauge | The disk backend in use and why, for a fallback |
| `crucible_shipper_batches_total`, `crucible_shipper_inserts_total`, `crucible_shipper_probes_total`, `crucible_shipper_checkpoints_total` | counter | Batches sealed and shipped, inserts sent and those with an unknown outcome, existence probes, checkpoints |
| `crucible_shipper_quarantined_frames_total`, `crucible_shipper_quarantined_pairs_total` | counter | Frames this binary could not decode or normalize, copied to the `quarantine` directory, and data that failed repeatedly |
| `crucible_shipper_encoded_bytes`, `crucible_shipper_drain_bytes_per_second` | gauge | Rows the shipper holds, against its budget; the rate at which the log drains |
| `crucible_clickhouse_connections_opened_total` | counter | Connections opened to ClickHouse |
| `crucible_auth_key_snapshot_age_seconds` | gauge | Seconds since the API keys last loaded from Postgres; -1 before the first load |
| `crucible_net_accept_errors_total` | counter | Failed accepts on a listener |

## Logs

The server logs to standard error, one line per event, with the level, the module and the fields of the event:
`docker compose logs crucible`. Every refusal it logs says what to change. Startup logs the address of each
listener, the WAL backend, whether the UI is embedded, and the mode (`clickhouse` or `ingest-only`, with
`postgres` or without a control plane). A request that failed on the server side is logged with the request id that
its response carried in `X-Request-Id`.

## Memory and disk

- **Memory.** The server's memory budgets are the recent-span index (`recent.max_bytes`, by default 10% of the
  memory it may use, at most 1 GiB), 512 MiB of request bytes in flight during ingest and 1 GiB of rows in the
  shipper. At startup it adds them up and logs a warning when they exceed 90% of the memory it may use, with the
  amount to give it. With the defaults, give the container at least 2 GiB.
- **Disk.** The write-ahead log lives in `data.dir`. It grows while ClickHouse is unreachable and shrinks as the
  shipper catches up. When the log is above its high-water mark or the disk is nearly full, ingest answers 503 with
  `Retry-After` instead of accepting what it might not keep.
