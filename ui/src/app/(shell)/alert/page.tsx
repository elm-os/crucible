import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { AlertRuleDetail, BackToRules } from "@/features/alerts/rule-detail";

export const metadata: Metadata = { title: "Alert rule" };

export default function AlertPage() {
  return (
    <>
      <PageHeader
        title="Alert rule"
        description={
          <Suspense fallback={null}>
            <BackToRules />
          </Suspense>
        }
      />
      <Suspense fallback={<SectionFrame title="Rule">{null}</SectionFrame>}>
        <AlertRuleDetail />
      </Suspense>
    </>
  );
}
