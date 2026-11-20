import type { Metadata } from "next";
import { OperationsSection } from "@/features/mcp/operations-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP operations" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP operations"
      description="Single MCP calls, newest first."
      active="operations"
      section="Operations"
    >
      <OperationsSection />
    </McpPageFrame>
  );
}
