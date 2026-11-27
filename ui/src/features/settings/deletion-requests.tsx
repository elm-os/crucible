"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  Dialog,
  DialogBackdrop,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogTitle,
  DialogTrigger,
} from "@ui-sdk/react/dialog";
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import { Textarea } from "@ui-sdk/react/textarea";
import { type FormEvent, useState } from "react";
import { createDeletionRequest, listDeletionRequests } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { DeletionKind } from "@/api/generated/DeletionKind";
import type { DeletionRequest } from "@/api/generated/DeletionRequest";
import type { DeletionRequestCreate } from "@/api/generated/DeletionRequestCreate";
import type { DeletionState } from "@/api/generated/DeletionState";
import type { SessionProject } from "@/api/generated/SessionProject";
import { DialogBody } from "@/components/dialog-body";
import { EmptyState } from "@/components/empty-state";
import { type CopyByCode, FormAlert, failureCopy, focusField } from "@/components/form-alert";
import { AlertIcon, CheckIcon, TodoIcon } from "@/components/icons";
import { Section } from "@/components/section";
import { formatCount, formatTime } from "@/lib/format";
import { RowsSkeleton } from "./rows-skeleton";

const requestsKey = (project: string) => [project, "deletion-requests"];
const REFRESH_MS = 5_000;
const MAX_TRACES = 10_000;

const done = (state: DeletionState) => state === "completed" || state === "failed";

export function DeletionRequests({ project }: { project: SessionProject }) {
  const query = useQuery({
    queryKey: requestsKey(project.id),
    queryFn: ({ signal }) => listDeletionRequests(project.id, signal),
    // A request moves on its own: follow it until every one has ended.
    refetchInterval: (latest) =>
      latest.state.data?.data.some((request) => !done(request.state)) ? REFRESH_MS : false,
  });
  const canDelete = project.permissions.includes("delete_traces");
  return (
    <Section
      title="Deletion requests"
      label="deletion requests"
      query={query}
      actions={canDelete ? <DeleteDataDialog project={project} /> : undefined}
      skeleton={<RowsSkeleton />}
      isEmpty={(list) => list.data.length === 0}
      empty={
        <EmptyState kind="first-use" title="No deletion requests yet.">
          {canDelete
            ? `Use Delete data to remove traces, or everything of one user or session, from ${project.name}.`
            : `Members and admins of ${project.name} can delete traces. Ask one of them, or for the member role.`}
        </EmptyState>
      }
    >
      {(list) => <RequestsTable requests={list.data} more={list.next_cursor !== null} />}
    </Section>
  );
}

function what(request: DeletionRequest): string {
  if (request.kind === "user") return `User ${request.user_id ?? ""}`;
  if (request.kind === "session") return `Session ${request.session_id ?? ""}`;
  const count = request.trace_count ?? 0;
  return count === 1 ? "1 trace" : `${formatCount(count)} traces`;
}

const STATUS: Record<DeletionState, { label: string; detail: string }> = {
  pending: {
    label: "Hidden",
    detail: "Hidden from pages and API answers. Waiting for every server to record the request.",
  },
  logged: {
    label: "Deleting",
    detail: "Hidden from pages and API answers. The deletes are running.",
  },
  applied: {
    label: "Deleted",
    detail: "Deleted for every read, SQL included. Removal from storage follows within a day.",
  },
  verified: { label: "Removed", detail: "Removed from storage and checked." },
  completed: { label: "Removed", detail: "Removed from storage and checked." },
  failed: {
    label: "Failed",
    detail:
      "A step failed and the request stopped. Some of the data may be readable again. Ask an operator to check the server log.",
  },
};

function StatusIcon({ state }: { state: DeletionState }) {
  if (state === "failed") return <AlertIcon aria-hidden="true" className="shrink-0" />;
  if (state === "verified" || state === "completed") {
    return <CheckIcon aria-hidden="true" className="shrink-0" />;
  }
  return <TodoIcon aria-hidden="true" className="shrink-0" />;
}

function progress(request: DeletionRequest): string {
  const deletes = request.places.reduce((sum, place) => sum + place.deletes, 0);
  if (deletes === 0) return "";
  const finished = request.places.reduce((sum, place) => sum + place.deletes_done, 0);
  const removed = request.places.reduce((sum, place) => sum + place.deletes_verified, 0);
  return `${formatCount(finished)} of ${formatCount(deletes)} deletes done, ${formatCount(removed)} removed from storage`;
}

function RequestsTable({ requests, more }: { requests: DeletionRequest[]; more: boolean }) {
  return (
    <>
      <div className="relative overflow-x-auto">
        <table className="w-full min-w-[44rem] text-start text-data">
          <thead>
            <tr className="text-label text-ink-soft">
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                What
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Requested
              </th>
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Status
              </th>
              <th scope="col" className="h-8 text-start font-medium">
                Progress
              </th>
            </tr>
          </thead>
          <tbody>
            {requests.map((request) => (
              <tr key={request.id} className="border-t border-ash-border align-top">
                <td className="max-w-60 truncate py-2.5 pe-4">{what(request)}</td>
                <td className="py-2.5 pe-4 whitespace-nowrap">
                  {formatTime(request.requested_at)}
                </td>
                <td className="py-2.5 pe-4">
                  <span className="flex items-center gap-1.5 font-medium">
                    <StatusIcon state={request.state} />
                    {STATUS[request.state].label}
                  </span>
                  <span className="block max-w-80 text-label text-ink-soft">
                    {STATUS[request.state].detail}
                  </span>
                </td>
                <td className="py-2.5 text-ink-soft">{progress(request)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {more ? (
        <p className="mt-3 text-label text-ink-soft">
          Showing the newest requests. Older ones are in the API under deletion-requests.
        </p>
      ) : null}
    </>
  );
}

const KINDS: { value: DeletionKind; label: string }[] = [
  { value: "trace", label: "Traces" },
  { value: "user", label: "A user" },
  { value: "session", label: "A session" },
];

const DELETE_COPY: CopyByCode = {
  forbidden: {
    title: "Your role can't delete this.",
    detail: "Members delete traces. Deleting by user or session needs a project admin.",
  },
  // The server stored the request and retries its first step itself; the same key makes a resend safe.
  ingest_unavailable: {
    title: "Your request is saved, and the deletion starts within seconds.",
    detail: "Press Delete data again to check that it started. Nothing is deleted twice.",
  },
};

// Trace ids as people paste them: separated by commas, spaces or lines, in any letter case.
export function parseTraceIds(text: string): { ids: string[]; invalid: string | null } {
  const ids = [
    ...new Set(
      text
        .split(/[\s,;]+/)
        .map((id) => id.trim().toLowerCase())
        .filter(Boolean),
    ),
  ];
  return { ids, invalid: ids.find((id) => !/^[0-9a-f]{32}$/.test(id)) ?? null };
}

function DeleteDataDialog({ project }: { project: SessionProject }) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger className="text-data">Delete data</DialogTrigger>
      <DialogBackdrop />
      <DialogContent>
        {open ? <DeleteDataForm project={project} onDone={() => setOpen(false)} /> : null}
      </DialogContent>
    </Dialog>
  );
}

// Unmounts with the dialog, so it opens empty each time, with a new idempotency key.
function DeleteDataForm({ project, onDone }: { project: SessionProject; onDone: () => void }) {
  const queryClient = useQueryClient();
  const byUser = project.permissions.includes("delete_by_user");
  const [kind, setKind] = useState<DeletionKind>("trace");
  const [traces, setTraces] = useState("");
  const [subject, setSubject] = useState("");
  const [checked, setChecked] = useState(false);
  // One key per opening: a retry after a failure the page couldn't read is the same request.
  const [key] = useState(() => crypto.randomUUID());
  const create = useMutation({
    mutationFn: (body: DeletionRequestCreate) => createDeletionRequest(project.id, body, key),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: requestsKey(project.id) });
      onDone();
    },
  });
  const failure = create.isError ? asUiError(create.error) : null;
  const parsed = parseTraceIds(traces);
  const tooMany = parsed.ids.length > MAX_TRACES;
  const tracesMissing = checked && kind === "trace" && parsed.ids.length === 0;
  const tracesInvalid = checked && kind === "trace" && parsed.invalid !== null;
  const subjectMissing = checked && kind !== "trace" && subject.trim() === "";

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setChecked(true);
    if (kind === "trace") {
      if (parsed.ids.length === 0 || parsed.invalid !== null || tooMany) {
        focusField(event.currentTarget, "trace_ids");
        return;
      }
      create.mutate({ kind, trace_ids: parsed.ids });
    } else if (subject.trim() !== "") {
      create.mutate(
        kind === "user" ? { kind, user_id: subject.trim() } : { kind, session_id: subject.trim() },
      );
    } else {
      focusField(event.currentTarget, "subject");
    }
  };

  return (
    <>
      <div className="flex flex-col gap-2">
        <DialogTitle>Delete data from {project.name}</DialogTitle>
        <DialogDescription>
          The data disappears from every page and API answer right away and is removed from storage
          within a day. Data that arrives after this request is kept. This can&apos;t be undone.
        </DialogDescription>
      </div>
      <form
        className="flex min-h-0 flex-col gap-4"
        noValidate
        onSubmit={submit}
        onInput={() => {
          if (failure) create.reset();
        }}
      >
        <DialogBody>
          {byUser ? (
            <SegmentedControl
              aria-label="What to delete"
              value={kind}
              onValueChange={(next) => {
                const chosen = KINDS.find((option) => option.value === next);
                if (chosen) setKind(chosen.value);
                setChecked(false);
                if (failure) create.reset();
              }}
            >
              <SegmentedControlIndicator />
              {KINDS.map((option) => (
                <SegmentedControlItem key={option.value} value={option.value} className="text-data">
                  {option.label}
                </SegmentedControlItem>
              ))}
            </SegmentedControl>
          ) : null}
          {kind === "trace" ? (
            <Field
              name="trace_ids"
              invalid={tracesMissing || tracesInvalid || tooMany || undefined}
            >
              <FieldLabel>Trace ids</FieldLabel>
              <Textarea
                rows={4}
                spellCheck={false}
                autoComplete="off"
                className="font-mono"
                value={traces}
                onValueChange={setTraces}
              />
              <FieldDescription>
                One or more, separated by commas, spaces or lines. A trace&apos;s id is on its page.
                {byUser ? "" : " Deleting by user or session needs a project admin."}
              </FieldDescription>
              <FieldError match={tracesMissing}>Enter at least one trace id.</FieldError>
              <FieldError match={tracesInvalid}>
                &quot;{parsed.invalid}&quot; isn&apos;t a trace id. A trace id is 32 characters of
                0-9 and a-f.
              </FieldError>
              <FieldError match={tooMany}>
                A request takes up to 10,000 traces. Split the list into several requests.
              </FieldError>
            </Field>
          ) : (
            <Field name="subject" invalid={subjectMissing || undefined}>
              <FieldLabel>{kind === "user" ? "User id" : "Session id"}</FieldLabel>
              <Input
                autoComplete="off"
                spellCheck={false}
                maxLength={512}
                value={subject}
                onValueChange={setSubject}
              />
              <FieldDescription>
                {kind === "user"
                  ? "The user id your app sends with its traces. Every trace of the user goes, with its feedback and MCP calls."
                  : "The session id your app sends with its traces. Every trace of the session goes, with its feedback and MCP calls."}
              </FieldDescription>
              <FieldError match={subjectMissing}>
                {kind === "user"
                  ? "Enter the user id to delete."
                  : "Enter the session id to delete."}
              </FieldError>
            </Field>
          )}
        </DialogBody>
        {failure ? (
          <FormAlert copy={failureCopy(failure, "Couldn't request the deletion.", DELETE_COPY)} />
        ) : null}
        <div className="flex justify-end gap-2">
          <DialogClose>Cancel</DialogClose>
          <Button
            type="submit"
            data-variant="solid"
            data-agent-confirm="Deletes the selected data from the project for good"
            disabled={create.isPending}
            aria-busy={create.isPending}
          >
            {create.isPending ? "Requesting the deletion" : "Delete data"}
          </Button>
        </div>
      </form>
    </>
  );
}
