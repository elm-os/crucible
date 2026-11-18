import type { Metadata } from "next";
import { ComingSoon } from "@/components/coming-soon";

export const metadata: Metadata = { title: "Dashboards" };

export default function Page() {
  return (
    <ComingSoon
      title="Dashboards"
      description="Dashboards will hold charts built from metrics queries, with starter dashboards to clone."
    />
  );
}
