import type { Metadata } from "next";
import { Suspense } from "react";
import { SectionFrame } from "@/components/section";
import { ProjectSettings } from "@/features/settings/project";

export const metadata: Metadata = { title: "Project settings" };

export default function ProjectSettingsPage() {
  return (
    <Suspense fallback={<SectionFrame title="Project details">{null}</SectionFrame>}>
      <ProjectSettings />
    </Suspense>
  );
}
