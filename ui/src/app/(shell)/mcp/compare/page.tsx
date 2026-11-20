import type { Metadata } from "next";
import { CompareSection } from "@/features/mcp/compare-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "Compare deploys" };

export default function Page() {
  return (
    <McpPageFrame
      title="Compare deploys"
      description="Two deploys of one server, tool by tool."
      active="compare"
      section="Comparison"
    >
      <CompareSection />
    </McpPageFrame>
  );
}
