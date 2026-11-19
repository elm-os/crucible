"use client";

import type { Route } from "next";
import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { type ReactNode, Suspense } from "react";
import type { AlertStatus } from "@/api/generated/AlertStatus";
import type { SessionProject } from "@/api/generated/SessionProject";
import { EmptyState } from "@/components/empty-state";
import { AlertIcon, CheckIcon, UnsetIcon } from "@/components/icons";
import { SectionFrame } from "@/components/section";
import { scopedHref } from "@/components/shell/nav";
import { useAuth } from "@/lib/auth";
import { useProject } from "@/lib/project";
import { STATUS_LABEL } from "./format";

export const alertKeys = {
  rules: (project: string) => [project, "alerts"] as const,
  rule: (project: string, id: string) => [project, "alerts", id] as const,
  history: (project: string, id: string) => [project, "alerts", id, "history"] as const,
  channels: (project: string) => [project, "channels"] as const,
};

const ICONS = {
  firing: AlertIcon,
  no_data: AlertIcon,
  pending: UnsetIcon,
  resolved: CheckIcon,
  ok: CheckIcon,
} as const;

// Status is never carried by color: each state has an icon, and the label says it in words.
export function AlertStatusLabel({ status }: { status: AlertStatus }) {
  const Icon = ICONS[status];
  return (
    <span data-status={status} className="flex items-center gap-1.5 whitespace-nowrap">
      <Icon className="shrink-0" />
      {STATUS_LABEL[status]}
    </span>
  );
}

// Alerts live in the control plane: without one, or without a project this session can see, the page says
// which setting or person changes that, and asks the server for nothing.
export function AlertsAccess({
  title,
  children,
}: {
  title: string;
  children: (project: SessionProject) => ReactNode;
}) {
  const auth = useAuth();
  const project = useProject();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title={title}>
        <EmptyState kind="unavailable" title="Alerts need a control plane.">
          Rules and channels are stored in Postgres. Set postgres.url (CRUCIBLE_POSTGRES_URL) and
          the master key, then sign in.
        </EmptyState>
      </SectionFrame>
    );
  }
  const access = auth.session.projects.find((visible) => visible.id === project);
  if (access === undefined) {
    return (
      <SectionFrame title={title}>
        <EmptyState kind="unavailable" title="This project isn't available to you.">
          Choose one of your projects in the project menu.
        </EmptyState>
      </SectionFrame>
    );
  }
  return children(access);
}

const PAGES = [
  { label: "Rules", href: "/alerts" },
  { label: "Channels", href: "/alerts/channels" },
] as const satisfies readonly { label: string; href: Route }[];

function Links({ search }: { search: URLSearchParams | null }) {
  const pathname = usePathname().replace(/\/$/, "");
  return (
    <ul className="flex gap-5">
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

export function AlertsNav() {
  return (
    <nav aria-label="Alerts" className="mb-6 border-b border-ash-border">
      <Suspense fallback={<Links search={null} />}>
        <ScopedLinks />
      </Suspense>
    </nav>
  );
}
