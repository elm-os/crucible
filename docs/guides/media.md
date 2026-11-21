# Media

Traces often carry images, audio, video or files in their inputs and outputs. Crucible keeps them in one of two
ways:

- **Inline**, by default. The media stay inside the payload as the client sent them, usually base64, and count
  toward the 8 MiB cap of one payload field ([size caps](sending-traces.md#size-caps)).
- **In object storage**, when the server has an S3-compatible bucket configured
  ([media](../self-hosting/media.md)). The client uploads each media object once, straight to the bucket, and the
  payload keeps only a short reference. The **Media** tab of the span panel then shows images, audio and video in
  place, and offers other types as a download.

## How media reach the bucket

A client asks Crucible for an upload, puts the bytes to the link it gets, and confirms:

1. `POST /api/v1/projects/{project_id}/media`, with a key that has the `ingest` scope, names the content type, the
   length, the SHA-256 of the bytes, and the trace, span and payload field that use them.
2. When the project already holds the same bytes, Crucible records the new use and answers with the existing
   media id and no upload link. Otherwise it answers with a presigned `PUT` link, valid for an hour, and the headers
   to send with it: the bucket refuses any other content type, length or checksum.
3. The client uploads the bytes to the link, then confirms with `PATCH /api/v1/projects/{project_id}/media/{media_id}`.
   Crucible checks the object's length and checksum in the bucket before it marks the media uploaded.

Readers get a media's details and a download link valid for 5 minutes from
`GET /api/v1/projects/{project_id}/media/{media_id}`, and the media a trace or span uses from
`GET /api/v1/projects/{project_id}/media?trace_id=<trace id>`. The routes and their permissions are in the
[API reference](../api/README.md#media).

## In the span panel

The **Media** tab of the span panel lists the media a span references, which the server finds in two ways: the
references its input, output and metadata hold, each shown with where it appears (for example "In the input at
/0/content/1/image_url/url"), and the media uploaded for the span that its payload doesn't name. Images, audio and
video of common types play in the page; other types are a download. A reference whose media the project doesn't hold,
because its upload never reached the server or the media was deleted, says "Not stored". The same list comes from
`GET /api/v1/projects/{project_id}/spans/{trace_id}/{span_id}/media?start_time=<the span's start>`, at most 64
references per span. On a server without object storage the tab says so and links
[media](../self-hosting/media.md).

SDKs that send to Crucible's compatibility paths upload their media through the same flow: Crucible serves the media
endpoint those SDKs call beside their trace path, and the span's payload keeps the SDK's media reference
([integrations](../integrations/README.md)).

## What is accepted

- A media object is at most `media.max_bytes`, 64 MiB by default; a larger one is refused with 413
  `media_too_large`, which names the setting.
- The content type must be a common image, audio, video, text or document type, or `application/octet-stream`.
- Images, audio and video of common types show in the page. SVG, HTML, PDF and other types that can carry scripts or
  active content always download as files.

## Without object storage

A server without a bucket answers every media route, including the compatibility media endpoint, with
`501 media_not_supported`, and the message names the settings that turn media on. Media sent inline are stored with
the payload as usual.

SDKs that upload media separately replace each media with its reference before they ask for the upload, so a server
without a bucket loses those media. The compatibility media endpoint's answer also names the SDK setting that keeps
media inline, and the server logs that warning the first time it refuses an upload
([integrations](../integrations/README.md)).

## Retention and deletion

A media use lives as long as the payload that holds it: deletion requests remove the uses of the traces they delete,
and payload retention removes uses by the time each was recorded. Every hour Crucible deletes the objects no use
names any more, and the uploads that were never confirmed within 24 hours
([retention and deletion](../self-hosting/retention-and-deletion.md)).
