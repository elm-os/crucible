"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  Dialog,
  DialogBackdrop,
  DialogContent,
  DialogDescription,
  DialogTitle,
  DialogTrigger,
} from "@ui-sdk/react/dialog";
import { Field, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { type FormEvent, useState } from "react";
import { createDashboard, duplicateDashboard, listDashboards } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { DashboardSummary } from "@/api/generated/DashboardSummary";
import { DialogBody } from "@/components/dialog-body";
import { EmptyState } from "@/components/empty-state";
import { FormAlert, failureCopy } from "@/components/form-alert";
import { Section } from "@/components/section";
import { formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { useWide } from "@/lib/wide";
import { MAX_NAME } from "./rules";
import { dashboardHref, dashboardKeys, editorHref, useDashboardAccess } from "./shared";

/** Where a new dashboard opens: the editor where there is room for it, else the dashboard itself. */
export function useOpenNew() {
  const router = useRouter();
  const search = useSearchParams();
  const wide = useWide();
  return (id: string) => router.push(wide ? editorHref(id, search) : dashboardHref(id, search));
}

export function DashboardList() {
  const project = useProject();
  const { signedIn, canEdit } = useDashboardAccess(project);
  const query = useQuery({
    queryKey: dashboardKeys.list(project),
    queryFn: ({ signal }) => listDashboards(project, signal),
  });
  return (
    <Section
      title="Dashboards"
      label="dashboards"
      query={query}
      skeleton={<CardsSkeleton />}
      isEmpty={() => false}
      empty={null}
      actions={canEdit ? <NewDashboard project={project} /> : undefined}
    >
      {(list) => {
        const own = list.data.filter((d) => d.builtin_key === undefined);
        const starters = list.data.filter((d) => d.builtin_key !== undefined);
        return (
          <div className="flex flex-col gap-6">
            <div className="flex flex-col gap-2">
              <h3 className="text-data font-medium">Your dashboards</h3>
              {own.length > 0 ? (
                <Cards dashboards={own} project={project} canEdit={canEdit} />
              ) : signedIn ? (
                <EmptyState kind="first-use" title="No dashboards yet.">
                  {canEdit
                    ? "A dashboard is a grid of charts, numbers and notes over the metrics of this project. Create one with New dashboard, or duplicate a starter below and change it."
                    : "A dashboard is a grid of charts, numbers and notes. Members and admins of the project create them; the starters below are ready to read."}
                </EmptyState>
              ) : (
                <EmptyState kind="unavailable" title="Your own dashboards need a control plane.">
                  Dashboards are stored in Postgres. Set postgres.url (CRUCIBLE_POSTGRES_URL), then
                  sign in to create them. The starters below work without it.
                </EmptyState>
              )}
            </div>
            <div className="flex flex-col gap-2">
              <h3 className="text-data font-medium">Starters</h3>
              <p className="text-label text-ink-soft">
                Built in and read-only. Duplicate one to change it.
              </p>
              <Cards dashboards={starters} project={project} canEdit={canEdit} />
            </div>
          </div>
        );
      }}
    </Section>
  );
}

function Cards({
  dashboards,
  project,
  canEdit,
}: {
  dashboards: DashboardSummary[];
  project: string;
  canEdit: boolean;
}) {
  const search = useSearchParams();
  return (
    <ul className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
      {dashboards.map((d) => (
        <li
          key={d.id}
          className="flex flex-col gap-1 rounded-lg border border-ash-border bg-parchment-white p-3"
        >
          <Link href={dashboardHref(d.id, search)} className="text-data font-medium">
            {d.name}
          </Link>
          {d.description ? <p className="text-label text-ink-soft">{d.description}</p> : null}
          <p className="text-label text-ink-soft">
            {d.widget_count === 1 ? "1 widget" : `${d.widget_count} widgets`}
            {d.updated_at ? `, changed ${formatTime(d.updated_at)}` : ""}
          </p>
          {canEdit && d.builtin_key !== undefined ? (
            <div className="mt-1">
              <DuplicateButton project={project} dashboard={d} />
            </div>
          ) : null}
        </li>
      ))}
    </ul>
  );
}

export function DuplicateButton({
  project,
  dashboard,
}: {
  project: string;
  dashboard: Pick<DashboardSummary, "id" | "name">;
}) {
  const queryClient = useQueryClient();
  const open = useOpenNew();
  const duplicate = useMutation({
    mutationFn: () => duplicateDashboard(project, dashboard.id, {}),
    onSuccess: async (copy) => {
      await queryClient.invalidateQueries({ queryKey: dashboardKeys.list(project) });
      open(copy.id);
    },
  });
  const error = duplicate.isError ? asUiError(duplicate.error) : null;
  return (
    <div className="flex flex-col items-start gap-2">
      <Button
        aria-label={`Duplicate ${dashboard.name}`}
        onClick={() => duplicate.mutate()}
        disabled={duplicate.isPending}
      >
        {duplicate.isPending ? "Duplicating" : "Duplicate"}
      </Button>
      {error ? (
        <FormAlert
          copy={failureCopy(error, "The dashboard couldn't be duplicated.")}
          requestId={error.requestId}
        />
      ) : null}
    </div>
  );
}

function NewDashboard({ project }: { project: string }) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger className="text-data">New dashboard</DialogTrigger>
      <DialogBackdrop />
      <DialogContent className="w-full max-w-md">
        {open ? <NewDashboardForm project={project} /> : null}
      </DialogContent>
    </Dialog>
  );
}

function NewDashboardForm({ project }: { project: string }) {
  const queryClient = useQueryClient();
  const openNew = useOpenNew();
  const create = useMutation({
    mutationFn: (name: string) => createDashboard(project, { name }),
    onSuccess: async (dashboard) => {
      await queryClient.invalidateQueries({ queryKey: dashboardKeys.list(project) });
      openNew(dashboard.id);
    },
  });
  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    create.mutate(String(new FormData(event.currentTarget).get("name")).trim());
  };
  const error = create.isError ? asUiError(create.error) : null;
  return (
    <form onSubmit={submit} className="flex min-h-0 flex-col gap-4">
      <div className="flex flex-col gap-1">
        <DialogTitle>New dashboard</DialogTitle>
        <DialogDescription>Name it; you add widgets in the editor next.</DialogDescription>
      </div>
      <DialogBody>
        <Field name="name">
          <FieldLabel>Name</FieldLabel>
          <Input required pattern=".*\S.*" maxLength={MAX_NAME} autoComplete="off" />
          <FieldError match="valueMissing">Enter a name, like Agent health.</FieldError>
          <FieldError match="patternMismatch">Enter a name, like Agent health.</FieldError>
        </Field>
      </DialogBody>
      {error ? (
        <FormAlert
          copy={failureCopy(error, "The dashboard couldn't be created.")}
          requestId={error.requestId}
        />
      ) : null}
      <div className="flex justify-end">
        <Button type="submit" data-variant="solid" disabled={create.isPending}>
          {create.isPending ? "Creating" : "Create"}
        </Button>
      </div>
    </form>
  );
}

const CARDS = ["a", "b", "c"];

function CardsSkeleton() {
  return (
    <ul className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
      {CARDS.map((id) => (
        <li key={id} className="flex flex-col gap-2 rounded-lg border border-ash-border p-3">
          <div className="skeleton h-4 w-1/2" />
          <div className="skeleton h-3 w-3/4" />
        </li>
      ))}
    </ul>
  );
}
