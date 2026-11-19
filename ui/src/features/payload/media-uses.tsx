"use client";

import { useQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { useState } from "react";
import { getMedia, listMedia } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { Media } from "@/api/generated/Media";
import { ErrorCard, useLoadingPhase } from "@/components/section";
import { useAuth } from "@/lib/auth";
import { formatBytes } from "./payload-viewer";

// A presigned download lives 5 minutes; a link read 4 minutes ago is fetched again before it is used.
const LINK_FRESH_MS = 4 * 60_000;

function kindOf(contentType: string): "image" | "audio" | "video" | "file" {
  const top = contentType.split("/")[0];
  return top === "image" || top === "audio" || top === "video" ? top : "file";
}

function describe(media: Media): string {
  return `${media.content_type}, ${formatBytes(media.len)}`;
}

// The media one payload field of a span uses (contract 14). They live on the object store, which needs a
// control plane: without one, or with media off, nothing is asked for and nothing is shown.
export function MediaUses({
  project,
  traceId,
  spanId,
  field,
  noun,
}: {
  project: string;
  traceId: string;
  spanId: string;
  field: string;
  noun: string;
}) {
  const auth = useAuth();
  const enabled = auth.mode === "session" && auth.session.features.media.enabled;
  const uses = useQuery({
    queryKey: [project, "media", traceId, spanId],
    enabled,
    staleTime: 60_000,
    queryFn: ({ signal }) => listMedia(project, traceId, spanId, signal),
  });
  const phase = useLoadingPhase(enabled && uses.isPending);
  if (!enabled) return null;
  if (uses.isPending) {
    return (
      <div aria-busy="true" className="flex flex-col gap-2">
        <output className="sr-only">
          {phase === "hidden" ? "" : `Loading the media of the ${noun}`}
        </output>
        <div
          aria-hidden="true"
          className={phase === "hidden" ? "invisible" : "skeleton h-24 w-40"}
        />
      </div>
    );
  }
  if (uses.isError) {
    const error = asUiError(uses.error);
    // The deployment has no object store: there is no media to show.
    if (error.code === "media_not_supported") return null;
    return (
      <ErrorCard error={error} label={`media of the ${noun}`} onRetry={() => uses.refetch()} />
    );
  }
  const shown = uses.data.data.filter((use) => use.field === field);
  // Most payloads hold no media; the payload above is the content, so no media adds nothing.
  if (shown.length === 0) return null;
  const heading = `Media in the ${noun}`;
  return (
    <section aria-label={heading} className="flex flex-col gap-2">
      <h3 className="text-label font-medium text-ink-soft">{heading}</h3>
      <ul className="flex flex-wrap gap-3">
        {shown.map((use) => (
          <li key={use.media.id} className="max-w-full">
            <MediaItem project={project} media={use.media} />
          </li>
        ))}
      </ul>
    </section>
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
  const frame = "flex flex-col gap-1.5 rounded-md border border-ash-border bg-parchment-white p-2";
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
