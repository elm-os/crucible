"use client";

import { Button } from "@ui-sdk/react/button";
import { AlertIcon } from "./icons";

// A page that failed to render, in place of the framework's own message. What was thrown can hold
// internals, so it stays out of the page; the layout around it keeps working.
export function PageError({ retry }: { retry: () => void }) {
  return (
    <div role="alert" className="flex flex-col items-start gap-2 py-6">
      <p className="flex items-center gap-2 text-body font-medium">
        <AlertIcon className="shrink-0" />
        This page couldn&apos;t be shown.
      </p>
      <p className="max-w-prose text-data text-ink-soft">
        Try again. If this keeps happening, reload the page.
      </p>
      <Button onClick={retry} className="mt-1">
        Try again
      </Button>
    </div>
  );
}
