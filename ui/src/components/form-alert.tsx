import type { ReactNode } from "react";
import type { UiError } from "@/api/errors";
import { AlertIcon } from "./icons";

export interface Copy {
  title: string;
  detail: string;
}

export type CopyByCode = Readonly<Record<string, Copy>>;

const SHARED: CopyByCode = {
  origin_invalid: {
    title: "Crucible refused a request from this address.",
    detail:
      "Open Crucible at its configured address, or ask an administrator to add this one to server.allowed_origins.",
  },
  csrf_invalid: {
    title: "This page is out of date.",
    detail: "Reload the page, then try again.",
  },
  control_plane_unavailable: {
    title: "Crucible can't reach its database.",
    detail: "Try again in a moment.",
  },
};

function waitCopy(seconds: number | null): string {
  if (seconds === null) return "Wait a minute, then try again.";
  return `Wait ${seconds} ${seconds === 1 ? "second" : "seconds"}, then try again.`;
}

// What a failed form says: the form's own copy for the codes it expects, then copy every form shares.
// Server text never reaches the page.
export function failureCopy(error: UiError, failed: string, byCode: CopyByCode = {}): Copy {
  const code = error.code ?? "";
  if (Object.hasOwn(byCode, code)) return byCode[code];
  if (Object.hasOwn(SHARED, code)) return SHARED[code];
  switch (error.kind) {
    case "network":
      return { title: error.title, detail: error.detail };
    case "rate_limited":
      return { title: "Too many attempts.", detail: waitCopy(error.retryAfterS) };
    case "invalid":
      return {
        title: "Some fields need attention.",
        detail: "Check what you entered, then try again.",
      };
    case "forbidden":
      return { title: "Your role can't do this.", detail: "Ask an organization owner or admin." };
    default:
      return { title: failed, detail: "Try again in a moment." };
  }
}

// Whether the server rejected the field `name` (its errors point at `/<name>`).
export function fieldFailed(error: UiError | null, name: string): boolean {
  return error?.fieldErrors.some((field) => field.pointer === `/${name}`) ?? false;
}

export function FormAlert({
  copy,
  requestId,
  action,
}: {
  copy: Copy;
  requestId?: string | null;
  action?: ReactNode;
}) {
  return (
    <div role="alert" className="flex items-start gap-2 text-data">
      <AlertIcon className="mt-0.5 shrink-0" />
      <div className="flex min-w-0 flex-col items-start gap-0.5">
        <p className="font-medium">{copy.title}</p>
        <p className="text-ink-soft">{copy.detail}</p>
        {requestId ? (
          <p className="text-label text-ink-soft">
            Reference <span className="font-mono select-all">{requestId}</span>
          </p>
        ) : null}
        {action ? <div className="mt-1">{action}</div> : null}
      </div>
    </div>
  );
}
