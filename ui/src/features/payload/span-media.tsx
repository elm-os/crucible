"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { useState } from "react";
import { getMedia, listSpanMedia, type PayloadSpan } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { Media } from "@/api/generated/Media";
import type { SpanMedia as Reference } from "@/api/generated/SpanMedia";
import { EmptyState } from "@/components/empty-state";
import { ErrorCard, useLoadingPhase } from "@/components/section";
import { useAuth } from "@/lib/auth";
import { formatBytes } from "./payload-viewer";

// A presigned download lives 5 minutes; a link read 4 minutes ago is fetched again before it is used.
const LINK_FRESH_MS = 4 * 60_000;

// How a server turns media on: the repository's own guide.
const MEDIA_GUIDE = "https://github.com/elm-os/crucible/blob/main/docs/self-hosting/media.md";

const FIELD_NOUN: Record<string, string> = {
  input: "input",
  output: "output",
  metadata: "metadata",
};

function kindOf(contentType: string): "image" | "audio" | "video" | "file" {
  const top = contentType.split("/")[0];
  return top === "image" || top === "audio" || top === "video" ? top : "file";
}

function describe(media: Media): string {
  return `${media.content_type}, ${formatBytes(media.len)}`;
}

// Where a reference appears: its payload field and the pointer of the string that names it, or that it was
// uploaded for the span without the payload naming it.
function whereOf(reference: Reference): string {
  const field = reference.field ? (FIELD_NOUN[reference.field] ?? reference.field) : undefined;
  if (reference.pointer === undefined) {
    return field ? `Uploaded for the span's ${field}` : "Uploaded for the span";
  }
  const named = `In the ${field ?? "payload"}`;
  return reference.pointer === "" ? named : `${named} at ${reference.pointer}`;
}

// Keys from what each reference is, numbered when the same media appears twice at the same place.
function keysOf(references: Reference[]): string[] {
  const seen = new Map<string, number>();
  return references.map((reference) => {
    const base = `${reference.field ?? ""}|${reference.pointer ?? "upload"}|${reference.media?.id ?? reference.content_type}`;
    const count = seen.get(base) ?? 0;
    seen.set(base, count + 1);
    return count === 0 ? base : `${base}|${count}`;
  });
}

// The media one span references (contract 14), which the server reads out of the span's input, output and
// metadata, with the media uploaded for it. They live on the object store: without one the panel says so.
export function SpanMedia({ project, span }: { project: string; span: PayloadSpan }) {
  const auth = useAuth();
  const off = auth.mode === "session" && !auth.session.features.media.enabled;
  const references = useQuery({
    queryKey: [project, "span-media", span.traceId, span.spanId],
    enabled: !off,
    staleTime: 60_000,
    queryFn: ({ signal }) => listSpanMedia(project, span, signal),
  });
  const phase = useLoadingPhase(!off && references.isPending);
  if (off) return <NoObjectStorage />;
  if (references.isPending) {
    return (
      <div aria-busy="true" className="flex flex-col gap-2">
        <output className="sr-only">
          {phase === "hidden" ? "" : "Loading the media of this span"}
        </output>
        <div
          aria-hidden="true"
          className={phase === "hidden" ? "invisible" : "skeleton h-24 w-40"}
        />
      </div>
    );
  }
  if (references.isError) {
    const error = asUiError(references.error);
    if (error.code === "media_not_supported") return <NoObjectStorage />;
    return (
      <ErrorCard error={error} label="media of this span" onRetry={() => references.refetch()} />
    );
  }
  const found = references.data.data;
  if (found.length === 0) {
    return (
      <EmptyState kind="filtered" title="This span references no media.">
        Images, audio and files that its input, output or metadata reference show here.
      </EmptyState>
    );
  }
  const keys = keysOf(found);
  return (
    <ul aria-label="Media of this span" className="flex flex-col gap-3">
      {found.map((reference, at) => (
        <li key={keys[at]} className="flex max-w-full flex-col gap-1">
          <p className="text-label text-ink-soft break-all">{whereOf(reference)}</p>
          {reference.media ? (
            <MediaItem project={project} media={reference.media} />
          ) : (
            <NotStored contentType={reference.content_type} />
          )}
        </li>
      ))}
    </ul>
  );
}

function NoObjectStorage() {
  return (
    <EmptyState kind="unavailable" title="This server has no object storage, so it keeps no media.">
      <a href={MEDIA_GUIDE} target="_blank" rel="noreferrer" className="underline">
        How to turn media on
      </a>
    </EmptyState>
  );
}

const frame = "flex flex-col gap-1.5 rounded-md border border-ash-border bg-parchment-white p-2";

function NotStored({ contentType }: { contentType: string }) {
  return (
    <div data-media="missing" className={frame}>
      <p className="text-data font-medium">Not stored</p>
      <p className="text-label text-ink-soft">
        The payload names {contentType} media that this project doesn&apos;t hold: its upload never
        reached this server, or it was deleted.
      </p>
    </div>
  );
}

function MediaItem({ project, media }: { project: string; media: Media }) {
  const [broken, setBroken] = useState(false);
  const item = useQuery({
    queryKey: [project, "media", media.id],
    enabled: media.state === "uploaded",
    staleTime: LINK_FRESH_MS,
    queryFn: ({ signal }) => getMedia(project, media.id, signal),
  });
  const phase = useLoadingPhase(item.isPending);
  if (media.state !== "uploaded") {
    return (
      <div data-media="pending" className={frame}>
        <p className="text-data font-medium">Upload not finished</p>
        <p className="text-label text-ink-soft">
          {describe(media)}. The SDK never confirmed the upload, so there is nothing to show.
        </p>
      </div>
    );
  }
  if (item.isPending) {
    return (
      <div aria-busy="true" className={frame}>
        <output className="sr-only">{phase === "hidden" ? "" : "Loading the media"}</output>
        <div
          aria-hidden="true"
          className={phase === "hidden" ? "invisible h-24 w-40" : "skeleton h-24 w-40"}
        />
      </div>
    );
  }
  if (item.isError) {
    return <ErrorCard error={asUiError(item.error)} label="media" onRetry={() => item.refetch()} />;
  }
  const url = item.data.url;
  if (broken || url === undefined) {
    return (
      <div data-error="media" className={frame}>
        <p className="text-data font-medium">This media couldn&apos;t be shown.</p>
        <p className="text-label text-ink-soft">
          Its link may have expired, or the object store can&apos;t be reached from this browser.
        </p>
        <div>
          <Button
            onClick={async () => {
              setBroken(false);
              await item.refetch();
            }}
          >
            Retry media
          </Button>
        </div>
      </div>
    );
  }
  const kind = kindOf(media.content_type);
  if (item.data.disposition === "inline" && kind !== "file") {
    const noun = kind === "image" ? "Image" : kind === "audio" ? "Audio" : "Video";
    const label = `${noun} from the payload, ${describe(media)}`;
    return (
      <figure data-media="inline" className={frame}>
        {kind === "image" ? (
          <img
            src={url}
            alt={label}
            className="max-h-64 max-w-full rounded-sm object-contain"
            onError={() => setBroken(true)}
          />
        ) : kind === "audio" ? (
          // Recorded payload media carry no captions to offer.
          // oxlint-disable-next-line jsx-a11y/media-has-caption
          <audio controls src={url} aria-label={label} onError={() => setBroken(true)} />
        ) : (
          // oxlint-disable-next-line jsx-a11y/media-has-caption
          <video
            controls
            src={url}
            aria-label={label}
            className="max-h-64 max-w-full"
            onError={() => setBroken(true)}
          />
        )}
        <figcaption className="text-label text-ink-soft">{describe(media)}</figcaption>
      </figure>
    );
  }
  return (
    <div data-media="download" className={frame}>
      <a href={url} className="text-data font-medium underline">
        Download {describe(media)}
      </a>
      <p className="text-label text-ink-soft">This type isn&apos;t shown in the page.</p>
    </div>
  );
}
