import type { Metadata } from "next";
import { ComingSoon } from "@/components/coming-soon";

export const metadata: Metadata = { title: "Search" };

export default function Page() {
  return (
    <ComingSoon
      title="Search"
      description="Search will find spans by the text of their inputs, outputs, metadata and names."
    />
  );
}
