import type { Metadata } from "next";
import { ErrorsSection } from "@/features/mcp/errors-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP errors" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP errors"
      description="Failed calls by error class and target, or grouped by message and code."
      active="errors"
      section="Error classes"
    >
      <ErrorsSection />
    </McpPageFrame>
  );
}
