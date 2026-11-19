import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { AlertRules } from "@/features/alerts/rules";
import { AlertsNav } from "@/features/alerts/shared";

export const metadata: Metadata = { title: "Alerts" };

export default function AlertsPage() {
  return (
    <>
      <PageHeader
        title="Alerts"
        description="Rules that watch a measure and notify a channel when it crosses a threshold."
      />
      <AlertsNav />
      <Suspense fallback={<SectionFrame title="Alert rules">{null}</SectionFrame>}>
        <AlertRules />
      </Suspense>
    </>
  );
}
