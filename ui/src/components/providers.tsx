"use client";

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { type ReactNode, useState } from "react";
import { retryDelayMs, shouldRetry } from "@/api/errors";

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      // "always": the fetch decides reachability. The default pauses while the browser reports
      // offline, which would show endless loading for a server on the local network.
      queries: {
        staleTime: 15_000,
        networkMode: "always",
        retry: shouldRetry,
        retryDelay: retryDelayMs,
      },
      mutations: { networkMode: "always", retry: false },
    },
  });
}

export function Providers({ children }: { children: ReactNode }) {
  const [client] = useState(createQueryClient);
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}
