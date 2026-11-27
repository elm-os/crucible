"use client";

import { Button } from "@ui-sdk/react/button";
import { useState } from "react";

// Beside an unexpected failure only: the request id that finds it in the server log. The browser has no
// clipboard API on plain-HTTP origins, so the code also selects with one click and the button says when
// copying fails. `label` names what failed, so several codes on one page keep buttons of their own.
export function ErrorCode({ code, label }: { code: string | null; label?: string }) {
  const [copied, setCopied] = useState<boolean | null>(null);
  if (code === null) return null;
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code);
      setCopied(true);
    } catch {
      setCopied(false);
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
        <output>{copied ? "Copied." : ""}</output>
      </div>
      {copied === false ? (
        <p role="alert">Couldn&apos;t copy here. Select the code and copy it yourself.</p>
      ) : null}
    </div>
  );
}
