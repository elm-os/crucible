import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { SqlWorkspace } from "@/features/sql/sql-workspace";

export const metadata: Metadata = { title: "SQL" };

export default function SqlPage() {
  return (
    <>
      <PageHeader
        title="SQL"
        description="Read-only queries over this project's telemetry, in the selected time range."
      />
      <Suspense fallback={<SectionFrame title="Query">{null}</SectionFrame>}>
        <SqlWorkspace />
      </Suspense>
    </>
  );
}
