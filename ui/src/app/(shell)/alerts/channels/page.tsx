import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { ChannelsSettings } from "@/features/alerts/channels";
import { AlertsNav } from "@/features/alerts/shared";

export const metadata: Metadata = { title: "Alert channels" };

export default function ChannelsPage() {
  return (
    <>
      <PageHeader
        title="Alerts"
        description="Rules that watch a measure and notify a channel when it crosses a threshold."
      />
      <AlertsNav />
      <Suspense fallback={<SectionFrame title="Channels">{null}</SectionFrame>}>
        <ChannelsSettings />
      </Suspense>
    </>
  );
}
