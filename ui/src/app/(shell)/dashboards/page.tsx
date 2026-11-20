import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { DashboardList } from "@/features/dashboards/dashboard-list";

export const metadata: Metadata = { title: "Dashboards" };

export default function DashboardsPage() {
  return (
    <>
      <PageHeader
        title="Dashboards"
        description="Charts, numbers and notes over this project's metrics, for the time range above."
      />
      <Suspense fallback={<SectionFrame title="Dashboards">{null}</SectionFrame>}>
        <DashboardList />
      </Suspense>
    </>
  );
}
