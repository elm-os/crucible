"use client";

import { createLocalStorage } from "@ui-sdk/core";
import { useSyncExternalStore } from "react";

// A server without a control plane (keys-file mode) has no browser sessions, so the Connect
// dialog keeps a project id and its key in this browser until Disconnect, so a new tab or a
// restart does not ask again.
const project = createLocalStorage("crucible.connection.project");
const key = createLocalStorage("crucible.connection.key");

export interface Connection {
  project: string;
  key: string;
}

export function saveConnection(connection: Connection): void {
  key.set(connection.key);
  project.set(connection.project);
}

export function clearConnection(): void {
  key.remove();
  project.remove();
}

export function connectionKey(): string | null {
  return key.get();
}

const subscribe = (onChange: () => void) => project.subscribe(onChange);
const notConnected = () => null;

export function useConnectedProject(): string | null {
  return useSyncExternalStore(subscribe, project.get, notConnected);
}
