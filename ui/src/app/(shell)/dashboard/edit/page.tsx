import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { DashboardEditor } from "@/features/dashboards/dashboard-editor";
import { BackToList } from "@/features/dashboards/dashboard-view";

export const metadata: Metadata = { title: "Edit dashboard" };

export default function DashboardEditPage() {
  return (
    <>
      <PageHeader
        title="Edit dashboard"
        description={
          <Suspense fallback={null}>
            <BackToList />
          </Suspense>
        }
      />
      <Suspense fallback={<SectionFrame title="Dashboard">{null}</SectionFrame>}>
        <DashboardEditor />
      </Suspense>
    </>
  );
}
