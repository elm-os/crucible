"use client";

import { Tree, TreeItem, TreeItemLabel } from "@ui-sdk/react/tree";
import type { Route } from "next";
import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { Suspense } from "react";

export const NAV_ITEMS = [
  { value: "home", label: "Home", href: "/home" },
  { value: "traces", label: "Traces", href: "/traces" },
  { value: "sessions", label: "Sessions", href: "/sessions" },
  { value: "users", label: "Users", href: "/users" },
  { value: "search", label: "Search", href: "/search" },
  { value: "mcp", label: "MCP", href: "/mcp/servers" },
  { value: "dashboards", label: "Dashboards", href: "/dashboards" },
  { value: "alerts", label: "Alerts", href: "/alerts" },
  { value: "sql", label: "SQL", href: "/sql" },
  { value: "settings", label: "Settings", href: "/settings/project" },
] as const satisfies readonly { value: string; label: string; href: Route }[];

const DETAIL_PARENT: Record<string, string> = { trace: "traces" };

export function navValueFor(pathname: string): string | null {
  const segment = pathname.split("/")[1] ?? "";
  const area = DETAIL_PARENT[segment] ?? segment;
  return NAV_ITEMS.find((item) => item.href.split("/")[1] === area)?.value ?? null;
}

// Links keep the project and the time range, so moving between pages keeps the same scope.
export function scopedHref(href: Route, search: URLSearchParams | null): Route {
  const kept = new URLSearchParams();
  for (const key of ["project", "range"]) {
    const value = search?.get(key);
    if (value) kept.set(key, value);
  }
  const query = kept.toString();
  return (query ? `${href}?${query}` : href) as Route;
}

function NavTree({
  search,
  onNavigate,
}: {
  search: URLSearchParams | null;
  onNavigate?: () => void;
}) {
  const pathname = usePathname();
  return (
    <Tree aria-label="Pages" mode="none" current={navValueFor(pathname)} onAction={onNavigate}>
      {NAV_ITEMS.map((item) => (
        <TreeItem key={item.value} value={item.value}>
          <TreeItemLabel
            render={<Link href={scopedHref(item.href, search)} />}
            className="ps-2 text-data text-ink-soft data-current:text-midnight-ink"
          >
            {item.label}
          </TreeItemLabel>
        </TreeItem>
      ))}
    </Tree>
  );
}

function ScopedNavTree({ onNavigate }: { onNavigate?: () => void }) {
  return <NavTree search={useSearchParams()} onNavigate={onNavigate} />;
}

export function Nav({ onNavigate }: { onNavigate?: () => void }) {
  return (
    <nav aria-label="Main">
      <Suspense fallback={<NavTree search={null} onNavigate={onNavigate} />}>
        <ScopedNavTree onNavigate={onNavigate} />
      </Suspense>
    </nav>
  );
}
