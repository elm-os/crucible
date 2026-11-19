import type { Metadata } from "next";
import { Suspense } from "react";
import { PageHeader } from "@/components/page-header";
import { UserDetail } from "@/features/users/user-detail";

export const metadata: Metadata = { title: "User" };

export default function UserPage() {
  return (
    <Suspense fallback={<PageHeader title="User" />}>
      <UserDetail />
    </Suspense>
  );
}
