import { useSyncExternalStore } from "react";
import { vi } from "vitest";

// Stands in for next/navigation. Like the Next router, it re-renders search-param and pathname
// readers after history.replaceState.
const listeners = new Set<() => void>();
const replaceState = window.history.replaceState.bind(window.history);
window.history.replaceState = (data: unknown, unused: string, url?: string | URL | null) => {
  replaceState(data, unused, url);
  for (const listener of listeners) listener();
};

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function useSearchParams(): URLSearchParams {
  return new URLSearchParams(useSyncExternalStore(subscribe, () => window.location.search));
}

export function usePathname(): string {
  return useSyncExternalStore(subscribe, () => window.location.pathname);
}

export function useRouter() {
  return { push: vi.fn(), replace: vi.fn(), prefetch: vi.fn() };
}
