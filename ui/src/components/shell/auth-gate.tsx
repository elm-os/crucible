"use client";

import { useQuery } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { getSession, getSignInMode } from "@/api/client";
import { asUiError } from "@/api/errors";
import { ErrorCard, SectionFrame, useLoadingPhase } from "@/components/section";
import { type Auth, setAuth, useAuth } from "@/lib/auth";

async function checkAuth(signal: AbortSignal): Promise<Auth> {
  const server = await getSignInMode(signal);
  const auth: Auth =
    server.mode === "keys"
      ? { mode: "keys" }
      : { mode: "session", session: await getSession(signal) };
  setAuth(auth);
  return auth;
}

// Pages render once the server's sign-in mode is known and, with a control plane, the session is
// loaded: every request and the default project depend on it. A missing session goes to sign in.
export function AuthGate({ children }: { children: ReactNode }) {
  const auth = useAuth();
  const check = useQuery({
    queryKey: ["auth"],
    queryFn: ({ signal }) => checkAuth(signal),
    enabled: auth.mode === "unknown",
    staleTime: Number.POSITIVE_INFINITY,
  });
  const phase = useLoadingPhase(auth.mode === "unknown" && !check.isError);

  if (auth.mode !== "unknown") return children;
  if (check.isError) {
    return (
      <SectionFrame title="Connection">
        <ErrorCard
          error={asUiError(check.error)}
          label="connection"
          onRetry={() => check.refetch()}
        />
      </SectionFrame>
    );
  }
  return (
    <output className="text-data text-ink-soft">
      {phase === "hidden" ? "" : "Connecting to Crucible"}
    </output>
  );
}
