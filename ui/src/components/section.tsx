"use client";

import type { UseQueryResult } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import type { Route } from "next";
import Link from "next/link";
import { type ReactNode, useEffect, useId, useState } from "react";
import { asUiError, type UiError } from "@/api/errors";
import { currentPath, loginHref } from "@/lib/next-path";
import { ErrorCode } from "./error-code";
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
  actions,
  className,
  children,
}: {
  title: string;
  busy?: boolean;
  actions?: ReactNode;
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
      <div
        className={`flex flex-wrap items-center justify-between gap-2 border-b border-ash-border px-4 ${actions ? "py-1.5" : "py-2.5"}`}
      >
        <h2 id={headingId} className="text-label font-medium text-ink-soft">
          {title}
        </h2>
        {actions}
      </div>
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
  actions,
  label = title.toLowerCase(),
  className,
  errorCopy,
  errorActions,
}: {
  title: string;
  query: UseQueryResult<T>;
  skeleton: ReactNode;
  isEmpty: (data: T) => boolean;
  empty: ReactNode;
  children: (data: T) => ReactNode;
  actions?: ReactNode;
  // What "Loading" and "Retry" name, for a title that lowercasing would spoil.
  label?: string;
  className?: string;
  // The section's own words for an error it understands better than the shared table does.
  errorCopy?: (error: UiError) => { title: string; detail: string } | null;
  // What else the section offers when it fails, beside the retry.
  errorActions?: ReactNode;
}) {
  const phase = useLoadingPhase(query.isPending);
  const retry = () => query.refetch();

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
    body = (
      <ErrorCard
        error={asUiError(query.error)}
        label={label}
        onRetry={retry}
        copy={errorCopy}
        actions={errorActions}
      />
    );
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
    <SectionFrame title={title} busy={query.isPending} actions={actions} className={className}>
      {body}
    </SectionFrame>
  );
}

export function ErrorCard({
  error,
  label,
  onRetry,
  copy,
  actions,
}: {
  error: UiError;
  label: string;
  onRetry: () => Promise<unknown>;
  copy?: (error: UiError) => { title: string; detail: string } | null;
  actions?: ReactNode;
}) {
  const words = copy?.(error) ?? error;
  return (
    <div data-error={error.kind} className="flex flex-col items-start gap-2 py-6">
      <p className="flex items-center gap-2 text-body font-medium">
        <AlertIcon className="shrink-0" />
        {words.title}
      </p>
      <p className="text-data text-ink-soft">{words.detail}</p>
      <div className="mt-1 flex flex-wrap items-center gap-x-4 gap-y-2">
        {error.action === "retry" ? <Button onClick={onRetry}>Retry {label}</Button> : null}
        {actions}
        {error.kind === "signed_out" ? (
          <Link href={loginHref(currentPath()) as Route}>Sign in</Link>
        ) : null}
        {error.kind === "unauthenticated" ? <ConnectTrigger>Connect</ConnectTrigger> : null}
      </div>
      <ErrorCode code={error.reference} label={label} />
    </div>
  );
}

export function RefreshFailed({
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
    <div data-error={error.kind} className="mb-3 flex flex-col items-start gap-2">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2 text-label text-ink-soft">
        <span className="flex items-center gap-1.5">
          <AlertIcon className="shrink-0" />
          Couldn&apos;t refresh. Showing data from {updatedFormat.format(updatedAt)}.
        </span>
        <Button onClick={onRetry}>Retry {label}</Button>
      </div>
      <ErrorCode code={error.reference} label={label} />
    </div>
  );
}
