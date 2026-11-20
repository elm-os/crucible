import type { Metadata } from "next";
import { UsersSection } from "@/features/mcp/users-section";
import { McpPageFrame } from "@/features/mcp/mcp-page";

export const metadata: Metadata = { title: "MCP users" };

export default function Page() {
  return (
    <McpPageFrame
      title="MCP users"
      description="The users behind the calls, from the identify hook or the bearer token."
      active="users"
      section="Users"
    >
      <UsersSection />
    </McpPageFrame>
  );
}
