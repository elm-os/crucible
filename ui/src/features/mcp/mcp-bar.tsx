"use client";

import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { Field, FieldLabel } from "@ui-sdk/react/field";
import { Switch, SwitchThumb } from "@ui-sdk/react/switch";
import Link from "next/link";
import type { McpServerSource } from "@/api/generated/McpServerSource";
import { CLASS_LABEL, sourcesSentence } from "./format";
import { parseSide, parseSource, SOURCES, useMcpHref, useMcpScope } from "./scope";

export const MCP_PAGES = [
  { value: "servers", label: "Servers" },
  { value: "tools", label: "Tools" },
  { value: "errors", label: "Errors" },
  { value: "clients", label: "Clients" },
  { value: "users", label: "Users" },
  { value: "sessions", label: "Sessions" },
  { value: "operations", label: "Operations" },
  { value: "deploys", label: "Deploys" },
  { value: "compare", label: "Compare" },
] as const;

export type McpPage = (typeof MCP_PAGES)[number]["value"];

// The MCP pages, the side read, which capture source each server is read from, and whether calls the
// caller caused count. Every page shares them through the URL.
export function McpBar({ active }: { active: McpPage | null }) {
  const [scope, setScope] = useMcpScope();
  const href = useMcpHref();
  return (
    <div className="mb-4 flex flex-col gap-3">
      <nav aria-label="MCP pages">
        <ul className="flex flex-wrap gap-x-4 gap-y-1 text-data">
          {MCP_PAGES.map((page) => (
            <li key={page.value}>
              <Link
                href={href(`/mcp/${page.value}/`)}
                aria-current={page.value === active ? "page" : undefined}
                className="text-ink-soft aria-[current=page]:font-medium aria-[current=page]:text-midnight-ink"
              >
                {page.label}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      <div className="flex flex-wrap items-center gap-x-6 gap-y-2">
        <SegmentedControl
          aria-label="Side"
          value={scope.side}
          onValueChange={(next) => setScope({ side: parseSide(next) })}
        >
          <SegmentedControlIndicator />
          <SegmentedControlItem value="server" className="text-data">
            Server side
          </SegmentedControlItem>
          <SegmentedControlItem value="client" className="text-data">
            Client side
          </SegmentedControlItem>
        </SegmentedControl>
        <Select
          items={SOURCES}
          value={scope.source}
          onValueChange={(next) => setScope({ source: parseSource(next) })}
        >
          <div className="flex items-center gap-2">
            <SelectLabel className="text-label text-ink-soft">Capture source</SelectLabel>
            <SelectTrigger className="text-data">
              <SelectValue />
            </SelectTrigger>
          </div>
          <SelectContent>
            {SOURCES.map((source) => (
              <SelectItem key={source.value} value={source.value}>
                {source.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Field className="flex flex-row items-center gap-2">
          <Switch
            checked={scope.faults}
            onCheckedChange={(checked) => setScope({ faults: checked })}
          >
            <SwitchThumb />
          </Switch>
          <FieldLabel className="text-data">Include caller faults</FieldLabel>
        </Field>
      </div>
    </div>
  );
}

// Says which capture source each server was read from: the automatic choice, or the one asked for.
export function SourceNote({ sources }: { sources: McpServerSource[] }) {
  const sentence = sourcesSentence(sources);
  const skipped = sources.filter((s) => s.chosen === null).map((s) => s.server_name);
  if (sentence === null && skipped.length === 0) return null;
  return (
    <p data-sources className="mb-3 text-label text-ink-soft">
      {sentence === null ? null : `Read ${sentence}.`}
      {skipped.length > 0
        ? ` Not read: ${skipped.join(", ")} (no rows of this capture source; ${skipped.length === 1 ? "it has" : "they have"} ${availableOf(sources, skipped)}).`
        : null}
    </p>
  );
}

function availableOf(sources: McpServerSource[], skipped: string[]): string {
  const classes = new Set(
    sources.filter((s) => skipped.includes(s.server_name)).flatMap((s) => s.available),
  );
  return [...classes].map((c) => CLASS_LABEL[c]).join(" and ") || "none";
}
