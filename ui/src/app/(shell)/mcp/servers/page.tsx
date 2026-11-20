import type { Metadata } from "next";
import { ServersSection } from "@/features/mcp/servers-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP servers" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP servers"
      description="Calls, error rate and latency of each MCP server in the selected time range."
      active="servers"
      section="Servers"
    >
      <ServersSection />
    </McpPageFrame>
  );
}
