import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { SpansSection } from "@/features/spans/spans-section";

export const metadata: Metadata = { title: "Spans" };

export default function SpansPage() {
  return (
    <>
      <PageHeader
        title="Spans"
        description="Every span that started in the selected time range, newest first."
      />
      <Suspense fallback={<SectionFrame title="Spans">{null}</SectionFrame>}>
        <SpansSection />
      </Suspense>
    </>
  );
}
