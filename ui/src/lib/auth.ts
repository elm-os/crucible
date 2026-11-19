"use client";

import { useSyncExternalStore } from "react";
import type { SessionInfo } from "@/api/generated/SessionInfo";

// How this browser proves who it is: a signed-in session when the server has a control plane, or
// the Connect dialog's project key when it has none. The shell finds out once per page load; the
// fetch layer reads the answer synchronously.
export type Auth =
  | { mode: "unknown" }
  | { mode: "keys" }
  | { mode: "session"; session: SessionInfo };

const UNKNOWN: Auth = { mode: "unknown" };
let current: Auth = UNKNOWN;
const listeners = new Set<() => void>();

export function currentAuth(): Auth {
  return current;
}

export function setAuth(next: Auth): void {
  current = next;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

const unknownOnServer = () => UNKNOWN;

export function useAuth(): Auth {
  return useSyncExternalStore(subscribe, currentAuth, unknownOnServer);
}
