import type { Metadata } from "next";
import { ComingSoon } from "@/components/coming-soon";

export const metadata: Metadata = { title: "SQL" };

export default function Page() {
  return (
    <ComingSoon
      title="SQL"
      description="The SQL workspace will run read-only queries over this project's telemetry, with saved queries and export."
    />
  );
}
