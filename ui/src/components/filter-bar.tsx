"use client";

import { Button } from "@ui-sdk/react/button";
import { Field, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { type FormEvent, useId, useState } from "react";
import type { ListFilters } from "@/api/types";
import { AlertIcon } from "@/components/icons";
import { conditions, type FilterField, withCondition, withoutCondition } from "@/lib/filters";

const NAME = "name";

// Two fields the list cannot combine, and what to say when someone tries.
export type FilterConflict = readonly [string, string, string];

export function FilterBar({
  filters,
  onChange,
  fields,
  conflicts = [],
  noun,
}: {
  filters: ListFilters;
  onChange: (next: ListFilters) => void;
  fields: readonly FilterField[];
  conflicts?: readonly FilterConflict[];
  noun: string;
}) {
  const [field, setField] = useState<string>(NAME);
  const [value, setValue] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const problemId = useId();
  const chosen = fields.find((f) => f.key === field);
  const active = conditions(filters);
  const options = [{ value: NAME, label: "Name contains" }].concat(
    fields.map((f) => ({ value: f.key, label: f.label })),
  );

  const add = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const text = value.trim();
    if (text === "") {
      setProblem("Enter a value to filter by.");
      return;
    }
    if (field === NAME) {
      onChange({ ...filters, name: text });
    } else if (chosen) {
      if (chosen.key === "metadata" && !/^[^=]+=/.test(text)) {
        setProblem("Write metadata as key=value, for example team=search.");
        return;
      }
      const conflict = conflicts.find(
        ([a, b]) =>
          (a === chosen.key && filters[b as keyof ListFilters] !== undefined) ||
          (b === chosen.key && filters[a as keyof ListFilters] !== undefined),
      );
      if (conflict) {
        setProblem(conflict[2]);
        return;
      }
      onChange(withCondition(filters, chosen.key, text));
    }
    setValue("");
    setProblem(null);
  };

  return (
    <div className="mb-4 flex flex-col gap-3">
      <form
        aria-label={`Filter ${noun}`}
        onSubmit={add}
        className="flex flex-wrap items-end gap-x-3 gap-y-2"
      >
        <Field className="flex flex-col gap-1">
          <FieldLabel className="text-label text-ink-soft">Filter by</FieldLabel>
          <Select
            items={options}
            value={field}
            onValueChange={(next) => {
              if (typeof next === "string") setField(next);
              setValue("");
              setProblem(null);
            }}
          >
            <SelectTrigger className="min-w-36 text-data">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {options.map((option) => (
                <SelectItem key={option.value} value={option.value}>
                  {option.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Field>
        <Field className="flex min-w-48 flex-1 flex-col gap-1" invalid={problem !== null}>
          <FieldLabel className="text-label text-ink-soft">Value</FieldLabel>
          {chosen?.values ? (
            <Select
              items={chosen.values.map((v) => ({ value: v, label: v }))}
              value={value === "" ? null : value}
              onValueChange={(next) => {
                setValue(typeof next === "string" ? next : "");
                setProblem(null);
              }}
              placeholder={`Choose a ${chosen.label.toLowerCase()}`}
            >
              <SelectTrigger
                className="text-data"
                aria-describedby={problem ? problemId : undefined}
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {chosen.values.map((v) => (
                  <SelectItem key={v} value={v}>
                    {v}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : (
            <Input
              value={value}
              onValueChange={(next) => {
                setValue(next);
                setProblem(null);
              }}
              placeholder={chosen?.placeholder ?? "text in the name"}
              autoComplete="off"
              spellCheck={false}
              aria-describedby={problem ? problemId : undefined}
            />
          )}
        </Field>
        <Button type="submit">Add filter</Button>
      </form>
      {problem ? (
        <p id={problemId} className="flex items-center gap-1.5 text-label text-ink-soft">
          <AlertIcon className="shrink-0" />
          {problem}
        </p>
      ) : null}
      {active.length > 0 ? (
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <ul aria-label="Active filters" className="flex min-w-0 max-w-full flex-wrap gap-2">
            {active.map((condition) => (
              <li key={`${condition.key}=${condition.value}`} className="min-w-0 max-w-full">
                <Button
                  aria-label={`Remove filter ${condition.label}`}
                  onClick={() => onChange(withoutCondition(filters, condition))}
                  className="max-w-full truncate text-data"
                >
                  {condition.label}
                  <span aria-hidden="true"> ×</span>
                </Button>
              </li>
            ))}
          </ul>
          <Button onClick={() => onChange({})}>Clear filters</Button>
        </div>
      ) : null}
    </div>
  );
}
