"use client";

import type { Route } from "next";
import { useSearchParams } from "next/navigation";
import type { McpScope } from "@/api/client";
import type { McpSideParam } from "@/api/generated/McpSideParam";
import type { SourceParam } from "@/api/generated/SourceParam";
import { useUrlParam } from "@/lib/url-state";

export const SOURCES: { value: SourceParam; label: string }[] = [
  { value: "auto", label: "Automatic" },
  { value: "sdk", label: "SDK" },
  { value: "proxy", label: "Proxy" },
  { value: "otlp", label: "OTLP" },
];

export function parseSide(value: string | null): McpSideParam {
  return value === "client" ? "client" : "server";
}

export function parseSource(value: string | null): SourceParam {
  return SOURCES.find((source) => source.value === value)?.value ?? "auto";
}

// The side, capture source, caller-fault rule and server of every MCP page live in the URL, so a link
// carries them and Back restores them.
export function useMcpScope(): [McpScope, (next: Partial<McpScope>) => void] {
  const [side, setSide] = useUrlParam("side");
  const [source, setSource] = useUrlParam("source");
  const [faults, setFaults] = useUrlParam("faults");
  const [server, setServer] = useUrlParam("server");
  const scope: McpScope = {
    side: parseSide(side),
    source: parseSource(source),
    faults: faults === "1",
    server: server || null,
  };
  const update = (next: Partial<McpScope>) => {
    if (next.side !== undefined) setSide(next.side === "server" ? null : next.side);
    if (next.source !== undefined) setSource(next.source === "auto" ? null : next.source);
    if (next.faults !== undefined) setFaults(next.faults ? "1" : null);
    if (next.server !== undefined) setServer(next.server);
  };
  return [scope, update];
}

// Links between MCP pages keep the project, the range and the MCP scope.
const KEPT = ["project", "range", "side", "source", "faults", "server"] as const;

export function useMcpHref(): (path: string, extra?: Record<string, string>) => Route {
  const current = useSearchParams();
  return (path, extra = {}) => {
    const params = new URLSearchParams();
    for (const key of KEPT) {
      const value = current.get(key);
      if (value) params.set(key, value);
    }
    for (const [key, value] of Object.entries(extra)) params.set(key, value);
    const query = params.toString();
    return (query ? `${path}?${query}` : path) as Route;
  };
}
