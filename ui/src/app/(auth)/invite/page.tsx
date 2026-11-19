import type { Metadata } from "next";
import { Suspense } from "react";
import { AuthHeading } from "@/features/auth/auth-heading";
import { InviteForm } from "@/features/auth/invite-form";

export const metadata: Metadata = { title: "Accept invitation" };

export default function InvitePage() {
  return (
    <Suspense fallback={<AuthHeading title="Join Crucible" />}>
      <InviteForm />
    </Suspense>
  );
}
