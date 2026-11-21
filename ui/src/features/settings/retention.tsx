"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { createAlertDialogController } from "@ui-sdk/react/alert-dialog";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { type FormEvent, type ReactNode, useState } from "react";
import { getRetention, setRetention } from "@/api/client";
import { asUiError, type UiError } from "@/api/errors";
import type { Retention } from "@/api/generated/Retention";
import type { RetentionUpdate } from "@/api/generated/RetentionUpdate";
import type { SessionProject } from "@/api/generated/SessionProject";
import { EmptyState } from "@/components/empty-state";
import {
  type Copy,
  type CopyByCode,
  FormAlert,
  failureCopy,
  fieldFailed,
} from "@/components/form-alert";
import { TickIcon } from "@/components/icons";
import { Section, SectionFrame } from "@/components/section";
import { useAuth } from "@/lib/auth";
import { formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { ConfirmDialog } from "./confirm-dialog";
import { DeletionRequests } from "./deletion-requests";
import { RowsSkeleton } from "./rows-skeleton";

const TITLE = "Retention";
const MAX_DAYS = 36_500;

export const retentionKey = (project: string) => [project, "retention"];

// The project of the page, or what to say when there is none to show settings for.
export function useSettingsProject(title: string): SessionProject | ReactNode {
  const auth = useAuth();
  const project = useProject();
  if (auth.mode !== "session") {
    return (
      <SectionFrame title={title}>
        <EmptyState kind="unavailable" title={`${title} needs a control plane.`}>
          This server keeps every project&apos;s data until an operator removes it. Set postgres.url
          (CRUCIBLE_POSTGRES_URL) to set retention and delete data here.
        </EmptyState>
      </SectionFrame>
    );
  }
  const access = auth.session.projects.find((visible) => visible.id === project);
  if (access === undefined) {
    return (
      <SectionFrame title={title}>
        <EmptyState kind="unavailable" title="This project isn't available to you.">
          Choose one of your projects in the project menu, or ask an organization owner or admin to
          add you to one.
        </EmptyState>
      </SectionFrame>
    );
  }
  return access;
}

function isProject(value: SessionProject | ReactNode): value is SessionProject {
  return typeof value === "object" && value !== null && "permissions" in value;
}

export function RetentionSettings() {
  const project = useSettingsProject(TITLE);
  if (!isProject(project)) return project;
  return (
    <div className="flex flex-col gap-6">
      <RetentionSection key={project.id} project={project} />
      <DeletionRequests project={project} />
    </div>
  );
}

interface Refusal {
  version: number;
  error: UiError;
}

function RetentionSection({ project }: { project: SessionProject }) {
  const query = useQuery({
    queryKey: retentionKey(project.id),
    queryFn: ({ signal }) => getRetention(project.id, signal),
  });
  // The version a save produced: the form of that version says so until it is edited.
  const [savedVersion, setSavedVersion] = useState<number | null>(null);
  // A save refused because the stored version moved on: the reloaded form says so until it is edited.
  const [refusal, setRefusal] = useState<Refusal | null>(null);
  return (
    <Section
      title={`Retention for ${project.name}`}
      label="retention"
      query={query}
      skeleton={<RowsSkeleton />}
      isEmpty={() => false}
      empty={null}
    >
      {(retention) => (
        // A saved or reloaded version starts the form from the stored values.
        <RetentionForm
          key={retention.version}
          project={project}
          retention={retention}
          saved={savedVersion === retention.version}
          onSaved={setSavedVersion}
          refused={refusal !== null && refusal.version !== retention.version ? refusal.error : null}
          onRefused={setRefusal}
        />
      )}
    </Section>
  );
}

// A save refused because the stored version moved on; the form shows the stored values again.
export const SETTINGS_CHANGED: Copy = {
  title: "These settings changed since the page loaded.",
  detail: "The form shows the stored values now. Enter your change again, then save.",
};

const SAVE_COPY: CopyByCode = {
  precondition_failed: SETTINGS_CHANGED,
  forbidden: {
    title: "Only project admins change retention.",
    detail: "Ask an admin of the project, or for the admin role on it.",
  },
};

type Days = number | null | "invalid";

// Empty keeps the data; anything else must be a whole number of days.
function parseDays(text: string): Days {
  const trimmed = text.trim().replace(/\s*days?$/i, "");
  if (trimmed === "") return null;
  if (!/^\d+$/.test(trimmed)) return "invalid";
  const days = Number(trimmed);
  return days >= 1 && days <= MAX_DAYS ? days : "invalid";
}

const shown = (days: number | null) => (days === null ? "" : String(days));

// A setting of its own, capped by how long the data itself is kept.
const effective = (own: number | null, all: number | null) =>
  own === null ? all : all === null ? own : Math.min(own, all);

// Whether the change hides data that is readable now.
function shortens(from: Retention, to: RetentionUpdate): boolean {
  const pairs: [number | null, number | null][] = [
    [from.retention_days, to.retention_days],
    [
      effective(from.payload_retention_days, from.retention_days),
      effective(to.payload_retention_days, to.retention_days),
    ],
    [
      effective(from.mcp_content_retention_days, from.retention_days),
      effective(to.mcp_content_retention_days, to.retention_days),
    ],
  ];
  return pairs.some(([before, after]) => after !== null && (before === null || after < before));
}

function RetentionForm({
  project,
  retention,
  saved,
  onSaved,
  refused,
  onRefused,
}: {
  project: SessionProject;
  retention: Retention;
  saved: boolean;
  onSaved: (version: number | null) => void;
  refused: UiError | null;
  onRefused: (refusal: Refusal | null) => void;
}) {
  const queryClient = useQueryClient();
  const canManage = project.permissions.includes("manage");
  const [all, setAll] = useState(shown(retention.retention_days));
  const [payload, setPayload] = useState(shown(retention.payload_retention_days));
  const [mcp, setMcp] = useState(shown(retention.mcp_content_retention_days));
  const [confirm] = useState(() => createAlertDialogController<RetentionUpdate>());
  const parsed = { all: parseDays(all), payload: parseDays(payload), mcp: parseDays(mcp) };
  const above = (own: Days) =>
    typeof own === "number" && typeof parsed.all === "number" && own > parsed.all;

  const store = async (body: RetentionUpdate) => {
    try {
      const stored = await setRetention(project.id, body, retention.version);
      onSaved(stored.version);
      queryClient.setQueryData(retentionKey(project.id), stored);
    } catch (error) {
      // The stored version is what the next attempt must build on.
      const failed = asUiError(error);
      if (failed.code === "precondition_failed") {
        onRefused({ version: retention.version, error: failed });
        await queryClient.invalidateQueries({ queryKey: retentionKey(project.id) });
      }
      throw error;
    }
  };
  const save = useMutation({ mutationFn: store });
  const failure = save.isError ? asUiError(save.error) : refused;

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const values = [parsed.all, parsed.payload, parsed.mcp];
    if (values.includes("invalid") || above(parsed.payload) || above(parsed.mcp)) return;
    const body: RetentionUpdate = {
      retention_days: parsed.all as number | null,
      payload_retention_days: parsed.payload as number | null,
      mcp_content_retention_days: parsed.mcp as number | null,
    };
    if (shortens(retention, body)) confirm.open({ payload: body });
    else save.mutate(body);
  };

  const changed =
    all !== shown(retention.retention_days) ||
    payload !== shown(retention.payload_retention_days) ||
    mcp !== shown(retention.mcp_content_retention_days);
  const hidden = retention.hidden_before;
  const floor = hidden.retention ?? hidden.payload ?? hidden.mcp_content;
  const fields = [
    {
      name: "retention_days",
      label: "Keep data for",
      value: all,
      set: setAll,
      days: parsed.all,
      over: false,
      placeholder: "Forever",
      help: "Days, counted from when each span started. Leave empty to keep data forever.",
    },
    {
      name: "payload_retention_days",
      label: "Keep inputs and outputs for",
      value: payload,
      set: setPayload,
      days: parsed.payload,
      over: above(parsed.payload),
      placeholder: "As long as the data",
      help: "Span inputs, outputs and corrections. Leave empty to keep them as long as the data.",
    },
    {
      name: "mcp_content_retention_days",
      label: "Keep MCP arguments and results for",
      value: mcp,
      set: setMcp,
      days: parsed.mcp,
      over: above(parsed.mcp),
      placeholder: "As long as the data",
      help: "What MCP tools were called with and returned. Leave empty to keep it as long as the data.",
    },
  ];

  return (
    <>
      <form
        className="flex max-w-prose flex-col gap-4"
        noValidate
        onSubmit={submit}
        onInput={() => {
          if (saved) onSaved(null);
          if (refused) onRefused(null);
          if (failure) save.reset();
        }}
      >
        {fields.map((field) => (
          <Field
            key={field.name}
            name={field.name}
            invalid={
              field.days === "invalid" ||
              field.over ||
              fieldFailed(failure, field.name) ||
              undefined
            }
          >
            <FieldLabel>{field.label}</FieldLabel>
            <Input
              inputMode="numeric"
              autoComplete="off"
              placeholder={field.placeholder}
              readOnly={!canManage}
              value={field.value}
              onValueChange={field.set}
              className="max-w-40"
            />
            <FieldDescription>{field.help}</FieldDescription>
            <FieldError match={field.days === "invalid"}>
              Enter a whole number of days from 1 to 36,500, or leave it empty.
            </FieldError>
            <FieldError match={field.over}>
              This can&apos;t be longer than the data is kept. Lower it, or raise the first setting.
            </FieldError>
            <FieldError match={fieldFailed(failure, field.name)}>
              The server refused this value. Enter 1 to 36,500 days, at most the first setting.
            </FieldError>
          </Field>
        ))}
        {floor ? (
          <p className="text-data text-ink-soft">
            Data from before {formatTime(floor)} stays hidden: a shorter setting had already expired
            it.
          </p>
        ) : null}
        {failure ? (
          <FormAlert
            copy={failureCopy(failure, "Couldn't save retention.", SAVE_COPY)}
            requestId={failure.requestId}
          />
        ) : null}
        {canManage ? (
          <div className="flex items-center gap-3">
            <Button
              type="submit"
              data-variant="solid"
              disabled={!changed || save.isPending}
              aria-busy={save.isPending}
            >
              {save.isPending ? "Saving" : "Save retention"}
            </Button>
            <output className="flex items-center gap-1.5 text-data text-ink-soft">
              {saved ? (
                <>
                  <TickIcon aria-hidden="true" /> Saved. Reads follow the new settings now.
                </>
              ) : null}
            </output>
          </div>
        ) : (
          <p className="text-data text-ink-soft">
            Only project admins change retention. Ask an admin of {project.name}.
          </p>
        )}
      </form>
      <ConfirmDialog
        controller={confirm}
        question={() => "Shorten retention?"}
        description="Data older than the new setting disappears from every page and API answer right away, and is removed from storage within a day. Making retention longer again later doesn't bring it back."
        confirm="Shorten retention"
        pending="Saving"
        agentConfirm="Shortens retention; older data is hidden now and removed within a day"
        failed="Couldn't save retention."
        copy={SAVE_COPY}
        run={store}
      />
    </>
  );
}
