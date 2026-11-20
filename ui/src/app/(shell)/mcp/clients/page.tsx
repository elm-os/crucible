import type { Metadata } from "next";
import { ClientsSection } from "@/features/mcp/clients-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP clients" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP clients"
      description="The clients that called your servers, by the name and version they reported."
      active="clients"
      section="Clients"
    >
      <ClientsSection />
    </McpPageFrame>
  );
}
