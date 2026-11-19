import type { ReactNode } from "react";

// Pages a person uses before a session exists: sign in, first-run setup, invitations and resets.
export default function AuthLayout({ children }: { children: ReactNode }) {
  return (
    <main className="grid min-h-dvh place-items-center px-4 py-10">
      <div className="flex w-full max-w-sm flex-col gap-6">
        <p className="wordmark">Crucible</p>
        {children}
      </div>
    </main>
  );
}
