"use client";

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  Progress,
  ProgressIndicator,
  ProgressLabel,
  ProgressTrack,
  ProgressValue,
} from "@ui-sdk/react/progress";
import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import { useState } from "react";
import { getPayloadRange, type PayloadSpan } from "@/api/client";
import { asUiError, UiError } from "@/api/errors";
import type { IoShape } from "@/api/generated/IoShape";
import type { PayloadField } from "@/api/generated/PayloadField";
import { EmptyState } from "@/components/empty-state";
import { ErrorCard, useLoadingPhase } from "@/components/section";
import { JsonTree } from "./json-tree";
import { MessageList } from "./message-list";
import { decode, type Parsed } from "./payload-model";
import { payloadParser } from "./payload-parser";

// The preview is the first kibibyte; a field up to 256 KiB loads on its own, a larger one when asked.
export const PREVIEW_BYTES = 1_024;
export const AUTO_LOAD_BYTES = 256 * 1_024;
const CHUNK_BYTES = 1_024 * 1_024;
const TEXT_CLAMP = 20_000;

const sizeFormat = new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 });

export function formatBytes(bytes: number): string {
  if (bytes < 1_024) return `${bytes} B`;
  if (bytes < 1_024 * 1_024) return `${sizeFormat.format(bytes / 1_024)} KB`;
  return `${sizeFormat.format(bytes / (1_024 * 1_024))} MB`;
}

function concat(parts: Uint8Array[], total: number): Uint8Array {
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

// One payload field of one span: its first kibibyte at once, the rest by byte ranges, then parsed in a
// worker into a tree, a message list or text. `plural` makes the copy agree with a plural noun, and
// `truncated` says whether the span reported the field cut at the size limit (null when not known).
export function PayloadViewer({
  project,
  span,
  field,
  noun,
  plural = false,
  truncated = null,
  ioShape,
}: {
  project: string;
  span: PayloadSpan;
  field: PayloadField;
  noun: string;
  plural?: boolean;
  truncated?: boolean | null;
  ioShape: IoShape | null;
}) {
  const queryClient = useQueryClient();
  const base = [project, "payload", span.traceId, span.spanId, span.startTime, field] as const;
  const head = useQuery({
    queryKey: [...base, "head"],
    queryFn: ({ signal }) => getPayloadRange(project, span, field, 0, PREVIEW_BYTES - 1, signal),
    staleTime: Number.POSITIVE_INFINITY,
  });
  const total = head.data?.total ?? 0;
  const whole = head.data !== undefined && head.data.bytes.length >= total;
  const [asked, setAsked] = useState(false);
  const [loadedBytes, setLoadedBytes] = useState(0);
  const rest = useQuery({
    queryKey: [...base, "rest"],
    enabled: head.data !== undefined && !whole && (total <= AUTO_LOAD_BYTES || asked),
    staleTime: Number.POSITIVE_INFINITY,
    queryFn: async ({ signal }) => {
      const first = head.data?.bytes ?? new Uint8Array(0);
      const parts = [first];
      let loaded = first.length;
      setLoadedBytes(loaded);
      while (loaded < total) {
        const last = Math.min(loaded + CHUNK_BYTES, total) - 1;
        const chunk = await getPayloadRange(project, span, field, loaded, last, signal);
        // A field never changes; one that does was replaced by a retried span, so start over.
        if (chunk.total !== total || chunk.bytes.length === 0)
          throw new UiError({ kind: "unknown" });
        parts.push(chunk.bytes);
        loaded += chunk.bytes.length;
        setLoadedBytes(loaded);
      }
      return concat(parts, total);
    },
  });
  const bytes = whole ? head.data?.bytes : rest.data;
  const doc = base.join("/");
  const parsed = useQuery({
    queryKey: [...base, "parsed"],
    enabled: bytes !== undefined,
    staleTime: Number.POSITIVE_INFINITY,
    retry: false,
    queryFn: () => payloadParser().parse(doc, bytes ?? new Uint8Array(0)),
  });
  const phase = useLoadingPhase(head.isPending);

  if (head.isPending) {
    return (
      <div aria-busy="true" className="flex flex-col gap-2 py-1">
        <output className="sr-only">{phase === "hidden" ? "" : `Loading the ${noun}`}</output>
        <div
          aria-hidden="true"
          className={phase === "hidden" ? "invisible" : "flex flex-col gap-2"}
        >
          <div className="skeleton h-3 w-4/5" />
          <div className="skeleton h-3 w-3/5" />
          <div className="skeleton h-3 w-2/3" />
        </div>
      </div>
    );
  }
  if (head.isError) {
    const error = asUiError(head.error);
    if (error.kind === "not_found") {
      return (
        <EmptyState
          kind="filtered"
          title={`This span's ${noun} ${plural ? "aren't" : "isn't"} stored.`}
        >
          It may have expired under the project&apos;s payload retention, which can be shorter than
          the retention of spans.
        </EmptyState>
      );
    }
    return <ErrorCard error={error} label={noun} onRetry={() => head.refetch()} />;
  }
  if (total === 0) {
    return <EmptyState kind="filtered" title={`No ${noun} ${plural ? "were" : "was"} recorded.`} />;
  }

  const preview = (
    <div className="flex flex-col gap-1">
      <p className="text-label text-ink-soft">
        {formatBytes(total)}
        {whole || rest.isSuccess ? "" : `, showing the first ${formatBytes(PREVIEW_BYTES)}`}
      </p>
      <pre className="max-h-48 overflow-auto rounded-md border border-ash-border bg-parchment-white px-3 py-2 font-mono text-data break-words whitespace-pre-wrap">
        {decode(head.data.bytes.subarray(0, PREVIEW_BYTES))}
        {whole ? "" : "…"}
      </pre>
    </div>
  );

  if (bytes === undefined) {
    if (rest.isError) {
      return (
        <div className="flex flex-col gap-3">
          {preview}
          <ErrorCard
            error={asUiError(rest.error)}
            label={`full ${noun}`}
            onRetry={() => rest.refetch()}
          />
          <p className="text-label text-ink-soft">
            Couldn&apos;t load the full {noun}. The preview above is intact.
          </p>
        </div>
      );
    }
    if (rest.fetchStatus === "fetching") {
      const percent = total === 0 ? 0 : Math.floor((loadedBytes / total) * 100);
      return (
        <div className="flex flex-col gap-3">
          {preview}
          <Progress value={percent} className="flex flex-col gap-1">
            <div className="flex items-center justify-between text-label text-ink-soft">
              <ProgressLabel>
                Loading {formatBytes(total)} of {noun}
              </ProgressLabel>
              <ProgressValue />
            </div>
            <ProgressTrack className="h-1.5 overflow-hidden rounded-full bg-ash-border">
              <ProgressIndicator className="h-full bg-midnight-ink" />
            </ProgressTrack>
          </Progress>
          <div>
            <Button
              onClick={() => {
                setAsked(false);
                void queryClient.cancelQueries({ queryKey: [...base, "rest"] });
              }}
            >
              Cancel
            </Button>
          </div>
        </div>
      );
    }
    return (
      <div className="flex flex-col gap-3">
        {preview}
        <div>
          <Button onClick={() => setAsked(true)}>Load {formatBytes(total)}</Button>
        </div>
      </div>
    );
  }

  if (parsed.isPending) {
    return (
      <div aria-busy="true" className="flex flex-col gap-3">
        {preview}
        <output className="text-label text-ink-soft">Reading the {noun}</output>
      </div>
    );
  }
  const note = textNote(noun, plural, parsed.isError ? "unreadable" : truncated ? "cut" : "text");
  if (parsed.isError) {
    return <TextView text={decode(bytes)} note={note} />;
  }
  return (
    <ParsedView
      doc={doc}
      noun={noun}
      parsed={parsed.data}
      note={note}
      ioShape={ioShape}
      total={total}
    />
  );
}

// What a field shown as text says about itself: why it could not be read, that the span reported it cut,
// or plainly that it is text.
function textNote(noun: string, plural: boolean, why: "unreadable" | "cut" | "text"): string {
  const shows = plural ? "they show" : "it shows";
  if (why === "unreadable")
    return `The ${noun} couldn't be read as a document, so ${shows} as text.`;
  if (why === "cut")
    return `The ${noun} ${plural ? "were" : "was"} cut at the size limit, so ${shows} as text.`;
  return "Text";
}

function ParsedView({
  doc,
  noun,
  parsed,
  note,
  ioShape,
  total,
}: {
  doc: string;
  noun: string;
  parsed: Parsed;
  note: string;
  ioShape: IoShape | null;
  total: number;
}) {
  const messages =
    parsed.kind === "json" && (ioShape === "genai_messages" || ioShape === "indexed_messages")
      ? parsed.messages
      : null;
  const [view, setView] = useState<"messages" | "json">(messages ? "messages" : "json");
  if (parsed.kind === "text") {
    return <TextView text={parsed.text} note={note} />;
  }
  const label = noun.charAt(0).toUpperCase() + noun.slice(1);
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-label text-ink-soft">{formatBytes(total)} of JSON</p>
        {messages ? (
          <SegmentedControl
            aria-label={`${label} view`}
            value={view}
            onValueChange={(next) => setView(next === "json" ? "json" : "messages")}
          >
            <SegmentedControlIndicator />
            <SegmentedControlItem value="messages" className="text-data">
              Messages
            </SegmentedControlItem>
            <SegmentedControlItem value="json" className="text-data">
              JSON
            </SegmentedControlItem>
          </SegmentedControl>
        ) : null}
      </div>
      {messages && view === "messages" ? (
        <MessageList label={`${label} messages`} messages={messages} />
      ) : parsed.top.length === 0 ? (
        <pre className="rounded-md border border-ash-border bg-parchment-white px-3 py-2 font-mono text-data break-words whitespace-pre-wrap">
          {parsed.root.preview}
        </pre>
      ) : (
        <JsonTree
          label={label}
          top={parsed.top}
          load={(pointer) => payloadParser().children(doc, pointer)}
        />
      )}
    </div>
  );
}

function TextView({ text, note }: { text: string; note: string }) {
  const [all, setAll] = useState(false);
  const long = text.length > TEXT_CLAMP;
  return (
    <div className="flex flex-col items-start gap-2">
      <p className="text-label text-ink-soft">{note}</p>
      <pre className="max-h-[28rem] w-full overflow-auto rounded-md border border-ash-border bg-parchment-white px-3 py-2 font-mono text-data break-words whitespace-pre-wrap">
        {long && !all ? `${text.slice(0, TEXT_CLAMP)}…` : text}
      </pre>
      {long && !all ? (
        <Button onClick={() => setAll(true)}>
          Show all {text.length.toLocaleString("en-US")} characters
        </Button>
      ) : null}
    </div>
  );
}
