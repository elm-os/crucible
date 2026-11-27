"use client";

import { Button } from "@ui-sdk/react/button";
import { asUiError } from "@/api/errors";
import { ErrorCode } from "./error-code";
import { AlertIcon } from "./icons";

// The last row of a paged list: loads the next page, says when that failed, and starts the list over
// when its cursor is no longer valid.
export function LoadMore({
  noun,
  hasMore,
  loading,
  error,
  onLoad,
  onRestart,
}: {
  noun: string;
  hasMore: boolean;
  loading: boolean;
  error: unknown;
  onLoad: () => void;
  onRestart: () => void;
}) {
  if (error) {
    const failure = asUiError(error);
    if (failure.kind === "cursor_reset") {
      return (
        <div className="mt-3 flex flex-wrap items-center gap-3 text-label text-ink-soft">
          <span>
            {failure.title} {failure.detail}
          </span>
          <Button onClick={onRestart}>Show the list from the top</Button>
        </div>
      );
    }
    return (
      <div className="mt-3 flex flex-col items-start gap-2">
        <div className="flex flex-wrap items-center gap-3 text-label text-ink-soft">
          <span className="flex items-center gap-1.5">
            <AlertIcon className="shrink-0" />
            Couldn&apos;t load more {noun}. {failure.detail}
          </span>
          <Button onClick={onLoad}>Retry loading more {noun}</Button>
        </div>
        <ErrorCode code={failure.reference} label={`more ${noun}`} />
      </div>
    );
  }
  if (!hasMore) return null;
  return (
    <div className="mt-3">
      <Button onClick={onLoad} disabled={loading} aria-busy={loading || undefined}>
        {loading ? `Loading more ${noun}` : `Load more ${noun}`}
      </Button>
    </div>
  );
}
