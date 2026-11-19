import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { SectionFrame } from "@/components/section";
import { UsersSection } from "@/features/users/users-section";

export const metadata: Metadata = { title: "Users" };

export default function UsersPage() {
  return (
    <>
      <PageHeader
        title="Users"
        description="The people or accounts your agents served, by the user id their spans carry."
      />
      <Suspense fallback={<SectionFrame title="Users">{null}</SectionFrame>}>
        <UsersSection />
      </Suspense>
    </>
  );
}
