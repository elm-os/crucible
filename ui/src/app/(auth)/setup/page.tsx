import type { Metadata } from "next";
import { Suspense } from "react";
import { AuthHeading } from "@/features/auth/auth-heading";
import { SetupForm } from "@/features/auth/setup-form";

export const metadata: Metadata = { title: "Set up" };

export default function SetupPage() {
  return (
    <Suspense fallback={<AuthHeading title="Set up Crucible" />}>
      <SetupForm />
    </Suspense>
  );
}
