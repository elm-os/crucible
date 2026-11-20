import type { Metadata } from "next";
import { SessionsSection } from "@/features/mcp/sessions-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP sessions" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP sessions"
      description="The MCP sessions in the selected time range, most recently seen first."
      active="sessions"
      section="Sessions"
    >
      <SessionsSection />
    </McpPageFrame>
  );
}
