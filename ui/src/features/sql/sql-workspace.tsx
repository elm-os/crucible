"use client";

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldDescription, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { Textarea } from "@ui-sdk/react/textarea";
import { type FormEvent, type KeyboardEvent, useEffect, useId, useRef, useState } from "react";
import { asUiError, type UiError } from "@/api/errors";
import type { SavedQuery } from "@/api/generated/SavedQuery";
import type { SqlExportFormat } from "@/api/generated/SqlExportFormat";
import type { SqlResult } from "@/api/generated/SqlResult";
import { AlertIcon, CheckIcon } from "@/components/icons";
import { SectionFrame } from "@/components/section";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, type Range, rangeWindow, useTimeRange } from "@/lib/time-range";
import {
  exportSql,
  getSchema,
  listRuns,
  listSavedQueries,
  runSavedQuery,
  runSql,
  sqlKeys,
} from "./api";
import { offsetAt } from "./cells";
import { SqlError, unavailableNotice, type WorkspaceNotice, workspaceNotice } from "./errors";
import { detectParameters, parameterValues } from "./params";
import { RunHistory } from "./run-history";
import { SaveDialog, type SaveMode } from "./save-dialog";
import { RunSavedDialog, SavedQueries } from "./saved-queries";
import { SchemaBrowser } from "./schema-browser";
import { ResultBody, type RunState, SqlErrorCard, Working } from "./sql-result";

const EXPORTS: readonly { format: SqlExportFormat; label: string }[] = [
  { format: "csv", label: "CSV" },
  { format: "jsonl", label: "JSON lines" },
  { format: "parquet", label: "Parquet" },
];

// Read when a run starts, so the window ends at the moment Run was pressed.
const windowNow = (range: Range) => rangeWindow(range, Date.now());

type ExportState =
  | { status: "idle" }
  | { status: "working"; label: string }
  | { status: "done"; filename: string }
  | { status: "failed"; error: UiError };

// The draft, the result and any run in flight belong to one project, so a project switch starts over.
export function SqlWorkspace() {
  const project = useProject();
  return <ProjectWorkspace key={project} project={project} />;
}

function ProjectWorkspace({ project }: { project: string }) {
  const [range] = useTimeRange(project);
  const queryClient = useQueryClient();

  const schema = useQuery({
    queryKey: sqlKeys.schema(project),
    queryFn: ({ signal }) => getSchema(project, signal),
  });
  const status = schema.data?.status;
  const notice: WorkspaceNotice | null =
    status && !status.enabled
      ? workspaceNotice(status.reason ?? null, null)
      : unavailableNotice(schema.error);
  // Saved queries and history wait for the schema's answer, so an unavailable workspace is asked once.
  const ready = !schema.isPending && notice === null;
  const saved = useQuery({
    queryKey: sqlKeys.saved(project),
    queryFn: ({ signal }) => listSavedQueries(project, signal),
    enabled: ready,
  });
  const runs = useQuery({
    queryKey: sqlKeys.runs(project),
    queryFn: ({ signal }) => listRuns(project, signal),
    enabled: ready,
  });

  // The editor starts from the first relation's example and keeps whatever is typed after that.
  const [draft, setDraft] = useState<string | null>(null);
  const text = draft ?? schema.data?.relations[0]?.example ?? "";
  const detected = detectParameters(text);
  const [values, setValues] = useState<Record<string, string>>({});
  const [loaded, setLoaded] = useState<SavedQuery | null>(null);
  const [run, setRun] = useState<RunState>({ status: "idle" });
  const [exported, setExported] = useState<ExportState>({ status: "idle" });
  const [message, setMessage] = useState<string | null>(null);
  const [save, setSave] = useState<{ open: boolean; mode: SaveMode; session: number }>({
    open: false,
    mode: "new",
    session: 0,
  });
  const [runForm, setRunForm] = useState<{ open: boolean; target: SavedQuery | null }>({
    open: false,
    target: null,
  });
  const textarea = useRef<HTMLTextAreaElement>(null);
  const inFlight = useRef<AbortController | null>(null);

  useEffect(() => () => inFlight.current?.abort(), []);

  const running = run.status === "running";
  const exporting = exported.status === "working";
  const blocked = notice !== null || text.trim() === "";

  const refreshHistory = () => queryClient.invalidateQueries({ queryKey: sqlKeys.runs(project) });

  const moveCaret = (line: number, column: number, focus: boolean) => {
    const field = textarea.current;
    if (!field) return;
    const offset = offsetAt(field.value, line, column);
    if (focus) field.focus();
    field.setSelectionRange(offset, offset);
  };

  const start = async (
    what: string,
    fromEditor: boolean,
    request: (signal: AbortSignal) => Promise<SqlResult>,
  ) => {
    inFlight.current?.abort();
    const own = new AbortController();
    inFlight.current = own;
    setRun({ status: "running", what });
    setMessage(null);
    let next: RunState;
    try {
      next = { status: "done", result: await request(own.signal) };
    } catch (error) {
      next = own.signal.aborted
        ? { status: "cancelled" }
        : { status: "failed", error: asUiError(error), fromEditor };
    }
    if (inFlight.current === own) {
      inFlight.current = null;
      setRun(next);
      if (next.status === "failed" && next.error instanceof SqlError) {
        if (fromEditor && next.error.line !== null) {
          moveCaret(next.error.line, next.error.column ?? 1, false);
        }
        if (next.error.code === "sql_workspace_unavailable") {
          void queryClient.invalidateQueries({ queryKey: sqlKeys.schema(project) });
        }
      }
    }
    void refreshHistory();
  };

  const runEditor = () => {
    if (running || blocked) return;
    const request = {
      sql: text,
      ...windowNow(range),
      params: parameterValues(detected, values),
    };
    void start("Running the query", true, (signal) => runSql(project, request, signal));
  };

  const runSaved = (target: SavedQuery, params: Record<string, string>) => {
    setRunForm({ open: false, target });
    const request = { ...windowNow(range), params };
    void start(`Running “${target.name}”`, false, (signal) =>
      runSavedQuery(project, target.id, request, signal),
    );
  };

  const download = async ({ format, label }: (typeof EXPORTS)[number]) => {
    setExported({ status: "working", label });
    setMessage(null);
    try {
      const file = await exportSql(project, {
        sql: text,
        ...windowNow(range),
        params: parameterValues(detected, values),
        format,
      });
      const url = URL.createObjectURL(file.blob);
      const link = Object.assign(document.createElement("a"), {
        href: url,
        download: file.filename,
      });
      document.body.append(link);
      link.click();
      link.remove();
      // Revoking at once can cancel the download before the browser has read the blob.
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
      setExported({ status: "done", filename: file.filename });
    } catch (error) {
      setExported({ status: "failed", error: asUiError(error) });
    }
    void refreshHistory();
  };

  const loadText = (next: string, from: SavedQuery | null, said: string) => {
    setDraft(next);
    setLoaded(from);
    setValues(
      Object.fromEntries(
        (from?.parameters ?? []).map((parameter) => [parameter.name, parameter.default ?? ""]),
      ),
    );
    setMessage(said);
    textarea.current?.focus();
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      runEditor();
    }
  };

  const onSubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    runEditor();
  };

  return (
    <>
      {notice ? (
        <WorkspaceUnavailable
          notice={notice}
          failedChecks={status?.failed_checks ?? []}
          checking={schema.isFetching}
          onCheck={() => schema.refetch()}
        />
      ) : null}
      <div className="grid grid-cols-[minmax(0,1fr)] gap-4 lg:grid-cols-[20rem_minmax(0,1fr)] lg:items-start">
        {notice ? null : (
          <SchemaBrowser
            query={schema}
            className="min-w-0"
            onInsert={(relation) =>
              loadText(relation.example, null, `Inserted the example for ${relation.name}.`)
            }
          />
        )}
        <div className={`flex min-w-0 flex-col gap-4 ${notice ? "lg:col-span-2" : ""}`}>
          <SectionFrame title="Query" busy={running}>
            <form className="flex flex-col gap-4" onSubmit={onSubmit}>
              <Field name="sql">
                <FieldLabel>Query text</FieldLabel>
                <Textarea
                  ref={textarea}
                  value={text}
                  onValueChange={setDraft}
                  onKeyDown={onKeyDown}
                  rows={10}
                  spellCheck={false}
                  autoCapitalize="off"
                  autoCorrect="off"
                  aria-keyshortcuts="Control+Enter Meta+Enter"
                  className="w-full font-mono text-data"
                />
                <FieldDescription>
                  <span className="font-mono">{"{from:DateTime64(6)}"}</span> and{" "}
                  <span className="font-mono">{"{to:DateTime64(6)}"}</span> are bound to the time
                  range in the header, now {RANGE_PHRASE[range]}. Ctrl+Enter or Cmd+Enter runs the
                  query.
                </FieldDescription>
              </Field>
              {loaded ? (
                <p data-loaded className="text-label text-ink-soft">
                  Opened from the saved query “{loaded.name}”.
                  {loaded.can_edit ? "" : " You can run it, and save your edits as a new query."}
                </p>
              ) : null}
              {detected.length > 0 ? (
                <fieldset className="flex flex-col gap-3">
                  <legend className="text-data font-medium">Parameters</legend>
                  <div className="grid gap-3 sm:grid-cols-2">
                    {detected.map(({ name, type }) => (
                      <Field key={name} name={`parameter.${name}`}>
                        <FieldLabel>{name}</FieldLabel>
                        <Input
                          value={values[name] ?? ""}
                          onValueChange={(value) =>
                            setValues((current) => ({ ...current, [name]: value }))
                          }
                          autoComplete="off"
                          spellCheck={false}
                          className="font-mono"
                        />
                        <FieldDescription>
                          <span className="font-mono">{type}</span>
                        </FieldDescription>
                      </Field>
                    ))}
                  </div>
                </fieldset>
              ) : null}
              <div className="flex flex-wrap items-center gap-2">
                <Button type="submit" data-variant="solid" disabled={running || blocked}>
                  Run
                </Button>
                <Button disabled={!running} onClick={() => inFlight.current?.abort()}>
                  Cancel
                </Button>
                {EXPORTS.map((option) => (
                  <Button
                    key={option.format}
                    disabled={exporting || blocked}
                    onClick={() => download(option)}
                  >
                    Export {option.label}
                  </Button>
                ))}
                <Button
                  disabled={blocked}
                  onClick={() =>
                    setSave((current) => ({
                      open: true,
                      mode: "new",
                      session: current.session + 1,
                    }))
                  }
                >
                  Save as new
                </Button>
                {loaded?.can_edit ? (
                  <Button
                    disabled={blocked}
                    onClick={() =>
                      setSave((current) => ({
                        open: true,
                        mode: "update",
                        session: current.session + 1,
                      }))
                    }
                  >
                    Save changes
                  </Button>
                ) : null}
              </div>
              {exported.status === "working" ? (
                <Working label={`Exporting ${exported.label}`}>
                  The file downloads when the query finishes.
                </Working>
              ) : null}
              <output className="block text-data">
                {exported.status === "done" ? (
                  <span data-exported className="flex items-center gap-1.5">
                    <CheckIcon className="shrink-0" />
                    Downloaded {exported.filename}.
                  </span>
                ) : null}
                {message !== null ? (
                  <span data-message className="flex items-center gap-1.5">
                    <CheckIcon className="shrink-0" />
                    {message}
                  </span>
                ) : null}
              </output>
              {exported.status === "failed" ? <SqlErrorCard error={exported.error} /> : null}
            </form>
          </SectionFrame>
          <SectionFrame title="Result" busy={running}>
            <ResultBody run={run} onGoTo={(line, column) => moveCaret(line, column, true)} />
          </SectionFrame>
        </div>
      </div>
      {notice ? null : (
        <div className="mt-4 grid grid-cols-[minmax(0,1fr)] gap-4">
          <SavedQueries
            project={project}
            query={saved}
            running={running}
            onOpen={(target) => loadText(target.sql, target, `Opened “${target.name}”.`)}
            onRun={(target) => {
              if (target.parameters.length === 0) runSaved(target, {});
              else setRunForm({ open: true, target });
            }}
            onDeleted={(target) => {
              if (loaded?.id === target.id) setLoaded(null);
            }}
          />
          <RunHistory
            query={runs}
            onLoad={(target) => loadText(target.sql, null, "Loaded the run's query.")}
          />
        </div>
      )}
      <RunSavedDialog
        open={runForm.open}
        target={runForm.target}
        onClose={() => setRunForm((current) => ({ ...current, open: false }))}
        onRun={runSaved}
      />
      <SaveDialog
        open={save.open}
        session={save.session}
        mode={save.mode}
        project={project}
        loaded={loaded}
        sql={text}
        detected={detected}
        onClose={() => setSave((current) => ({ ...current, open: false }))}
        onSaved={(target, mode) => {
          setSave((current) => ({ ...current, open: false }));
          setLoaded(target);
          setMessage(
            mode === "new" ? `Saved “${target.name}”.` : `Saved changes to “${target.name}”.`,
          );
          void queryClient.invalidateQueries({ queryKey: sqlKeys.saved(project) });
        }}
        onReloaded={(fresh) => {
          setSave((current) => ({ ...current, open: false }));
          loadText(fresh.sql, fresh, `Reloaded “${fresh.name}”. Apply your change again.`);
          void queryClient.invalidateQueries({ queryKey: sqlKeys.saved(project) });
        }}
      />
    </>
  );
}

function WorkspaceUnavailable({
  notice,
  failedChecks,
  checking,
  onCheck,
}: {
  notice: WorkspaceNotice;
  failedChecks: readonly string[];
  checking: boolean;
  onCheck: () => Promise<unknown>;
}) {
  const headingId = useId();
  return (
    <section
      aria-labelledby={headingId}
      data-unavailable={notice.reason ?? "unknown"}
      className="mb-4 flex flex-col items-start gap-2 rounded-xl border border-midnight-ink bg-warm-sand px-4 py-4"
    >
      <h2 id={headingId} className="flex items-center gap-2 text-body font-medium">
        <AlertIcon className="shrink-0" />
        {notice.title}
      </h2>
      <p className="max-w-prose text-data text-ink-soft">{notice.detail}</p>
      {failedChecks.length > 0 ? (
        <p className="text-data text-ink-soft">
          Failed checks: <span className="font-mono">{failedChecks.join(", ")}</span>
        </p>
      ) : null}
      <p className="text-data text-ink-soft">
        Run, export and saving are off until it is available.
      </p>
      <Button onClick={onCheck} aria-busy={checking || undefined}>
        Check again
      </Button>
    </section>
  );
}
