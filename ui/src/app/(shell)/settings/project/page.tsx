import type { Metadata } from "next";
import { EmptyState } from "@/components/empty-state";
import { SectionFrame } from "@/components/section";

export const metadata: Metadata = { title: "Project settings" };

export default function Page() {
  return (
    <SectionFrame title="About project settings">
      <EmptyState kind="planned" title="Project settings are not built yet.">
        Project settings will manage the project&apos;s name, retention and model prices.
      </EmptyState>
    </SectionFrame>
  );
}
