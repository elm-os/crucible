import type { Metadata } from "next";
import { ComingSoon } from "@/components/coming-soon";

export const metadata: Metadata = { title: "MCP" };

export default function Page() {
  return (
    <ComingSoon
      title="MCP"
      description="MCP analytics will show calls, error rates and latency for each server, tool, client and session."
    />
  );
}
