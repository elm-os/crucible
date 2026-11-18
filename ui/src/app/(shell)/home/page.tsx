import type { Metadata } from "next";
import { PageHeader } from "@/components/page-header";
import { HomeOverview } from "@/features/home/home-overview";

export const metadata: Metadata = { title: "Home" };

export default function HomePage() {
  return (
    <>
      <PageHeader
        title="Home"
        description="Setup progress and headline numbers for this project."
      />
      <HomeOverview />
    </>
  );
}
