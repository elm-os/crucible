"use client";

import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertDialogTrigger, createAlertDialogController } from "@ui-sdk/react/alert-dialog";
import { Button } from "@ui-sdk/react/button";
import {
  Timeline,
  TimelineConnector,
  TimelineDescription,
  TimelineItem,
  TimelineMarker,
  TimelineTime,
  TimelineTitle,
} from "@ui-sdk/react/timeline";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { useState } from "react";
import { deleteAlert, getAlert, getAlertHistory, patchAlert, previewAlert } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { AlertEvent } from "@/api/generated/AlertEvent";
import type { AlertRule } from "@/api/generated/AlertRule";
import type { SessionProject } from "@/api/generated/SessionProject";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy } from "@/components/form-alert";
import { ErrorCard, Section, SectionFrame, useLoadingPhase } from "@/components/section";
import { scopedHref } from "@/components/shell/nav";
import { ConfirmDialog } from "@/features/settings/confirm-dialog";
import { RowsSkeleton } from "@/features/settings/rows-skeleton";
import { formatTime } from "@/lib/format";
import { conditionText, formatSpan, formatValue, seriesLabel, STATUS_LABEL } from "./format";
import { PreviewResult, RuleFormDialog } from "./rule-form";
import { AlertStatusLabel, AlertsAccess, alertKeys } from "./shared";

const TITLE = "Alert rule";

const CHANGED: CopyByCode = {
  precondition_failed: {
    title: "This rule changed since the page loaded.",
    detail: "The page is up to date now. Check the rule, then try again.",
  },
};

export function AlertRuleDetail() {
  const id = useSearchParams().get("id");
  return (
    <AlertsAccess title={TITLE}>
      {(project) =>
        id === null || !/^[0-9a-f-]{36}$/.test(id) ? (
          <SectionFrame title={TITLE}>
            <EmptyState kind="unavailable" title="This link doesn't name a rule.">
              Open a rule from the list of alert rules.
            </EmptyState>
          </SectionFrame>
        ) : (
          <Detail project={project} id={id} />
        )
      }
    </AlertsAccess>
  );
}

function Detail({ project, id }: { project: SessionProject; id: string }) {
  const query = useQuery({
    queryKey: alertKeys.rule(project.id, id),
    queryFn: ({ signal }) => getAlert(project.id, id, signal),
    refetchInterval: 60_000,
  });
  return (
    <div className="flex flex-col gap-6">
      <Section
        title="Rule"
        label="the rule"
        query={query}
        skeleton={<RowsSkeleton />}
        isEmpty={() => false}
        empty={null}
        actions={
          query.data && project.permissions.includes("edit_content") ? (
            <RuleActions project={project} rule={query.data} />
          ) : undefined
        }
      >
        {(rule) => <RuleSummary rule={rule} />}
      </Section>
      {query.data ? (
        <>
          <SeriesSection rule={query.data} />
          <PreviewSection project={project.id} rule={query.data} />
          <HistorySection project={project.id} rule={query.data} />
        </>
      ) : null}
    </div>
  );
}

function RuleActions({ project, rule }: { project: SessionProject; rule: AlertRule }) {
  const queryClient = useQueryClient();
  const router = useRouter();
  const search = useSearchParams();
  const [remove] = useState(() => createAlertDialogController<AlertRule>());
  const refresh = () => queryClient.invalidateQueries({ queryKey: alertKeys.rules(project.id) });
  const toggle = useMutation({
    mutationFn: () => patchAlert(project.id, rule, { enabled: !rule.enabled }),
    onSuccess: (updated) => {
      queryClient.setQueryData(alertKeys.rule(project.id, rule.id), updated);
      return refresh();
    },
    onError: (error) => {
      if (asUiError(error).code === "precondition_failed") return refresh();
    },
  });
  const failure = toggle.isError ? asUiError(toggle.error) : null;

  const removeOne = async (target: AlertRule) => {
    try {
      await deleteAlert(project.id, target);
    } catch (error) {
      const failed = asUiError(error);
      if (failed.kind !== "not_found") {
        if (failed.code === "precondition_failed") await refresh();
        throw error;
      }
    }
    queryClient.removeQueries({ queryKey: alertKeys.rule(project.id, target.id) });
    await queryClient.invalidateQueries({ queryKey: alertKeys.rules(project.id), exact: true });
    router.push(scopedHref("/alerts", search));
  };

  return (
    <div className="flex flex-col items-end gap-2">
      <div className="flex flex-wrap items-center gap-2">
        <RuleFormDialog project={project} rule={rule} label="Edit" />
        <Button
          className="text-data"
          onClick={() => toggle.mutate()}
          disabled={toggle.isPending}
          aria-busy={toggle.isPending}
        >
          {toggle.isPending
            ? rule.enabled
              ? "Turning off"
              : "Turning on"
            : rule.enabled
              ? "Turn off"
              : "Turn on"}
        </Button>
        <AlertDialogTrigger controller={remove} payload={rule} className="text-data">
          Delete
        </AlertDialogTrigger>
      </div>
      {failure ? (
        <FormAlert
          copy={failureCopy(failure, "Couldn't change the rule.", CHANGED)}
          requestId={failure.requestId}
        />
      ) : null}
      <ConfirmDialog
        controller={remove}
        question={(target) => (target ? `Delete the rule "${target.name}"?` : "Delete this rule?")}
        description="Its state and its history are deleted with it, and it stops notifying. This can't be undone."
        confirm="Delete rule"
        pending="Deleting"
        agentConfirm="Deletes the alert rule with its state and history"
        failed="Couldn't delete the rule."
        copy={CHANGED}
        run={removeOne}
      />
    </div>
  );
}

function RuleSummary({ rule }: { rule: AlertRule }) {
  const facts: [string, string][] = [
    ["Condition", conditionText(rule)],
    ["Grouped by", rule.dimensions.length === 0 ? "Nothing" : rule.dimensions.join(", ")],
    ["Checked", `every ${formatSpan(rule.interval_secs)}`],
    ["Fires after", rule.for_secs === 0 ? "the first breach" : formatSpan(rule.for_secs)],
    [
      "No data",
      rule.no_data === "keep"
        ? "keeps the state"
        : `${rule.no_data === "ok" ? "counts as OK" : "notifies"} after ${formatSpan(rule.no_data_for_secs)}`,
    ],
    ["Repeats", `every ${formatSpan(rule.renotify_secs)} while firing`],
    [
      "Channels",
      rule.channels.length === 0
        ? "None: the rule notifies nobody"
        : `${rule.channels.length} ${rule.channels.length === 1 ? "channel" : "channels"}`,
    ],
    ["Last check", rule.last_evaluated_at ? formatTime(rule.last_evaluated_at) : "Not yet"],
  ];
  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
        <h2 className="text-body font-medium">{rule.name}</h2>
        {rule.enabled ? (
          <AlertStatusLabel status={rule.status} />
        ) : (
          <span className="text-data text-ink-soft">Turned off: not checked, notifies nobody</span>
        )}
      </div>
      {rule.eval_failures >= 5 ? (
        <output data-notice className="text-data">
          The last {rule.eval_failures} checks couldn&apos;t read the data, so the state may be
          stale. The telemetry store may be down.
        </output>
      ) : null}
      <dl className="grid gap-x-8 gap-y-2 text-data sm:grid-cols-2">
        {facts.map(([term, value]) => (
          <div key={term} className="flex gap-2">
            <dt className="w-28 shrink-0 text-ink-soft">{term}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
    </div>
  );
}

function SeriesSection({ rule }: { rule: AlertRule }) {
  return (
    <SectionFrame title="Series">
      {rule.series.length === 0 ? (
        <EmptyState kind="incomplete" title="No series yet.">
          {rule.last_evaluated_at
            ? "The rule has found no data in its window so far."
            : `The first check runs within ${formatSpan(rule.interval_secs)} of saving the rule.`}
        </EmptyState>
      ) : (
        <div className="relative overflow-x-auto">
          <table className="w-full min-w-[32rem] text-start text-data">
            <thead>
              <tr className="text-label text-ink-soft">
                <th scope="col" className="h-8 pe-4 text-start font-medium">
                  Series
                </th>
                <th scope="col" className="h-8 pe-4 text-start font-medium">
                  State
                </th>
                <th scope="col" className="h-8 pe-4 text-end font-medium">
                  Last value
                </th>
                <th scope="col" className="h-8 text-start font-medium">
                  Since
                </th>
              </tr>
            </thead>
            <tbody>
              {rule.series.map((series) => (
                <tr key={seriesLabel(series.key)} className="h-10 border-t border-ash-border">
                  <td className="pe-4">{seriesLabel(series.key)}</td>
                  <td className="pe-4">
                    <AlertStatusLabel status={series.status} />
                  </td>
                  <td className="pe-4 text-end tabular-nums">
                    {series.value === undefined ? "None" : formatValue(rule.unit, series.value)}
                  </td>
                  <td className="whitespace-nowrap">{formatTime(series.since)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </SectionFrame>
  );
}

// What the rule would see if it were checked now. It reads the telemetry store, so it fails and retries
// on its own, apart from the rule and its history.
function PreviewSection({ project, rule }: { project: string; rule: AlertRule }) {
  const query = useQuery({
    queryKey: [...alertKeys.rule(project, rule.id), "preview", rule.version],
    queryFn: ({ signal }) =>
      previewAlert(
        project,
        {
          name: rule.name,
          measure: rule.measure,
          aggregation: rule.aggregation,
          dimensions: rule.dimensions,
          comparison: rule.comparison,
          threshold: rule.threshold,
          window_secs: rule.window_secs,
        },
        signal,
      ),
  });
  return (
    <Section
      title="Preview of the next check"
      label="preview"
      query={query}
      skeleton={<RowsSkeleton />}
      isEmpty={() => false}
      empty={null}
    >
      {(preview) => <PreviewResult preview={preview} />}
    </Section>
  );
}

function eventTitle(event: AlertEvent): string {
  if (event.kind === "evaluation_failing") return "Checks keep failing";
  const series = event.key.length === 0 ? "" : `${seriesLabel(event.key)}: `;
  if (event.kind === "renotify") return `${series}still firing`;
  return `${series}${STATUS_LABEL[event.from_status]} to ${STATUS_LABEL[event.to_status]}`;
}

function eventDetail(event: AlertEvent, unit: string): string {
  const parts: string[] = [];
  if (event.kind === "evaluation_failing") {
    parts.push("Five checks in a row couldn't read the data.");
  } else if (event.value !== undefined) {
    parts.push(`Value ${formatValue(unit, event.value)}.`);
  } else {
    parts.push("No data in the window.");
  }
  if (event.notified) parts.push("Channels were notified.");
  return parts.join(" ");
}

function HistorySection({ project, rule }: { project: string; rule: AlertRule }) {
  const query = useInfiniteQuery({
    queryKey: alertKeys.history(project, rule.id),
    queryFn: ({ pageParam, signal }) => getAlertHistory(project, rule.id, pageParam, signal),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor,
  });
  const phase = useLoadingPhase(query.isPending);
  const events = query.data?.pages.flatMap((page) => page.data) ?? [];
  const moreFailure = query.isFetchNextPageError ? asUiError(query.error) : null;

  let body: React.ReactNode;
  if (query.isPending) {
    body = (
      <>
        <output className="sr-only">{phase === "hidden" ? "" : "Loading history"}</output>
        <div aria-hidden="true" className={phase === "hidden" ? "invisible" : undefined}>
          <RowsSkeleton />
        </div>
      </>
    );
  } else if (query.isLoadingError) {
    body = (
      <ErrorCard error={asUiError(query.error)} label="history" onRetry={() => query.refetch()} />
    );
  } else if (events.length === 0) {
    body = (
      <EmptyState kind="first-use" title="No history yet.">
        State changes and notifications of this rule will be listed here.
      </EmptyState>
    );
  } else {
    body = (
      <div className="flex flex-col items-start gap-3">
        <Timeline aria-label={`History of ${rule.name}`}>
          {events.map((event) => (
            <TimelineItem key={event.id}>
              <TimelineMarker />
              <TimelineConnector />
              <TimelineTitle>{eventTitle(event)}</TimelineTitle>
              <TimelineTime dateTime={event.at}>{formatTime(event.at)}</TimelineTime>
              <TimelineDescription>{eventDetail(event, rule.unit)}</TimelineDescription>
            </TimelineItem>
          ))}
        </Timeline>
        {moreFailure ? (
          <FormAlert
            copy={failureCopy(moreFailure, "Couldn't load older history.")}
            requestId={moreFailure.requestId}
          />
        ) : null}
        {query.hasNextPage ? (
          <Button
            onClick={() => query.fetchNextPage()}
            disabled={query.isFetchingNextPage}
            aria-busy={query.isFetchingNextPage}
          >
            {query.isFetchingNextPage ? "Loading older history" : "Load older history"}
          </Button>
        ) : null}
      </div>
    );
  }
  return (
    <SectionFrame title="History" busy={query.isPending}>
      {body}
    </SectionFrame>
  );
}

export function BackToRules() {
  const search = useSearchParams();
  return (
    <Link href={scopedHref("/alerts", search)} className="text-data">
      All alert rules
    </Link>
  );
}
