import type { Metadata } from "next";
import { OperationDetail } from "@/features/mcp/operation-detail";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP operation" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP operation"
      description="One MCP call, its captured content and the other side's view of it."
      active="operations"
      section="Operation"
    >
      <OperationDetail />
    </McpPageFrame>
  );
}
