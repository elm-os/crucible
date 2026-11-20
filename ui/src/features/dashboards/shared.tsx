"use client";

import { useQuery } from "@tanstack/react-query";
import type { Route } from "next";
import { useSyncExternalStore } from "react";
import { getMetricsSchema } from "@/api/client";
import { useAuth } from "@/lib/auth";

export const dashboardKeys = {
  list: (project: string) => [project, "dashboards"] as const,
  one: (project: string, id: string) => [project, "dashboards", id] as const,
  schema: (project: string) => [project, "metrics-schema"] as const,
};

// Who may do what here. Without a control plane, the Connect dialog's key reads the built-in dashboards;
// changing anything needs a session whose project role edits content.
export function useDashboardAccess(project: string): { signedIn: boolean; canEdit: boolean } {
  const auth = useAuth();
  if (auth.mode !== "session") return { signedIn: false, canEdit: false };
  const access = auth.session.projects.find((visible) => visible.id === project);
  return { signedIn: true, canEdit: access?.permissions.includes("edit_content") ?? false };
}

// The metrics schema names measures and lists what a widget may ask; it changes only with the server.
export function useMetricsSchema(project: string) {
  return useQuery({
    queryKey: dashboardKeys.schema(project),
    queryFn: ({ signal }) => getMetricsSchema(project, signal),
    staleTime: 5 * 60_000,
  });
}

function withScope(path: string, search: URLSearchParams | null, id: string): Route {
  const params = new URLSearchParams();
  for (const key of ["project", "range"]) {
    const value = search?.get(key);
    if (value) params.set(key, value);
  }
  params.set("id", id);
  return `${path}?${params}` as Route;
}

export function dashboardHref(id: string, search: URLSearchParams | null): Route {
  return withScope("/dashboard/", search, id);
}

export function editorHref(id: string, search: URLSearchParams | null): Route {
  return withScope("/dashboard/edit/", search, id);
}

const WIDE = "(min-width: 48rem)";

function subscribeWide(onChange: () => void): () => void {
  const query = window.matchMedia(WIDE);
  query.addEventListener("change", onChange);
  return () => query.removeEventListener("change", onChange);
}

// Editing is for screens with room for the grid; phones read dashboards.
export function useWide(): boolean {
  return useSyncExternalStore(
    subscribeWide,
    () => window.matchMedia(WIDE).matches,
    () => true,
  );
}
