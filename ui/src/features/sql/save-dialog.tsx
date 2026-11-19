"use client";

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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { Textarea } from "@ui-sdk/react/textarea";
import { type FormEvent, useState } from "react";
import { asUiError, type UiError } from "@/api/errors";
import type { SavedQuery } from "@/api/generated/SavedQuery";
import type { SavedQueryInput } from "@/api/generated/SavedQueryInput";
import type { SavedQueryParameter } from "@/api/generated/SavedQueryParameter";
import { createSavedQuery, getSavedQuery, updateSavedQuery } from "./api";
import {
  type DetectedParameter,
  guessParameterType,
  PARAMETER_TYPES,
  parseParameterType,
} from "./params";
import { SqlErrorCard } from "./sql-result";

export type SaveMode = "new" | "update";

const TYPE_LABEL: Record<(typeof PARAMETER_TYPES)[number], string> = {
  string: "String",
  int: "Integer",
  float: "Decimal number",
  bool: "True or false",
  timestamp: "Timestamp",
  uuid: "UUID",
};

interface Props {
  project: string;
  mode: SaveMode;
  /** The saved query open in the editor; `update` changes it. */
  loaded: SavedQuery | null;
  sql: string;
  detected: readonly DetectedParameter[];
  onSaved: (saved: SavedQuery, mode: SaveMode) => void;
  onReloaded: (fresh: SavedQuery) => void;
}

export function SaveDialog({
  open,
  session,
  onClose,
  ...props
}: Props & { open: boolean; session: number; onClose: () => void }) {
  const updating = props.mode === "update" && props.loaded !== null;
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
          <DialogTitle>{updating ? "Save changes" : "Save as a new query"}</DialogTitle>
          <DialogDescription>
            {updating
              ? "The saved query gets the text in the editor and the details below."
              : "Saves the text in the editor so you can run it again."}
          </DialogDescription>
        </div>
        <SaveForm key={session} {...props} />
      </DialogContent>
    </Dialog>
  );
}

function SaveForm({ project, mode, loaded, sql, detected, onSaved, onReloaded }: Props) {
  const target = mode === "update" ? loaded : null;
  const [pending, setPending] = useState(false);
  const [failure, setFailure] = useState<UiError | null>(null);
  const declared = new Map(loaded?.parameters.map((parameter) => [parameter.name, parameter]));

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const parameters: SavedQueryParameter[] = detected.map(({ name, type }) => {
      const chosen = parseParameterType(form.get(`type.${name}`)) ?? guessParameterType(type);
      const fallback = String(form.get(`default.${name}`) ?? "");
      return fallback === "" ? { name, type: chosen } : { name, type: chosen, default: fallback };
    });
    const input: SavedQueryInput = {
      name: String(form.get("name")).trim(),
      description: String(form.get("description") ?? "").trim(),
      visibility: form.get("visibility") === "project" ? "project" : "private",
      sql,
      parameters,
    };
    setPending(true);
    setFailure(null);
    try {
      const saved =
        target === null
          ? await createSavedQuery(project, input)
          : await updateSavedQuery(project, target, input);
      onSaved(saved, target === null ? "new" : "update");
    } catch (error) {
      setFailure(asUiError(error));
    } finally {
      setPending(false);
    }
  };

  const reload = async () => {
    if (target === null) return;
    try {
      onReloaded(await getSavedQuery(project, target.id));
    } catch (error) {
      setFailure(asUiError(error));
    }
  };

  return (
    <form className="mt-4 flex flex-col gap-4" onSubmit={submit}>
      <Field name="name">
        <FieldLabel>Name</FieldLabel>
        <Input required maxLength={120} defaultValue={target?.name ?? ""} autoComplete="off" />
        <FieldError match="valueMissing">Give the saved query a name.</FieldError>
      </Field>
      <Field name="description">
        <FieldLabel>Description</FieldLabel>
        <Textarea rows={2} maxLength={2000} defaultValue={target?.description ?? ""} />
        <FieldDescription>Optional. What the query is for.</FieldDescription>
      </Field>
      <Field>
        <FieldLabel>Visibility</FieldLabel>
        <Select name="visibility" defaultValue={target?.visibility ?? "private"}>
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="private">Private: only you</SelectItem>
            <SelectItem value="project">Project: everyone in the project</SelectItem>
          </SelectContent>
        </Select>
      </Field>
      {detected.length > 0 ? (
        <fieldset className="flex flex-col gap-3">
          <legend className="text-data font-medium">Parameters</legend>
          <p className="text-label text-ink-soft">
            Found in the query text. The time window is bound for you and is not listed.
          </p>
          {detected.map(({ name, type }) => (
            <div key={name} className="grid gap-3 sm:grid-cols-2">
              <Field>
                <FieldLabel>
                  Type of <span className="font-mono">{name}</span>
                </FieldLabel>
                <Select
                  name={`type.${name}`}
                  defaultValue={declared.get(name)?.type ?? guessParameterType(type)}
                >
                  <SelectTrigger>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {PARAMETER_TYPES.map((option) => (
                      <SelectItem key={option} value={option}>
                        {TYPE_LABEL[option]}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
              <Field name={`default.${name}`}>
                <FieldLabel>
                  Default for <span className="font-mono">{name}</span>
                </FieldLabel>
                <Input
                  defaultValue={declared.get(name)?.default ?? ""}
                  autoComplete="off"
                  spellCheck={false}
                  className="font-mono"
                />
              </Field>
            </div>
          ))}
        </fieldset>
      ) : null}
      {failure ? (
        <SqlErrorCard error={failure}>
          {failure.code === "precondition_failed" ? (
            <Button onClick={reload}>Reload saved query</Button>
          ) : null}
        </SqlErrorCard>
      ) : null}
      <div className="flex flex-wrap justify-end gap-2">
        <DialogClose>Cancel</DialogClose>
        <Button type="submit" data-variant="solid" disabled={pending} aria-busy={pending}>
          {pending ? "Saving" : target === null ? "Save query" : "Save changes"}
        </Button>
      </div>
    </form>
  );
}
