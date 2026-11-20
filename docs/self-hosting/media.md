# Media

Media are the images, audio, video and files that a trace's inputs and outputs carry. Without object storage they
stay inline in the payload as base64, under the 8 MiB cap of one field, and Crucible stores them with the payload.
With object storage configured, SDKs upload each media object once, straight to the storage, and the payload keeps
only a short reference to it. The trace view then shows images, audio and video in place and offers other types as a
download.

Media are optional. A server without object storage answers every media route, including the media endpoint of
compatible SDKs, with `501 media_not_supported`, and the answer names the settings below.

## What it needs

- **The control plane.** Crucible records each object and every use of it in Postgres, so media need `postgres.url`.
  A configuration with object storage and without Postgres is refused at startup with that message.
- **An S3-compatible bucket** that both the server and the clients reach. The service must check the signed headers
  of a presigned upload and return an object's checksum on request. The conformance rows of
  `crucible-s3-conformance` run against the [Versity S3 gateway](https://github.com/versity/versitygw), which
  `deploy/compose/compose.media.yaml` adds. The bucket must exist; Crucible does not create it, and at startup it
  logs whether the bucket answers.

## With Docker Compose

`deploy/compose/compose.media.yaml` adds the Versity S3 gateway to `compose.yaml`, with its data in the volume
`media-data` and the bucket `crucible-media`, and points Crucible at it. Add two settings to `.env`, then start both
files:

```sh
printf 'MEDIA_ACCESS_KEY=%s\nMEDIA_SECRET_KEY=%s\n' "$(openssl rand -hex 8)" "$(openssl rand -hex 24)" >> .env
docker compose -f compose.yaml -f compose.media.yaml up -d --wait
```

The gateway listens on port 7070 (`CRUCIBLE_MEDIA_PORT` changes the host port). Uploads and downloads go between the
clients and the gateway directly, so the gateway must be reachable at the address that upload and download links
name: `http://localhost:7070` by default, which works on the machine that runs Compose. For SDKs or browsers on other
machines, set `CRUCIBLE_MEDIA_PUBLIC_URL` in `.env` to the gateway's address as they reach it, for example
`http://crucible.internal:7070`.

## Settings

| Setting | Variable | Meaning |
|---|---|---|
| `object_store.endpoint` | `CRUCIBLE_OBJECT_STORE_ENDPOINT` | The endpoint Crucible reaches, for example `http://versitygw:7070` or `https://s3.eu-west-1.amazonaws.com`. Setting it turns media on |
| `object_store.public_endpoint` | `CRUCIBLE_OBJECT_STORE_PUBLIC_ENDPOINT` | The endpoint SDKs and browsers reach, which presigned links name. Default: `object_store.endpoint` |
| `object_store.bucket` | `CRUCIBLE_OBJECT_STORE_BUCKET` | The bucket |
| `object_store.region` | `CRUCIBLE_OBJECT_STORE_REGION` | The region the request signatures name. Default `us-east-1` |
| `object_store.access_key` | `CRUCIBLE_OBJECT_STORE_ACCESS_KEY` | The access key id |
| `object_store.secret_key`, `object_store.secret_key_file` | `CRUCIBLE_OBJECT_STORE_SECRET_KEY`, `CRUCIBLE_OBJECT_STORE_SECRET_KEY_FILE` | The secret access key, or a file that holds it |
| `object_store.url_style` | `CRUCIBLE_OBJECT_STORE_URL_STYLE` | `path` (`host/bucket/key`, the default) or `virtual_host` (`bucket.host/key`) |
| `object_store.prefix` | `CRUCIBLE_OBJECT_STORE_PREFIX` | The key prefix of media objects. Default `media` |
| `media.max_bytes` | `CRUCIBLE_MEDIA_MAX_BYTES` | The largest media object, in bytes. Default 67108864 (64 MiB) |

Objects are stored under `<prefix>/<project id>/<year>/<month>/<media id>`.

## How an upload works

1. The SDK asks for an upload (`POST /api/v1/projects/{project_id}/media`, with an API key that has the `ingest`
   scope) with the media's content type, length and SHA-256, and the trace, span and payload field that use it.
2. When the project already holds these bytes, Crucible records the new use and answers without an upload. Otherwise
   it answers with a presigned `PUT` link that works for an hour; the content type, the length and the SHA-256 are
   signed into it, so the storage refuses any other body.
3. The SDK uploads to the link and confirms (`PATCH .../media/{media_id}`). Crucible asks the storage for the
   object's length and checksum and marks the media uploaded only when they match.
4. Readers get the media's metadata and a presigned download link that works for 5 minutes. Common image, audio and
   video types are served inline; every other type downloads as a file.

The content type must be one of a list of common image, audio, video, text and document types, or
`application/octet-stream`. SVG, HTML, PDF and the other types that can carry scripts or active content are always
served as attachments.

## The browser and the storage

The trace view loads images, audio and video from the storage's public endpoint, so that origin must be reachable
from the browser. Crucible's pages allow media from exactly that origin and from nowhere else. A browser that cannot
reach it shows "This media couldn't be shown." with a retry.

## Deletion and retention

- A deletion request removes the uses of the traces it deletes, and media also follow the project's payload retention
  (`payload_retention_days`) by the time each use was recorded.
- Every hour the collector deletes the objects that have no use left, and the uploads that were asked for more than
  24 hours ago and never confirmed. A download link handed out before can outlive a deletion by at most 5 minutes.
- Once a week it lists the bucket under the prefix and deletes the objects older than 48 hours that no media record
  names, which a server stopped between deleting a record and its object can leave behind.
- As a backstop, give the bucket a lifecycle rule that expires incomplete multipart uploads, and objects under the
  prefix that are older than your longest retention.
