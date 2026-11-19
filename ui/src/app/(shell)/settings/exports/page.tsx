import type { Metadata } from "next";
import { Suspense } from "react";
import { SectionFrame } from "@/components/section";
import { ExportsSettings } from "@/features/exports/exports";

export const metadata: Metadata = { title: "Exports" };

export default function ExportsPage() {
  return (
    <Suspense fallback={<SectionFrame title="Exports">{null}</SectionFrame>}>
      <ExportsSettings />
    </Suspense>
  );
}
