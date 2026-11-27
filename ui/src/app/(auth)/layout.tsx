import type { ReactNode } from "react";
import { Brand } from "@/components/brand";
import { AuthShowcase } from "@/features/auth/showcase";

// Pages a person uses before a session exists: sign in, first-run setup, invitations and resets. The page is the
// frame: two rounded panels fill it with one even gutter, the form on the left and, on wide screens, a picture of
// Crucible on the right.
export default function AuthLayout({ children }: { children: ReactNode }) {
  return (
    <main className="grid min-h-dvh gap-3 bg-parchment-white p-3 lg:grid-cols-[48fr_52fr]">
      <section className="flex min-w-0 flex-col rounded-[20px] border border-ash-border bg-parchment-white px-6 py-6 sm:px-12 sm:py-8">
        <p className="flex">
          <Brand />
        </p>
        <div className="flex flex-1 items-center justify-center py-10">
          <div className="flex w-full max-w-[440px] flex-col gap-6">{children}</div>
        </div>
      </section>
      <AuthShowcase />
    </main>
  );
}
