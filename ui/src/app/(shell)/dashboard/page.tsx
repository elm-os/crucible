import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { BackToList, DashboardView } from "@/features/dashboards/dashboard-view";

export const metadata: Metadata = { title: "Dashboard" };

export default function DashboardPage() {
  return (
    <>
      <PageHeader
        title="Dashboard"
        description={
          <Suspense fallback={null}>
            <BackToList />
          </Suspense>
        }
      />
      <Suspense fallback={<SectionFrame title="Dashboard">{null}</SectionFrame>}>
        <DashboardView />
      </Suspense>
    </>
  );
}
