"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Checkbox, CheckboxIndicator } from "@ui-sdk/react/checkbox";
import {
  Dialog,
  DialogBackdrop,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogTitle,
  DialogTrigger,
} from "@ui-sdk/react/dialog";
import { Field, FieldDescription, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import { type FormEvent, useId, useState } from "react";
import { createExport, listExports, newIdempotencyKey } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { Export } from "@/api/generated/Export";
import type { ExportFormat } from "@/api/generated/ExportFormat";
import type { SessionProject } from "@/api/generated/SessionProject";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy } from "@/components/form-alert";
import { TickIcon } from "@/components/icons";
import { Section, SectionFrame } from "@/components/section";
import { StatusLabel } from "@/components/status";
import { formatBytes } from "@/features/sql/cells";
import { RowsSkeleton } from "@/features/settings/rows-skeleton";
import { useAuth } from "@/lib/auth";
import { formatCount, formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, rangeWindow, useTimeRange } from "@/lib/time-range";

const TITLE = "Exports";
const POLL_MS = 2_000;

export const exportsKey = (project: string) => [project, "exports"];

const FORMATS: readonly { value: ExportFormat; label: string; about: string }[] = [
  {
    value: "crucible-jsonl",
    label: "Crucible JSON lines",
    about:
      "Every field of each span, its MCP operation, and its trace's tags, scores and corrections.",
  },
  {
    value: "otlp-json",
    label: "OTLP JSON",
    about:
      "One OpenTelemetry document per span, with the GenAI attributes, for tools that read OTLP. Spans with a field cut by a size limit are left out.",
  },
];

// What a failed export says, by the code it failed with. The server's own text never shows.
const NARROW =
  "Narrow the time range at the top of the page, or export one user or session, then export again.";
const FAILURES: Record<string, string> = {
  query_timeout: `Reading the spans took too long. ${NARROW}`,
  query_memory_exceeded: `Reading the spans needed too much memory. ${NARROW}`,
  query_too_broad: `The export reads too many spans at once. ${NARROW}`,
  export_too_large: `The export didn't finish within 30 minutes. ${NARROW}`,
  project_deleted: "The project was deleted before the export ran.",
  attempts_exhausted:
    "The export kept failing, so the server stopped retrying. Try again later; the server's log says why it failed.",
};

export function ExportsSettings() {
  const auth = useAuth();
  const project = useProject();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="Exports need a control plane.">
          The control plane keeps track of exports. Set postgres.url (CRUCIBLE_POSTGRES_URL) to
          export spans here, or run crucible export on the server.
        </EmptyState>
      </SectionFrame>
    );
  }
  const access = auth.session.projects.find((visible) => visible.id === project);
  if (access === undefined) {
    return (
      <SectionFrame title={TITLE}>
        <EmptyState kind="unavailable" title="This project isn't available to you.">
          Choose one of your projects in the project menu.
        </EmptyState>
      </SectionFrame>
    );
  }
  return <ExportsSection project={access} />;
}

function ExportsSection({ project }: { project: SessionProject }) {
  const canCreate = project.permissions.includes("export");
  const query = useQuery({
    queryKey: exportsKey(project.id),
    queryFn: ({ signal }) => listExports(project.id, signal),
    // While an export is queued or running, its state and counts come back every two seconds.
    refetchInterval: (current) =>
      current.state.data?.data.some((item) => item.state === "queued" || item.state === "running")
        ? POLL_MS
        : false,
  });
  return (
    <Section
      title={`Exports of ${project.name}`}
      label="Exports"
      query={query}
      actions={canCreate ? <CreateExportDialog project={project} /> : null}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No exports yet.">
          {canCreate
            ? `Export ${project.name}'s spans as a file, to keep them or to read them with other tools.`
            : `Members and admins of ${project.name} can export its spans; exports they make show here.`}
        </EmptyState>
      }
    >
      {(list) => <ExportsTable exports={list.data} />}
    </Section>
  );
}

function StateCell({ item }: { item: Export }) {
  switch (item.state) {
    case "ready":
      return (
        <div className="flex flex-col">
          <StatusLabel status="ok" label="Ready" />
          {item.expires_at ? (
            <span className="text-label text-ink-soft">Until {formatTime(item.expires_at)}</span>
          ) : null}
        </div>
      );
    case "failed":
      return (
        <div className="flex flex-col">
          <StatusLabel status="error" label="Failed" />
          <span className="max-w-72 text-label text-ink-soft">
            {FAILURES[item.error_code ?? ""] ?? "Something went wrong on the server. Export again."}
          </span>
        </div>
      );
    case "expired":
      return <StatusLabel status="unset" label="Expired" />;
    case "running":
      return (
        <StatusLabel
          status="unset"
          label={`Writing, ${formatCount(item.spans)} ${item.spans === 1 ? "span" : "spans"} so far`}
        />
      );
    default:
      return <StatusLabel status="unset" label="Queued" />;
  }
}

function ExportsTable({ exports }: { exports: Export[] }) {
  return (
    // `relative` keeps the visually hidden labels in the rows inside the scroller on a phone.
    <div className="relative overflow-x-auto">
      <table className="w-full min-w-[48rem] text-start text-data">
        <thead>
          <tr className="text-label text-ink-soft">
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              Created
            </th>
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              Format
            </th>
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              Spans from
            </th>
            <th scope="col" className="h-8 pe-4 text-start font-medium">
              State
            </th>
            <th scope="col" className="h-8 pe-4 text-end font-medium">
              Spans
            </th>
            <th scope="col" className="h-8 pe-4 text-end font-medium">
              Size
            </th>
            <th scope="col" className="h-8 text-start font-medium">
              <span className="sr-only">Download</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {exports.map((item) => (
            <tr key={item.id} className="h-12 border-t border-ash-border align-middle">
              <td className="pe-4 whitespace-nowrap">{formatTime(item.created_at)}</td>
              <td className="pe-4 whitespace-nowrap">
                {FORMATS.find((format) => format.value === item.format)?.label}
                {item.gzip ? ", gzip" : ""}
              </td>
              <td className="pe-4 whitespace-nowrap">
                {formatTime(item.from)} to {formatTime(item.to)}
              </td>
              <td className="py-1.5 pe-4">
                <StateCell item={item} />
              </td>
              <td className="pe-4 text-end tabular-nums">
                {formatCount(item.spans)}
                {item.skipped > 0 ? (
                  <span className="block text-label text-ink-soft">
                    {formatCount(item.skipped)} left out
                  </span>
                ) : null}
              </td>
              <td className="pe-4 text-end tabular-nums whitespace-nowrap">
                {item.state === "ready" ? formatBytes(item.bytes) : ""}
              </td>
              <td className="text-end">
                {/* Shown only while the file is ready. A file that expires in the meantime fails as a
                    download, which the browser reports itself; the page shows no server text. */}
                {item.download_path ? (
                  <a href={item.download_path} download className="text-data">
                    Download{" "}
                    <span className="sr-only">
                      the export created {formatTime(item.created_at)}
                    </span>
                  </a>
                ) : null}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function rateLimitCopy(retryAfterS: number | null): CopyByCode {
  const minutes = retryAfterS === null ? null : Math.max(1, Math.ceil(retryAfterS / 60));
  return {
    rate_limited: {
      title: "A project can start 5 exports an hour.",
      detail:
        minutes === null
          ? "Try again later."
          : `Try again in ${minutes} ${minutes === 1 ? "minute" : "minutes"}.`,
    },
  };
}

function CreateExportDialog({ project }: { project: SessionProject }) {
  const queryClient = useQueryClient();
  const [range] = useTimeRange(project.id);
  const [open, setOpen] = useState(false);
  const [format, setFormat] = useState<ExportFormat>("crucible-jsonl");
  const [gzip, setGzip] = useState(false);
  const gzipId = useId();
  // One key per opened form: a second click, or a retry after a lost answer, starts no second export.
  const [key, setKey] = useState(newIdempotencyKey);
  const create = useMutation({
    mutationFn: (fields: { user_id?: string; session_id?: string }) => {
      const window = rangeWindow(range, Date.now());
      return createExport(project.id, { format, ...window, ...fields, gzip }, key);
    },
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: exportsKey(project.id) });
      setOpen(false);
    },
  });
  const failure = create.isError ? asUiError(create.error) : null;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    const text = (name: string) => String(data.get(name) ?? "").trim() || undefined;
    create.mutate({ user_id: text("user_id"), session_id: text("session_id") });
  };

  return (
    <Dialog
      open={open}
      onOpenChange={setOpen}
      onOpenChangeComplete={(isOpen) => {
        if (!isOpen) {
          create.reset();
          setKey(newIdempotencyKey());
        }
      }}
    >
      <DialogTrigger className="text-data">Export spans</DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        <div className="flex flex-col gap-2">
          <DialogTitle>Export {project.name}&apos;s spans</DialogTitle>
          <DialogDescription>
            The spans that started in {RANGE_PHRASE[range]}, the range at the top of the page, as
            one file. It takes from seconds to minutes; the list shows its progress, and the file
            can be downloaded for 7 days.
          </DialogDescription>
        </div>
        <form
          className="flex flex-col gap-4"
          onSubmit={submit}
          onInput={() => {
            if (failure) create.reset();
          }}
        >
          <div className="flex flex-col gap-2">
            <SegmentedControl
              aria-label="Format"
              value={format}
              onValueChange={(next) => {
                if (next !== null) setFormat(next as ExportFormat);
              }}
            >
              <SegmentedControlIndicator />
              {FORMATS.map((option) => (
                <SegmentedControlItem key={option.value} value={option.value} className="text-data">
                  {option.label}
                </SegmentedControlItem>
              ))}
            </SegmentedControl>
            <p className="text-label text-ink-soft">
              {FORMATS.find((option) => option.value === format)?.about}
            </p>
          </div>
          <Field name="user_id">
            <FieldLabel>User</FieldLabel>
            <Input autoComplete="off" />
            <FieldDescription>Optional. Only spans of this user id.</FieldDescription>
          </Field>
          <Field name="session_id">
            <FieldLabel>Session</FieldLabel>
            <Input autoComplete="off" />
            <FieldDescription>Optional. Only spans of this session id.</FieldDescription>
          </Field>
          <div className="flex items-center gap-1.5 text-data">
            <Checkbox id={gzipId} checked={gzip} onCheckedChange={(next) => setGzip(next === true)}>
              <CheckboxIndicator>
                <TickIcon aria-hidden="true" />
              </CheckboxIndicator>
            </Checkbox>
            <label htmlFor={gzipId}>Compress the file with gzip</label>
          </div>
          {failure ? (
            <FormAlert
              copy={failureCopy(
                failure,
                "Couldn't start the export.",
                rateLimitCopy(failure.retryAfterS),
              )}
            />
          ) : null}
          <div className="flex justify-end gap-2">
            <DialogClose>Cancel</DialogClose>
            <Button
              type="submit"
              data-variant="solid"
              disabled={create.isPending}
              aria-busy={create.isPending}
            >
              {create.isPending ? "Starting the export" : "Start export"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
