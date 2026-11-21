import type { Metadata } from "next";
import { SignInSettings } from "@/features/settings/sign-in-providers";

export const metadata: Metadata = { title: "Sign-in providers" };

export default function SignInPage() {
  return <SignInSettings />;
}
