"use client";

import { useSyncExternalStore } from "react";

const WIDE = "(min-width: 48rem)";

function subscribeWide(onChange: () => void): () => void {
  const query = window.matchMedia(WIDE);
  query.addEventListener("change", onChange);
  return () => query.removeEventListener("change", onChange);
}

// Editors need a screen at least 768 pixels wide; phones read.
export function useWide(): boolean {
  return useSyncExternalStore(
    subscribeWide,
    () => window.matchMedia(WIDE).matches,
    () => true,
  );
}
