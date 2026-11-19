import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SessionDetail } from "@/features/sessions/session-detail";

export const metadata: Metadata = { title: "Session" };

export default function SessionPage() {
  return (
    <Suspense fallback={<PageHeader title="Session" />}>
      <SessionDetail />
    </Suspense>
  );
}
