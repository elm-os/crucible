"use client";

import { createLocalStorage } from "@ui-sdk/core";
import { useSearchParams } from "next/navigation";
import { useEffect, useSyncExternalStore } from "react";
import { useConnectedProject } from "./connection";

// Stands in until the session endpoint lists the user's projects.
export const PLACEHOLDER_PROJECT = "default";

const lastProject = createLocalStorage("crucible.project");
const subscribe = (onChange: () => void) => lastProject.subscribe(onChange);
const noStoredValue = () => null;

// Precedence: the URL's `project`, then the project of the Connect dialog (temporary, milestone 1),
// then the project last opened in this browser.
export function useProject(): string {
  const fromUrl = useSearchParams().get("project");
  const connected = useConnectedProject();
  const stored = useSyncExternalStore(subscribe, lastProject.get, noStoredValue);
  useEffect(() => {
    if (fromUrl) lastProject.set(fromUrl);
  }, [fromUrl]);
  return fromUrl || connected || stored || PLACEHOLDER_PROJECT;
}
