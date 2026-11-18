import type { Metadata } from "next";
import { ComingSoon } from "@/components/coming-soon";

export const metadata: Metadata = { title: "Alerts" };

export default function Page() {
  return (
    <ComingSoon
      title="Alerts"
      description="Alerts will watch a metric and notify Slack, email or a webhook when it crosses a threshold."
    />
  );
}
