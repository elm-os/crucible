import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { HomeOverview, HomeSkeleton } from "@/features/home/home-overview";

export const metadata: Metadata = { title: "Home" };

export default function HomePage() {
  return (
    <>
      <PageHeader
        title="Home"
        description="Headline numbers for the selected time range, and setup progress."
      />
      <Suspense fallback={<HomeSkeleton />}>
        <HomeOverview />
      </Suspense>
    </>
  );
}
