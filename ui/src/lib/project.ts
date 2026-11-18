"use client";

import { createLocalStorage } from "@ui-sdk/core";
import { useSearchParams } from "next/navigation";
import { useEffect, useSyncExternalStore } from "react";

// Stands in until the session endpoint lists the user's projects.
export const PLACEHOLDER_PROJECT = "default";

const lastProject = createLocalStorage("crucible.project");
const subscribe = (onChange: () => void) => lastProject.subscribe(onChange);
const noStoredValue = () => null;

export function useProject(): string {
  const fromUrl = useSearchParams().get("project");
  const stored = useSyncExternalStore(subscribe, lastProject.get, noStoredValue);
  useEffect(() => {
    if (fromUrl) lastProject.set(fromUrl);
  }, [fromUrl]);
  return fromUrl || stored || PLACEHOLDER_PROJECT;
}
