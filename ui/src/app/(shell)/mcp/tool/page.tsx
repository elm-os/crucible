import type { Metadata } from "next";
import { ToolDetail } from "@/features/mcp/tool-detail";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP tool" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP tool"
      description="One tool's calls, error classes and latency over time, with its servers' deploys marked."
      active="tools"
      section="Summary"
    >
      <ToolDetail />
    </McpPageFrame>
  );
}
