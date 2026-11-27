"use client";

import { type UseQueryResult, useQueryClient } from "@tanstack/react-query";
import {
  AlertDialog,
  AlertDialogBackdrop,
  AlertDialogClose,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogTitle,
  createConfirm,
} from "@ui-sdk/react/alert-dialog";
import { Button } from "@ui-sdk/react/button";
import {
  Dialog,
  DialogBackdrop,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "@ui-sdk/react/dialog";
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { type FormEvent, useState } from "react";
import { asUiError, type UiError } from "@/api/errors";
import type { SavedQuery } from "@/api/generated/SavedQuery";
import type { SavedQueryList } from "@/api/generated/SavedQueryList";
import { DialogBody } from "@/components/dialog-body";
import { EmptyState } from "@/components/empty-state";
import { CheckIcon } from "@/components/icons";
import { Section } from "@/components/section";
import { deleteSavedQuery, sqlKeys } from "./api";
import { SqlErrorCard } from "./sql-result";

const VISIBILITY: Record<SavedQuery["visibility"], string> = {
  private: "Private",
  project: "Shared with the project",
};

const SKELETON_ROWS = ["a", "b", "c"] as const;

export function SavedQueries({
  project,
  query,
  running,
  onOpen,
  onRun,
  onDeleted,
}: {
  project: string;
  query: UseQueryResult<SavedQueryList>;
  running: boolean;
  onOpen: (saved: SavedQuery) => void;
  onRun: (saved: SavedQuery) => void;
  onDeleted: (saved: SavedQuery) => void;
}) {
  const queryClient = useQueryClient();
  const [confirm] = useState(() => createConfirm<{ name: string }>());
  const [deleting, setDeleting] = useState<string | null>(null);
  const [deleted, setDeleted] = useState<string | null>(null);
  const [failure, setFailure] = useState<UiError | null>(null);

  const remove = async (saved: SavedQuery) => {
    if (!(await confirm({ name: saved.name }))) return;
    setDeleting(saved.id);
    setDeleted(null);
    setFailure(null);
    try {
      await deleteSavedQuery(project, saved.id);
      setDeleted(saved.name);
      onDeleted(saved);
      await queryClient.invalidateQueries({ queryKey: sqlKeys.saved(project) });
    } catch (error) {
      setFailure(asUiError(error));
    } finally {
      setDeleting(null);
    }
  };

  return (
    <>
      <Section
        title="Saved queries"
        query={query}
        skeleton={
          <div className="flex flex-col gap-4 py-1">
            {SKELETON_ROWS.map((row) => (
              <div key={row} className="flex flex-col gap-2">
                <div className="skeleton h-4 w-1/3" />
                <div className="skeleton h-3 w-2/3" />
              </div>
            ))}
          </div>
        }
        isEmpty={(list) => list.data.length === 0 && deleted === null && failure === null}
        empty={<NoSavedQueries />}
      >
        {(list) => (
          <>
            <output className="block">
              {deleted !== null ? (
                <span data-deleted className="mb-2 flex items-center gap-1.5 text-data">
                  <CheckIcon className="shrink-0" />
                  Deleted “{deleted}”.
                </span>
              ) : null}
            </output>
            {failure ? <SqlErrorCard error={failure} /> : null}
            {list.data.length === 0 ? <NoSavedQueries /> : null}
            <ul className="flex flex-col">
              {list.data.map((saved) => (
                <li
                  key={saved.id}
                  data-saved-query={saved.id}
                  aria-busy={deleting === saved.id || undefined}
                  className="flex flex-col gap-2 border-t border-ash-border py-3 first:border-t-0 sm:flex-row sm:items-start sm:justify-between sm:gap-6"
                >
                  <div className="flex min-w-0 flex-col gap-1">
                    <p className="text-body font-medium break-words">{saved.name}</p>
                    {saved.description ? (
                      <p className="max-w-prose text-data text-ink-soft">{saved.description}</p>
                    ) : null}
                    <p className="text-label text-ink-soft">
                      {VISIBILITY[saved.visibility]}
                      {saved.parameters.length === 0 ? ". No parameters." : ". Parameters: "}
                      {saved.parameters.map((parameter, index) => (
                        <span key={parameter.name} className="font-mono">
                          {index > 0 ? ", " : ""}
                          {parameter.name}: {parameter.type}
                          {parameter.default === undefined ? "" : ` = ${parameter.default}`}
                        </span>
                      ))}
                    </p>
                  </div>
                  <div className="flex shrink-0 flex-wrap gap-2">
                    <Button onClick={() => onOpen(saved)}>
                      Open<span className="sr-only"> {saved.name}</span>
                    </Button>
                    {saved.can_run ? (
                      <Button disabled={running} onClick={() => onRun(saved)}>
                        Run<span className="sr-only"> {saved.name}</span>
                      </Button>
                    ) : null}
                    {saved.can_edit ? (
                      <Button disabled={deleting !== null} onClick={() => remove(saved)}>
                        {deleting === saved.id ? "Deleting" : "Delete"}
                        <span className="sr-only"> {saved.name}</span>
                      </Button>
                    ) : null}
                  </div>
                </li>
              ))}
            </ul>
          </>
        )}
      </Section>
      <AlertDialog controller={confirm.controller}>
        <AlertDialogBackdrop />
        <AlertDialogContent<{ name: string }>>
          {(state) => (
            <>
              <div className="flex flex-col gap-2">
                <AlertDialogTitle>Delete “{state.payload?.name}”?</AlertDialogTitle>
                <AlertDialogDescription>
                  The saved query is removed for everyone who can see it. Runs already in the
                  history stay.
                </AlertDialogDescription>
              </div>
              <div className="mt-4 flex flex-wrap justify-end gap-2">
                <AlertDialogClose>Keep it</AlertDialogClose>
                <AlertDialogClose data-variant="solid" value="confirm">
                  Delete saved query
                </AlertDialogClose>
              </div>
            </>
          )}
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

function NoSavedQueries() {
  return (
    <EmptyState kind="first-use" title="No saved queries yet.">
      Write a query, then choose Save as new to keep it and share it with the project.
    </EmptyState>
  );
}

// The run form of a saved query: one field per declared parameter, defaults prefilled.
export function RunSavedDialog({
  target,
  open,
  onClose,
  onRun,
}: {
  target: SavedQuery | null;
  open: boolean;
  onClose: () => void;
  onRun: (saved: SavedQuery, params: Record<string, string>) => void;
}) {
  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (target === null) return;
    const form = new FormData(event.currentTarget);
    const params: Record<string, string> = {};
    for (const parameter of target.parameters) {
      const value = String(form.get(`parameter.${parameter.name}`) ?? "");
      if (value !== "" || parameter.default === undefined) params[parameter.name] = value;
    }
    onRun(target, params);
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) onClose();
      }}
    >
      <DialogBackdrop />
      <DialogContent>
        <div className="flex flex-col gap-2">
          <DialogTitle>Run “{target?.name}”</DialogTitle>
          <DialogDescription>
            The query runs over the time range chosen in the header. Fill in its parameters.
          </DialogDescription>
        </div>
        {target ? (
          <form key={target.id} className="mt-4 flex min-h-0 flex-col gap-4" onSubmit={submit}>
            <DialogBody>
              {target.parameters.map((parameter) => (
                <Field key={parameter.name} name={`parameter.${parameter.name}`}>
                  <FieldLabel>{parameter.name}</FieldLabel>
                  <Input
                    required={parameter.default === undefined}
                    defaultValue={parameter.default ?? ""}
                    autoComplete="off"
                    spellCheck={false}
                    className="font-mono"
                  />
                  <FieldDescription>
                    Type {parameter.type}
                    {parameter.default === undefined
                      ? ". Required."
                      : ". Leave empty to use the default."}
                  </FieldDescription>
                  <FieldError match="valueMissing">Enter a value for {parameter.name}.</FieldError>
                </Field>
              ))}
            </DialogBody>
            <div className="flex flex-wrap justify-end gap-2">
              <DialogClose>Cancel</DialogClose>
              <Button type="submit" data-variant="solid">
                Run saved query
              </Button>
            </div>
          </form>
        ) : null}
      </DialogContent>
    </Dialog>
  );
}
