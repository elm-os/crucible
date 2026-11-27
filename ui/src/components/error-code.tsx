"use client";

import { Button } from "@ui-sdk/react/button";
import { useState } from "react";

// Beside an unexpected failure only: the id of its line in the server log. Plain-HTTP origins have no
// clipboard API, so the code also selects with one click; `label` keeps several codes' buttons apart.
export function ErrorCode({ code, label }: { code: string | null; label?: string }) {
  // The last copy's outcome belongs to the code it copied: a new failure's code starts without one.
  const [copied, setCopied] = useState<{ code: string; ok: boolean } | null>(null);
  if (code === null) return null;
  const outcome = copied?.code === code ? copied.ok : null;
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code);
      setCopied({ code, ok: true });
    } catch {
      setCopied({ code, ok: false });
    }
  };
  return (
    <div data-error-code className="flex flex-col items-start gap-1.5 text-label text-ink-soft">
      <p>
        If this keeps happening, give your administrator this code:{" "}
        <code className="font-mono break-all text-midnight-ink select-all">{code}</code>
      </p>
      <div className="flex flex-wrap items-center gap-3">
        <Button onClick={copy} aria-label={label ? `Copy code for ${label}` : undefined}>
          Copy code
        </Button>
        <output>{outcome ? "Copied." : ""}</output>
      </div>
      {outcome === false ? (
        <p role="alert">Couldn&apos;t copy here. Select the code and copy it yourself.</p>
      ) : null}
    </div>
  );
}
