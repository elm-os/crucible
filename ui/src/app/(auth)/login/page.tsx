import type { Metadata } from "next";
import { Suspense } from "react";
import { AuthHeading } from "@/features/auth/auth-heading";
import { LoginForm } from "@/features/auth/login-form";

export const metadata: Metadata = { title: "Sign in" };

export default function LoginPage() {
  return (
    <Suspense fallback={<AuthHeading title="Sign in to Crucible" />}>
      <LoginForm />
    </Suspense>
  );
}
