"use client";

import type { Route } from "next";
import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { Suspense } from "react";
import { scopedHref } from "@/components/shell/nav";

const PAGES = [
  { label: "Project", href: "/settings/project" },
  { label: "Members", href: "/settings/members" },
  { label: "API keys", href: "/settings/api-keys" },
  { label: "Exports", href: "/settings/exports" },
  { label: "Retention", href: "/settings/retention" },
  { label: "Organization", href: "/settings/organization" },
  { label: "Sign-in", href: "/settings/sign-in" },
] as const satisfies readonly { label: string; href: Route }[];

function Links({ search }: { search: URLSearchParams | null }) {
  const pathname = usePathname().replace(/\/$/, "");
  return (
    <ul className="flex flex-wrap gap-x-5 gap-y-1">
      {PAGES.map((page) => (
        <li key={page.href}>
          <Link
            href={scopedHref(page.href, search)}
            aria-current={pathname === page.href ? "page" : undefined}
            className="block border-b-2 border-transparent pb-2 text-data text-ink-soft no-underline aria-[current=page]:border-midnight-ink aria-[current=page]:font-medium aria-[current=page]:text-midnight-ink"
          >
            {page.label}
          </Link>
        </li>
      ))}
    </ul>
  );
}

function ScopedLinks() {
  return <Links search={useSearchParams()} />;
}

export function SettingsNav() {
  return (
    <nav aria-label="Settings">
      <Suspense fallback={<Links search={null} />}>
        <ScopedLinks />
      </Suspense>
    </nav>
  );
}
