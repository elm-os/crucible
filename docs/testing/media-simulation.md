# Media simulation and conformance

How media on object storage (contract 14) are verified: the upload, confirm and download flow and the `media-gc`
collector of `crucible-media`, the media routes, the compat media endpoint, and the store adapter against a real
S3-compatible server. The method is in [`methodology.md`](methodology.md); the job runners below the collector are
in [`jobs-simulation.md`](jobs-simulation.md) and the store models in [`control-store.md`](control-store.md).

## Pieces

| Piece | Where | What it is |
|---|---|---|
| Flow and collector | `crates/crucible-media` | `Media` (create, confirm, get, uses), the `media-gc` leader job, the S3 adapter `S3Store` over `Net`, and the rules: the allowlist, the inline-safe list, the caps and the key layout |
| Unit and property tests | `crates/crucible-media/src`, `crates/crucible-compat/src/media.rs` | The inline-safe rule over arbitrary type texts, the key layout's round trip over arbitrary prefixes and ids, the times of both S3 forms, the compat media id: a hash lies in the range its own id names, and the range holds only hashes with that id |
| Signing properties | `crates/crucible-sim/tests/media_signing.rs` | Over arbitrary keys, types, checksums, lengths and lifetimes on the simulation's clock: an upload is signed for the public endpoint with exactly its type, length and checksum, at the clock's time and for the asked lifetime; a download signs its disposition; each signed input changes the signature; no URL carries the secret key; endpoints that make no bucket URL are refused with the setting to fix |
| Simulated store | `crucible_sim::object_model` | `SimObjectStore`: presigned URLs as grants that a simulated client uses, refused as a real store refuses them; uploads that never arrive; a lax store that checks no signed header and keeps no checksum; a listing that lags writes and deletes; throttling |
| Simulation | `crucible_sim::media_sim`, `crates/crucible-sim/tests/media_dst.rs` | The real flow and the real collector on two nodes over `PgModel` and `SimObjectStore` under faults, with the oracle and the mutants |
| Deletion requests | `crates/crucible-sim/tests/lifecycle_directed.rs` | A deletion by trace deletes the media uses of the trace recorded before it, keeps the others and the later one, and reports the media place `done` |
| Differential, Postgres | The `media` script of `crucible-conformance` | Every statement of `MediaRepo` on `PgModel` and on Postgres 15 and 18, among them a delete of an object row that races a new use, in both orders |
| Conformance, object store | `crucible-s3-conformance` | `S3Store` and a client of its presigned URLs against the Versity S3 gateway in Docker (below) |
| Routes | `crates/crucible-api/tests/api/media.rs` | The flow over `PgModel` and `SimObjectStore`, idempotency, every refusal, the mismatch, and `media_not_supported` without a store |
| End to end | `crates/crucible/tests/e2e.rs` | The real binary with Postgres and the gateway: an upload through the compat media endpoint, confirmed and read back (`CRUCIBLE_E2E_S3_ENDPOINT`) |

## What runs

Clients act every 50 ms to 2 s for an hour of virtual time. Each step is one of:

- an upload of bytes from a pool of six, in one of eight traces of one of two projects, so the same bytes come back
  and are deduplicated. Of the uploads a client is handed, a fifth never arrive, a tenth arrive with other bytes of
  the same length and a tenth with a shorter body (which a strict store refuses and a lax store keeps), and the rest
  arrive whole. Four in five are confirmed at once;
- a confirm of an object asked for earlier;
- a deletion request, which deletes the uses of one trace or, one time in three, of every trace of the project, as
  the deletion executor does with each page of traces;
- a read of an object, whose download is fetched from the presigned URL at once.

Two nodes run the job registry with the collector. The collector's horizons are shortened so that a run covers many
of them: it runs every minute, a pending upload goes after 10 minutes, the orphan scan starts every hour and deletes
objects older than 20 minutes without a row. One project keeps payloads for a day. At the start an object is
planted that no row names. After the hour, the run is quiet for two days and an hour.

## Faults

| Fault | How it is injected |
|---|---|
| Crash | A collector node's tasks are aborted after 20 s to 5 min, and it starts again up to 2 min later |
| Control store | 1 in 100 calls fails as unavailable, 1 in 100 writes ends with an unknown outcome, 1 in 100 statements in a transaction fails with a spurious serialization failure, and every call takes 0.1 to 20 ms |
| Object store | 3 in 100 calls are throttled, and the listing lags writes and deletes by up to 30 s |
| Lax store (one test) | The store checks no signed length or checksum and keeps no checksum |

The faults end with the client phase; the quiet tail has none.

## The oracle

| Invariant | Checked | What it says |
|---|---|---|
| M1 no lost bytes | After every client step, at every read, and at the end | Every object the clients know of that is uploaded is stored under its key, with its declared length, and with its SHA-256 when the store keeps checksums; a download from a fresh presigned URL of an uploaded object finds bytes of its length |
| M2 no garbage | At the end | No pending object is older than the pending horizon, every uploaded object has a use, and every stored object under the prefix has a row |
| M3 retention | At the end | No use of the one-day project is older than its payload cut-off |

With a lax store the confirm catches a body of another length (`media_hash_mismatch`, the object deleted); a body of
the same length with other bytes is a known limit of such a store, and M1 then checks the length only.

## Mutants

Each defect sits behind the `test-mutants` feature of `crucible-media`, which only `crucible-sim` enables.
`every_seeded_defect_is_caught` requires each to fail at least one of six seeds.

| Mutant | Defect | Caught by |
|---|---|---|
| `ObjectBeforeRow` | Each stored object is deleted before its row's transaction, whatever that transaction's outcome | M1: an object whose row a new use kept, or whose delete a fault failed, is gone |
| `KeepUnused` | Uploaded objects without a use are kept | M2 |
| `SkipRetention` | Uses are kept past their project's payload cut-off | M3 |

## The object store conformance rows

`crucible-s3-conformance` runs `crucible-media`'s `S3Store` against a real server, and plays the client of its
presigned URLs:

| Row | What it checks |
|---|---|
| S3 1 | The bucket answers with these credentials |
| S3 2 | A presigned PUT stores the bytes; HEAD reports the length and returns the stored SHA-256 |
| S3 3 | A PUT with another content type, other bytes of the signed length or a body of another length is refused, and leaves no object |
| S3 4 | A presigned GET serves the bytes with `inline` or `attachment` as asked; a stream of the object gives the bytes |
| S3 5 | An expired presigned GET is refused |
| S3 6 | Listing pages in key order from a start key; delete, and a batch delete with a missing key |

```sh
docker network create media-net
docker run -d --rm --name vgw --network media-net -e ROOT_ACCESS_KEY=conformance -e ROOT_SECRET_KEY=conformance-secret \
  -e VGW_BACKEND=posix -e VGW_BACKEND_ARG=/data --entrypoint sh versity/versitygw:v1.10.0 \
  -c "mkdir -p /data/media && exec /usr/local/bin/docker-entrypoint.sh"
docker run --rm --network media-net -v "$PWD":/work -w /work -v crucible-target:/target \
  -v crucible-cargo:/usr/local/cargo/registry crucible-dev:1 cargo run -p crucible-conformance \
  --bin crucible-s3-conformance -- --endpoint http://vgw:7070 --bucket media --access-key conformance \
  --secret-key conformance-secret --label versitygw
```

`.github/workflows/s3-conformance.yml` runs the rows on pull requests that touch `crucible-media`,
`crucible-env/src/object.rs` or the rows themselves.
