import type { Metadata } from "next";
import { MembersSettings } from "@/features/settings/members";

export const metadata: Metadata = { title: "Members" };

export default function MembersPage() {
  return <MembersSettings />;
}
