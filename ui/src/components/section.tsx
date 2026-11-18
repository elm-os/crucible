"use client";

import type { UseQueryResult } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { type ReactNode, useEffect, useId, useState } from "react";
import { asUiError, type UiError } from "@/api/errors";
import { AlertIcon } from "./icons";
import { ConnectTrigger } from "./shell/connect-dialog";

type LoadingPhase = "hidden" | "skeleton" | "slow";

const SKELETON_AFTER_MS = 300;
const LABEL_AFTER_MS = 5_000;

const updatedFormat = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });

export function useLoadingPhase(loading: boolean): LoadingPhase {
  const [phase, setPhase] = useState<LoadingPhase>("hidden");
  useEffect(() => {
    if (!loading) return;
    const skeleton = setTimeout(() => setPhase("skeleton"), SKELETON_AFTER_MS);
    const slow = setTimeout(() => setPhase("slow"), LABEL_AFTER_MS);
    return () => {
      clearTimeout(skeleton);
      clearTimeout(slow);
      setPhase("hidden");
    };
  }, [loading]);
  return phase;
}

export function SectionFrame({
  title,
  busy = false,
  className,
  children,
}: {
  title: string;
  busy?: boolean;
  className?: string;
  children: ReactNode;
}) {
  const headingId = useId();
  return (
    <section
      aria-labelledby={headingId}
      aria-busy={busy || undefined}
      className={`rounded-xl border border-ash-border bg-warm-sand ${className ?? ""}`}
    >
      <h2
        id={headingId}
        className="border-b border-ash-border px-4 py-2.5 text-label font-medium text-ink-soft"
      >
        {title}
      </h2>
      <div className="px-4 py-3">{children}</div>
    </section>
  );
}

// One fetch, one frame: loading, error, empty and success all render inside it, so a failure
// here never blanks the rest of the page.
export function Section<T>({
  title,
  query,
  skeleton,
  isEmpty,
  empty,
  children,
  className,
}: {
  title: string;
  query: UseQueryResult<T>;
  skeleton: ReactNode;
  isEmpty: (data: T) => boolean;
  empty: ReactNode;
  children: (data: T) => ReactNode;
  className?: string;
}) {
  const phase = useLoadingPhase(query.isPending);
  const retry = () => query.refetch();
  const label = title.toLowerCase();

  let body: ReactNode;
  if (query.isPending) {
    body = (
      <>
        <output className="sr-only">{phase === "hidden" ? "" : `Loading ${label}`}</output>
        <div aria-hidden="true" className={phase === "hidden" ? "invisible" : undefined}>
          {skeleton}
        </div>
        {phase === "slow" ? (
          <p className="mt-3 text-label text-ink-soft">
            Still loading. Long time ranges take longer.
          </p>
        ) : null}
      </>
    );
  } else if (query.isLoadingError) {
    body = <ErrorCard error={asUiError(query.error)} label={label} onRetry={retry} />;
  } else {
    body = (
      <>
        {query.isRefetchError ? (
          <RefreshFailed
            error={asUiError(query.error)}
            updatedAt={query.dataUpdatedAt}
            label={label}
            onRetry={retry}
          />
        ) : null}
        {isEmpty(query.data) ? empty : children(query.data)}
      </>
    );
  }

  return (
    <SectionFrame title={title} busy={query.isPending} className={className}>
      {body}
    </SectionFrame>
  );
}

export function ErrorCard({
  error,
  label,
  onRetry,
}: {
  error: UiError;
  label: string;
  onRetry: () => Promise<unknown>;
}) {
  return (
    <div data-error={error.kind} className="flex flex-col items-start gap-2 py-6">
      <p className="flex items-center gap-2 text-body font-medium">
        <AlertIcon className="shrink-0" />
        {error.title}
      </p>
      <p className="text-data text-ink-soft">{error.detail}</p>
      <div className="mt-1 flex flex-wrap items-center gap-x-4 gap-y-2">
        {error.action === "retry" ? <Button onClick={onRetry}>Retry {label}</Button> : null}
        {error.action === "sign_in" ? <ConnectTrigger>Connect</ConnectTrigger> : null}
        <Reference id={error.requestId} />
      </div>
    </div>
  );
}

function RefreshFailed({
  error,
  updatedAt,
  label,
  onRetry,
}: {
  error: UiError;
  updatedAt: number;
  label: string;
  onRetry: () => Promise<unknown>;
}) {
  return (
    <div
      data-error={error.kind}
      className="mb-3 flex flex-wrap items-center gap-x-4 gap-y-2 text-label text-ink-soft"
    >
      <span className="flex items-center gap-1.5">
        <AlertIcon className="shrink-0" />
        Couldn&apos;t refresh. Showing data from {updatedFormat.format(updatedAt)}.
      </span>
      <Button onClick={onRetry}>Retry {label}</Button>
      <Reference id={error.requestId} />
    </div>
  );
}

function Reference({ id }: { id: string | null }) {
  if (id === null) return null;
  return (
    <p className="text-label text-ink-soft">
      Reference <span className="font-mono select-all">{id}</span>
    </p>
  );
}
