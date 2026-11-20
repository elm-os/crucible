"use client";

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { AlertDialogTrigger, createAlertDialogController } from "@ui-sdk/react/alert-dialog";
import type { Route } from "next";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { type CSSProperties, useState } from "react";
import { deleteDashboard, getDashboard } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { Dashboard } from "@/api/generated/Dashboard";
import { EmptyState } from "@/components/empty-state";
import { Section, SectionFrame } from "@/components/section";
import { scopedHref } from "@/components/shell/nav";
import { ConfirmDialog } from "@/features/settings/confirm-dialog";
import { formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { useTimeRange } from "@/lib/time-range";
import { DuplicateButton } from "./dashboard-list";
import { dashboardKeys, editorHref, useDashboardAccess, useMetricsSchema } from "./shared";
import { catalogOf } from "./widget-data";
import { type WidgetContext, WidgetView } from "./widget-view";

const DELETED = {
  precondition_failed: {
    title: "This dashboard changed since the page loaded.",
    detail: "The page is up to date now. Look again, then delete it if you still want to.",
  },
};

export function DashboardView() {
  const id = useSearchParams().get("id");
  const project = useProject();
  if (!id) {
    return (
      <SectionFrame title="Dashboard">
        <EmptyState kind="unavailable" title="This link doesn't name a dashboard.">
          <BackToList />
        </EmptyState>
      </SectionFrame>
    );
  }
  return <View project={project} id={id} />;
}

export function BackToList() {
  const search = useSearchParams();
  return <Link href={scopedHref("/dashboards", search)}>All dashboards</Link>;
}

function View({ project, id }: { project: string; id: string }) {
  const search = useSearchParams();
  const [range, setRange] = useTimeRange(project);
  const { canEdit } = useDashboardAccess(project);
  const query = useQuery({
    queryKey: dashboardKeys.one(project, id),
    queryFn: ({ signal }) => getDashboard(project, id, signal),
  });
  const schema = useMetricsSchema(project);
  const dashboard = query.data;
  const editable = canEdit && dashboard !== undefined && dashboard.builtin_key === undefined;
  return (
    <div className="flex flex-col gap-6">
      <Section
        title={dashboard?.name ?? "Dashboard"}
        label="the dashboard"
        query={query}
        skeleton={<div className="skeleton h-4 w-1/2" />}
        isEmpty={() => false}
        empty={null}
        actions={
          dashboard && canEdit ? (
            <Actions project={project} dashboard={dashboard} editable={editable} />
          ) : undefined
        }
      >
        {(d) => (
          <div className="flex flex-col gap-1 text-data">
            {d.description ? <p>{d.description}</p> : null}
            <p className="text-label text-ink-soft">
              {d.builtin_key !== undefined
                ? "Built in and read-only. Duplicate it to change it."
                : d.updated_at
                  ? `Changed ${formatTime(d.updated_at)}`
                  : null}
            </p>
            {editable ? (
              <p className="text-label text-ink-soft md:hidden">Editing needs a wider screen.</p>
            ) : null}
          </div>
        )}
      </Section>
      {dashboard ? (
        dashboard.widgets.length === 0 ? (
          <SectionFrame title="Widgets">
            <EmptyState kind="first-use" title="This dashboard has no widgets yet.">
              {editable ? (
                <>
                  Add charts, numbers and notes in the{" "}
                  <Link href={editorHref(dashboard.id, search)}>editor</Link>.
                </>
              ) : (
                "A member or admin of the project can add charts, numbers and notes to it."
              )}
            </EmptyState>
          </SectionFrame>
        ) : (
          <Grid
            dashboard={dashboard}
            context={{
              project,
              owner: dashboard.id,
              range,
              catalog: [],
              onWiden: () => setRange("30d"),
            }}
            schema={schema.data}
            editable={editable}
          />
        )
      ) : null}
    </div>
  );
}

function Grid({
  dashboard,
  context,
  schema,
  editable,
}: {
  dashboard: Dashboard;
  context: WidgetContext;
  schema: ReturnType<typeof useMetricsSchema>["data"];
  editable: boolean;
}) {
  const search = useSearchParams();
  return (
    <div data-dashboard>
      <div data-dashboard-grid>
        {dashboard.widgets.map((widget) => (
          <div
            key={widget.id}
            data-dashboard-cell
            data-widget={widget.id}
            className="min-w-0"
            style={
              {
                "--cell-x": widget.layout.x + 1,
                "--cell-y": widget.layout.y + 1,
                "--cell-w": widget.layout.w,
                "--cell-h": widget.layout.h,
              } as CSSProperties
            }
          >
            <WidgetView
              widget={widget}
              context={{
                ...context,
                catalog: catalogOf(schema, widget.query?.source ?? ""),
                editHref: editable
                  ? (`${editorHref(dashboard.id, search)}&widget=${encodeURIComponent(widget.id)}` as Route)
                  : undefined,
              }}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

function Actions({
  project,
  dashboard,
  editable,
}: {
  project: string;
  dashboard: Dashboard;
  editable: boolean;
}) {
  const search = useSearchParams();
  const router = useRouter();
  const queryClient = useQueryClient();
  const [remove] = useState(() => createAlertDialogController<Dashboard>());
  const run = async (target: Dashboard) => {
    try {
      await deleteDashboard(project, target);
    } catch (error) {
      const failed = asUiError(error);
      if (failed.code === "precondition_failed") {
        await queryClient.invalidateQueries({ queryKey: dashboardKeys.one(project, target.id) });
      }
      if (failed.kind !== "not_found") throw error;
    }
    await queryClient.invalidateQueries({ queryKey: dashboardKeys.list(project) });
    router.push(scopedHref("/dashboards", search));
  };
  return (
    <div className="flex flex-wrap items-center gap-3">
      {editable ? (
        <Link
          href={editorHref(dashboard.id, search)}
          className="hidden text-data md:inline"
          aria-label={`Edit ${dashboard.name}`}
        >
          Edit
        </Link>
      ) : null}
      <DuplicateButton project={project} dashboard={dashboard} />
      {editable ? (
        <>
          <AlertDialogTrigger controller={remove} payload={dashboard} className="text-data">
            Delete
          </AlertDialogTrigger>
          <ConfirmDialog
            controller={remove}
            question={(target) => `Delete ${target?.name ?? "this dashboard"}?`}
            description="The dashboard and its widgets are deleted for everyone in the project. The data they show is kept."
            confirm="Delete dashboard"
            pending="Deleting"
            agentConfirm="Deletes the dashboard and its widgets for everyone in the project"
            failed="The dashboard couldn't be deleted."
            copy={DELETED}
            run={run}
          />
        </>
      ) : null}
    </div>
  );
}
