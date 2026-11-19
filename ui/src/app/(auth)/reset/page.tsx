import type { Metadata } from "next";
import { Suspense } from "react";
import { AuthHeading } from "@/features/auth/auth-heading";
import { ResetForm } from "@/features/auth/reset-form";

export const metadata: Metadata = { title: "Reset password" };

export default function ResetPage() {
  return (
    <Suspense fallback={<AuthHeading title="Reset your password" />}>
      <ResetForm />
    </Suspense>
  );
}
