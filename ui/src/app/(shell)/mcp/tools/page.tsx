import type { Metadata } from "next";
import { ToolsSection } from "@/features/mcp/tools-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP tools" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP tools"
      description="Each tool's calls, error rate and latency, and the listed tools nobody called."
      active="tools"
      section="Tools"
    >
      <ToolsSection />
    </McpPageFrame>
  );
}
