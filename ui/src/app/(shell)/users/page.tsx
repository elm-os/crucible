import type { Metadata } from "next";
import { ComingSoon } from "@/components/coming-soon";

export const metadata: Metadata = { title: "Users" };

export default function Page() {
  return (
    <ComingSoon
      title="Users"
      description="Users will list the people your agents served, with their traces, sessions, cost and errors."
    />
  );
}
