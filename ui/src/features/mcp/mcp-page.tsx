import { type ReactNode, Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { McpBar, type McpPage } from "./mcp-bar";

// Every MCP page: its header, the MCP pages and scope controls, and its sections, which read the URL.
export function McpPageFrame({
  title,
  description,
  active,
  section,
  children,
}: {
  title: string;
  description: string;
  active: McpPage | null;
  /** The title of the first section, shown while the URL is read. */
  section: string;
  children: ReactNode;
}) {
  return (
    <>
      <PageHeader title={title} description={description} />
      <Suspense fallback={<SectionFrame title={section}>{null}</SectionFrame>}>
        <McpBar active={active} />
        {children}
      </Suspense>
    </>
  );
}
