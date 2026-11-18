import type { ReactNode } from "react";
import { PageHeader } from "@/components/page-header";
import { SettingsNav } from "@/features/settings/settings-nav";

export default function SettingsLayout({ children }: { children: ReactNode }) {
  return (
    <>
      <PageHeader title="Settings" />
      <SettingsNav />
      {children}
    </>
  );
}
