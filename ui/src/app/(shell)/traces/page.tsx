import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { TracesSection } from "@/features/traces/traces-section";

export const metadata: Metadata = { title: "Traces" };

export default function TracesPage() {
  return (
    <>
      <PageHeader
        title="Traces"
        description="Root spans that started in the selected time range."
      />
      <Suspense fallback={<SectionFrame title="Recent traces">{null}</SectionFrame>}>
        <TracesSection />
      </Suspense>
    </>
  );
}
