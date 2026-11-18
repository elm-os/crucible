import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { TraceDetail } from "@/features/trace/trace-detail";

export const metadata: Metadata = { title: "Trace" };

export default function TracePage() {
  return (
    <Suspense fallback={<PageHeader title="Trace" />}>
      <TraceDetail />
    </Suspense>
  );
}
