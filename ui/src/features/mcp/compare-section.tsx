"use client";

import { useQuery } from "@tanstack/react-query";
import Link from "next/link";
import { compareMcpDeploys } from "@/api/client";
import type { McpCompare } from "@/api/generated/McpCompare";
import type { McpStats } from "@/api/generated/McpStats";
import { EmptyState } from "@/components/empty-state";
import { Section } from "@/components/section";
import { MetaNotice } from "@/components/status";
import { formatCount } from "@/lib/format";
import { useProject } from "@/lib/project";
import { rangeWindow, useTimeRange } from "@/lib/time-range";
import { useUrlParam } from "@/lib/url-state";
import { formatLatency, formatRate } from "./format";
import { SourceNote } from "./mcp-bar";
import { useMcpHref, useMcpScope } from "./scope";
import { emptyAndWhole, McpEmpty, Table, TableSkeleton } from "./shared";

const COLUMNS = ["Tool", "Calls", "Error rate", "p50", "p99"] as const;

const changes = (compare: McpCompare) =>
  compare.tools.length + compare.added.length + compare.removed.length + compare.changed.length;

// Two deploys of one server side by side: each tool's calls, error rate and latency on both, and the
// tools the candidate added, removed or changed.
export function CompareSection() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [scope] = useMcpScope();
  const href = useMcpHref();
  const [baseline] = useUrlParam("baseline");
  const [candidate] = useUrlParam("candidate");
  const ready = Boolean(scope.server && baseline && candidate);
  const query = useQuery({
    queryKey: [project, "mcp", "compare", baseline, candidate, scope, range],
    queryFn: ({ signal }) =>
      compareMcpDeploys(
        project,
        baseline ?? "",
        candidate ?? "",
        rangeWindow(range, Date.now()),
        scope,
        signal,
      ),
    enabled: ready,
  });
  if (!ready) {
    return (
      <EmptyState kind="filtered" title="Pick two deploys to compare.">
        Open the <Link href={href("/mcp/deploys/")}>deploys</Link> and choose Compare beside a
        deploy to set it against the one before it.
      </EmptyState>
    );
  }
  return (
    <Section
      title={`${scope.server}: ${baseline} against ${candidate}`}
      label="the comparison"
      query={query}
      skeleton={<TableSkeleton />}
      isEmpty={(compare) => emptyAndWhole(changes(compare), compare.meta)}
      empty={
        <McpEmpty
          noun="calls of these deploys"
          range={range}
          onWiden={() => setRange("30d")}
          widest="Both deploys must have run inside the time range."
        />
      }
    >
      {(compare) => (
        <>
          <MetaNotice meta={compare.meta} noun="calls" />
          <SourceNote sources={compare.capture_sources} />
          <div className="mb-4 grid gap-4 sm:grid-cols-3">
            <NameList title="Added" names={compare.added} />
            <NameList title="Removed" names={compare.removed} />
            <NameList title="Schema changed" names={compare.changed} />
          </div>
          <p className="mb-2 text-label text-ink-soft">{`Each cell reads baseline ${baseline}, then candidate ${candidate}.`}</p>
          <Table columns={COLUMNS} wide label="Tools of both deploys">
            {compare.tools.map((tool) => (
              <tr key={tool.tool_name} className="h-8 border-t border-ash-border">
                <td className="max-w-60 truncate pe-4">{tool.tool_name}</td>
                <Delta
                  base={tool.baseline}
                  next={tool.candidate}
                  pick={(s) => formatCount(s.calls)}
                />
                <Delta
                  base={tool.baseline}
                  next={tool.candidate}
                  pick={(s) => formatRate(s.error_rate)}
                />
                <Delta
                  base={tool.baseline}
                  next={tool.candidate}
                  pick={(s) => formatLatency(s.latency.p50_us)}
                />
                <Delta
                  base={tool.baseline}
                  next={tool.candidate}
                  pick={(s) => formatLatency(s.latency.p99_us)}
                />
              </tr>
            ))}
          </Table>
        </>
      )}
    </Section>
  );
}

function Delta({
  base,
  next,
  pick,
}: {
  base: McpStats;
  next: McpStats;
  pick: (stats: McpStats) => string;
}) {
  return (
    <td className="pe-4 whitespace-nowrap tabular-nums">
      {pick(base)}
      <span className="text-ink-soft"> to </span>
      {pick(next)}
    </td>
  );
}

function NameList({ title, names }: { title: string; names: string[] }) {
  return (
    <div className="flex flex-col gap-1">
      <h3 className="text-label text-ink-soft">{`${title} (${names.length})`}</h3>
      {names.length === 0 ? (
        <p className="text-data text-ink-soft">None</p>
      ) : (
        <ul className="flex flex-col gap-0.5 text-data">
          {names.map((name) => (
            <li key={name} className="truncate">
              {name}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
