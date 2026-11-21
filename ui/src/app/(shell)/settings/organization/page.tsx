import type { Metadata } from "next";
import { OrganizationSettings } from "@/features/settings/organization";

export const metadata: Metadata = { title: "Organization" };

export default function OrganizationPage() {
  return <OrganizationSettings />;
}
