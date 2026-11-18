"use client";

import { createSessionStorage } from "@ui-sdk/core";
import { useSyncExternalStore } from "react";

// Temporary, for the milestone 1 development preview: the UI has no browser sessions yet, so the
// Connect dialog keeps a project id and its secret key for this tab only. sessionStorage, never
// localStorage: the key is gone when the tab closes. Sessions replace this file in milestone 4.
const project = createSessionStorage("crucible.connection.project");
const key = createSessionStorage("crucible.connection.key");

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
