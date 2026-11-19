"use client";

import { useQuery } from "@tanstack/react-query";
import type { Route } from "next";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { listAlerts } from "@/api/client";
import type { AlertRule } from "@/api/generated/AlertRule";
import type { SessionProject } from "@/api/generated/SessionProject";
import { EmptyState } from "@/components/empty-state";
import { Section } from "@/components/section";
import { RowsSkeleton } from "@/features/settings/rows-skeleton";
import { formatTime } from "@/lib/format";
import { conditionText, formatSpan } from "./format";
import { RuleFormDialog } from "./rule-form";
import { AlertStatusLabel, AlertsAccess, alertKeys } from "./shared";

const TITLE = "Alert rules";

export function AlertRules() {
  return (
    <AlertsAccess title={TITLE}>{(project) => <RulesSection project={project} />}</AlertsAccess>
  );
}

function RulesSection({ project }: { project: SessionProject }) {
  const canEdit = project.permissions.includes("edit_content");
  const query = useQuery({
    queryKey: alertKeys.rules(project.id),
    queryFn: ({ signal }) => listAlerts(project.id, signal),
    // States change when the server evaluates, about once a minute.
    refetchInterval: 60_000,
  });
  return (
    <Section
      title={`Alert rules for ${project.name}`}
      label="alert rules"
      query={query}
      actions={canEdit ? <RuleFormDialog project={project} label="New rule" /> : undefined}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No alert rules yet.">
          {canEdit
            ? "A rule watches a measure, like errors or p95 duration, and notifies a channel when it crosses a threshold. Create one with New rule."
            : "A rule watches a measure and notifies a channel when it crosses a threshold. Members and admins of the project create rules."}
        </EmptyState>
      }
    >
      {(list) => <RulesTable rules={list.data} project={project.id} />}
    </Section>
  );
}

function RulesTable({ rules, project }: { rules: AlertRule[]; project: string }) {
  const search = useSearchParams();
  const href = (rule: AlertRule): Route => {
    const params = new URLSearchParams({ project, id: rule.id });
    const range = search.get("range");
    if (range) params.set("range", range);
    return `/alert/?${params}` as Route;
  };
  return (
    <div className="relative overflow-x-auto">
      <table className="w-full min-w-[44rem] text-start text-data">
        <thead>
          <tr className="text-label text-ink-soft">
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              Rule
            </th>
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              State
            </th>
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              Condition
            </th>
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              Checked
            </th>
            <th scope="col" className="h-8 text-start font-medium">
              Last check
            </th>
          </tr>
        </thead>
        <tbody>
          {rules.map((rule) => {
            const firing = rule.series.filter((series) => series.status === "firing").length;
            return (
              <tr key={rule.id} className="h-10 border-t border-ash-border">
                <td className="max-w-72 truncate pe-4">
                  <Link href={href(rule)}>{rule.name}</Link>
                </td>
                <td className="pe-4">
                  {rule.enabled ? (
                    <span className="flex items-center gap-2">
                      <AlertStatusLabel status={rule.status} />
                      {firing > 1 ? <span className="text-ink-soft">{firing} series</span> : null}
                    </span>
                  ) : (
                    <span className="text-ink-soft">Turned off</span>
                  )}
                </td>
                <td className="pe-4">{conditionText(rule)}</td>
                <td className="pe-4 whitespace-nowrap">every {formatSpan(rule.interval_secs)}</td>
                <td className="whitespace-nowrap">
                  {rule.last_evaluated_at ? formatTime(rule.last_evaluated_at) : "Not yet"}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
