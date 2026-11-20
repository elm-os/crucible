import type { Metadata } from "next";
import { DeploysSection } from "@/features/mcp/deploys-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP deploys" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP deploys"
      description="The deploys of each server seen in the selected time range, newest first."
      active="deploys"
      section="Deploys"
    >
      <DeploysSection />
    </McpPageFrame>
  );
}
