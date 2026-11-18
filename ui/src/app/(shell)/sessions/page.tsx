import type { Metadata } from "next";
import { ComingSoon } from "@/components/coming-soon";

export const metadata: Metadata = { title: "Sessions" };

export default function Page() {
  return (
    <ComingSoon
      title="Sessions"
      description="Sessions will group the traces of one conversation or run, with duration, cost and token totals."
    />
  );
}
