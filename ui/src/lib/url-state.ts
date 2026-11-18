"use client";

import { useSearchParams } from "next/navigation";
import { useState } from "react";

// One search param as state. Changes replace the history entry. The router applies replaceState in a
// transition, so the choice is held here until the URL has it; otherwise readers would see the old
// value for a render.
export function useUrlParam(name: string): [string | null, (next: string | null) => void] {
  const fromUrl = useSearchParams().get(name);
  const [chosen, setChosen] = useState<{ value: string | null } | null>(null);
  if (chosen !== null && chosen.value === fromUrl) setChosen(null);

  const set = (next: string | null) => {
    setChosen({ value: next });
    const params = new URLSearchParams(window.location.search);
    if (next === null) params.delete(name);
    else params.set(name, next);
    window.history.replaceState(null, "", `?${params}`);
  };

  return [chosen === null ? fromUrl : chosen.value, set];
}
