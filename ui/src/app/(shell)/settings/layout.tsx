import type { ReactNode } from "react";
import { PageHeader } from "@/components/page-header";
import { NewProjectControl } from "@/features/settings/new-project";
import { SettingsNav } from "@/features/settings/settings-nav";

export default function SettingsLayout({ children }: { children: ReactNode }) {
  return (
    <>
      <PageHeader title="Settings" />
      <div className="mb-6 flex flex-wrap items-end justify-between gap-x-4 border-b border-ash-border">
        <SettingsNav />
        <NewProjectControl />
      </div>
      {children}
    </>
  );
}
