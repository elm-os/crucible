import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SearchView } from "@/features/search/search-view";

export const metadata: Metadata = { title: "Search" };

export default function SearchPage() {
  return (
    <Suspense fallback={<PageHeader title="Search" />}>
      <SearchView />
    </Suspense>
  );
}
