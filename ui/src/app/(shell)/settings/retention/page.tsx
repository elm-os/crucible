import type { Metadata } from "next";
import { Suspense } from "react";
import { SectionFrame } from "@/components/section";
import { RetentionSettings } from "@/features/settings/retention";

export const metadata: Metadata = { title: "Retention" };

export default function RetentionPage() {
  return (
    <Suspense fallback={<SectionFrame title="Retention">{null}</SectionFrame>}>
      <RetentionSettings />
    </Suspense>
  );
}
