"use client";

import { Button } from "@ui-sdk/react/button";
import { useState } from "react";

// A value the server returns once, like a secret key or an invitation link. It lives only in the
// dialog's state, and agents never read it. The browser has no clipboard API on plain-HTTP origins,
// so the text itself is selectable and the button says when copying fails.
export function ShowOnce({
  label,
  value,
  copyLabel,
}: {
  label: string;
  value: string;
  copyLabel: string;
}) {
  const [copied, setCopied] = useState<boolean | null>(null);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  return (
    <div data-agent-hidden className="flex flex-col gap-2">
      <p className="text-label font-medium">{label}</p>
      <code className="block rounded-md border border-ash-border bg-warm-sand px-3 py-2 font-mono text-data break-all select-all">
        {value}
      </code>
      <div className="flex flex-wrap items-center gap-3">
        <Button onClick={copy}>{copyLabel}</Button>
        <output className="text-label text-ink-soft">{copied ? "Copied." : ""}</output>
      </div>
      {copied === false ? (
        <p role="alert" className="text-label">
          Couldn&apos;t copy here. Select the text above and copy it yourself.
        </p>
      ) : null}
    </div>
  );
}
