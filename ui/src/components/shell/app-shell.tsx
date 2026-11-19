import Link from "next/link";
import type { ReactNode } from "react";
import { AccountControls, ConnectControl } from "./account";
import { AuthGate } from "./auth-gate";
import { MobileNav } from "./mobile-nav";
import { Nav } from "./nav";
import { ProjectSwitcher } from "./project-switcher";
import { TimeRangeControl } from "./time-range-control";

export function AppShell({ children }: { children: ReactNode }) {
  return (
    <div className="flex min-h-dvh">
      <aside className="sticky top-0 hidden h-dvh w-56 shrink-0 flex-col gap-4 border-e border-ash-border px-3 py-4 md:flex">
        <Link href="/home" className="wordmark px-2 py-1 no-underline">
          Crucible
        </Link>
        <Nav />
      </aside>
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-ash-border px-4 py-3 md:px-8">
          <MobileNav />
          <ProjectSwitcher />
          <ConnectControl />
          <div className="ms-auto flex flex-wrap items-center gap-x-3 gap-y-2">
            <TimeRangeControl />
            <AccountControls />
          </div>
        </header>
        <main className="flex-1 px-4 py-6 md:px-8">
          <AuthGate>{children}</AuthGate>
        </main>
      </div>
    </div>
  );
}
