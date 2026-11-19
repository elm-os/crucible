"use client";

import { createLocalStorage } from "@ui-sdk/core";
import { useSearchParams } from "next/navigation";
import { useEffect, useSyncExternalStore } from "react";
import { useAuth } from "./auth";
import { useConnectedProject } from "./connection";

// Stands in when no project is known: no project in the URL, none connected, none in the session.
export const PLACEHOLDER_PROJECT = "default";

const lastProject = createLocalStorage("crucible.project");
const subscribe = (onChange: () => void) => lastProject.subscribe(onChange);
const noStoredValue = () => null;

// Precedence: the URL's `project`. Signed in, then the project last opened in this browser if the
// session still lists it, else the session's first project. Without a control plane, the project of
// the Connect dialog (temporary, milestone 1), then the project last opened in this browser.
export function useProject(): string {
  const fromUrl = useSearchParams().get("project");
  const auth = useAuth();
  const connected = useConnectedProject();
  const stored = useSyncExternalStore(subscribe, lastProject.get, noStoredValue);
  useEffect(() => {
    if (fromUrl) lastProject.set(fromUrl);
  }, [fromUrl]);
  if (fromUrl) return fromUrl;
  if (auth.mode === "session") {
    const visible = auth.session.projects;
    return (
      (visible.find((project) => project.id === stored) ?? visible[0])?.id ?? PLACEHOLDER_PROJECT
    );
  }
  return connected || stored || PLACEHOLDER_PROJECT;
}
