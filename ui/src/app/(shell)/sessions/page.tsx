import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { SessionsSection } from "@/features/sessions/sessions-section";

export const metadata: Metadata = { title: "Sessions" };

export default function SessionsPage() {
  return (
    <>
      <PageHeader
        title="Sessions"
        description="Traces grouped by the session id their spans carry, in the selected time range."
      />
      <Suspense fallback={<SectionFrame title="Sessions">{null}</SectionFrame>}>
        <SessionsSection />
      </Suspense>
    </>
  );
}
