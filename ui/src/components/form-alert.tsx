"use client";

import { type ReactNode, useEffect, useRef } from "react";
import type { UiError } from "@/api/errors";
import { ErrorCode } from "./error-code";
import { AlertIcon } from "./icons";

export interface Copy {
  title: string;
  detail: string;
  /** The request id of an unexpected failure, which an administrator looks up in the server log. */
  reference?: string | null;
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
  precondition_failed: {
    title: "Someone changed this since the page loaded.",
    detail: "Reload the page to see the change, then try again.",
  },
  idempotency_key_reused: {
    title: "An earlier attempt may have gone through.",
    detail: "Close this and check the list before you send it again.",
  },
  feature_disabled: {
    title: "This is turned off on this server.",
    detail: "Ask an administrator of this Crucible server to turn it on.",
  },
  payload_too_large: {
    title: "That's more than Crucible takes at once.",
    detail: "Make it shorter, then try again.",
  },
};

function waitCopy(seconds: number | null): string {
  if (seconds === null) return "Wait a minute, then try again.";
  return `Wait ${seconds} ${seconds === 1 ? "second" : "seconds"}, then try again.`;
}

// What a failed form says: its own copy for the codes it expects, then the shared copy, then words for the
// kind of failure. Server text never reaches the page; only an unexpected failure brings its reference.
export function failureCopy(error: UiError, failed: string, byCode: CopyByCode = {}): Copy {
  return { ...words(error, failed, byCode), reference: error.reference };
}

function words(error: UiError, failed: string, byCode: CopyByCode): Copy {
  const code = error.code ?? "";
  if (Object.hasOwn(byCode, code)) return byCode[code];
  if (Object.hasOwn(SHARED, code)) return SHARED[code];
  switch (error.kind) {
    case "network":
    case "signed_out":
    case "too_heavy":
    case "too_broad":
      return { title: error.title, detail: error.detail };
    case "rate_limited":
      return { title: "Too many attempts.", detail: waitCopy(error.retryAfterS) };
    case "invalid":
      // A 409 is a clash with the stored state, not a field to correct.
      return error.status === 409
        ? { title: failed, detail: "Reload the page to see the latest version, then try again." }
        : {
            title: "Some fields need attention.",
            detail: "Check what you entered, then try again.",
          };
    case "forbidden":
      return { title: "Your role can't do this.", detail: "Ask an organization owner or admin." };
    case "not_found":
      return {
        title: "This isn't there anymore.",
        detail: "Someone may have deleted it. Reload the page to see what's there now.",
      };
    default:
      return { title: failed, detail: "Try again in a moment." };
  }
}

// Whether the server rejected the field `name` (its errors point at `/<name>`).
export function fieldFailed(error: UiError | null, name: string): boolean {
  return error?.fieldErrors.some((field) => field.pointer === `/${name}`) ?? false;
}

// For a form that checks its own fields on submit: focus moves to the first one it found invalid.
export function focusField(form: HTMLFormElement, name: string): void {
  const control = form.elements.namedItem(name);
  if (control instanceof HTMLElement) control.focus();
}

export function FormAlert({ copy, action }: { copy: Copy; action?: ReactNode }) {
  const alert = useRef<HTMLDivElement>(null);
  // Appearing after a submit, it takes focus to the first field the server rejected, so keyboard and
  // screen reader users land on what to fix. Outside a form, or with no field rejected, it moves nothing.
  useEffect(() => {
    alert.current?.closest("form")?.querySelector<HTMLElement>('[aria-invalid="true"]')?.focus();
  }, []);
  return (
    <div ref={alert} role="alert" className="flex items-start gap-2 text-data">
      <AlertIcon className="mt-0.5 shrink-0" />
      <div className="flex min-w-0 flex-col items-start gap-0.5">
        <p className="font-medium">{copy.title}</p>
        <p className="text-ink-soft">{copy.detail}</p>
        {copy.reference ? (
          <div className="mt-1">
            <ErrorCode code={copy.reference} />
          </div>
        ) : null}
        {action ? <div className="mt-1">{action}</div> : null}
      </div>
    </div>
  );
}
