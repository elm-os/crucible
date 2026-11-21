"use client";

import { useAsyncOptions } from "@ui-sdk/react/async-options";
import {
  Autocomplete,
  AutocompleteContent,
  AutocompleteEmpty,
  AutocompleteInput,
  AutocompleteItem,
  AutocompleteLabel,
  AutocompleteList,
  AutocompleteStatus,
} from "@ui-sdk/react/autocomplete";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldLabel } from "@ui-sdk/react/field";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { type FormEvent, useId, useState } from "react";
import { getFilterValues } from "@/api/client";
import type { MetricsFilter } from "@/api/generated/MetricsFilter";
import { AlertIcon } from "@/components/icons";
import { formatCount } from "@/lib/format";
import { LEVELS, OBSERVATION_TYPES } from "@/lib/filters";
import { RANGE_PHRASE, type Range, rangeWindow } from "@/lib/time-range";
import { conditionsOf, filterOf, withoutValue, withValue } from "./widget-filter";

const FIELD_LABEL: Record<string, string> = {
  environment: "Environment",
  release: "Release",
  version: "Version",
  level: "Level",
  observation_type: "Type",
  user_id: "User",
  session_id: "Session",
};

// Fields with a closed set of values are chosen from it; the others suggest the values the range holds.
const CLOSED: Record<string, readonly string[]> = {
  level: LEVELS,
  observation_type: OBSERVATION_TYPES,
};

const MATCHES: { value: "is" | "is_not"; label: string }[] = [
  { value: "is", label: "is" },
  { value: "is_not", label: "is not" },
];

const label = (field: string) => FIELD_LABEL[field] ?? field.replaceAll("_", " ");

interface ValueItem {
  value: string;
  label: string;
  count: number;
}

// The widget's filter, edited as a list of conditions (a field is, or is not, one of its values), each a chip
// that removes it. The fields come from the metrics schema of the widget's data.
export function WidgetFilters({
  project,
  source,
  range,
  fields,
  filter,
  onChange,
}: {
  project: string;
  source: string;
  range: Range;
  fields: readonly string[];
  filter: MetricsFilter | undefined;
  onChange: (next: MetricsFilter | undefined) => void;
}) {
  const [picked, setPicked] = useState(fields[0] ?? "");
  const [match, setMatch] = useState<"is" | "is_not">("is");
  const [value, setValue] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const problemId = useId();
  const conditions = conditionsOf(filter);
  const field = fields.includes(picked) ? picked : (fields[0] ?? "");
  const closed = CLOSED[field];

  if (conditions === null) {
    return (
      <div className="flex flex-col items-start gap-2">
        <span className="text-label font-medium text-ink-soft">Filters</span>
        <p className="text-data text-ink-soft">
          This widget&apos;s filter combines conditions in a way the sheet can&apos;t show, and is
          kept as it is.
        </p>
        <Button onClick={() => onChange(undefined)}>Remove the filter</Button>
      </div>
    );
  }

  const add = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const text = value.trim();
    if (text === "") {
      setProblem(`Choose or type a ${label(field).toLowerCase()} to filter by.`);
      return;
    }
    onChange(filterOf(withValue(conditions, field, match === "is_not", text)));
    setValue("");
    setProblem(null);
  };

  return (
    <div className="flex flex-col gap-3">
      <form
        aria-label="Filter the widget's data"
        onSubmit={add}
        className="flex flex-wrap items-end gap-x-3 gap-y-2"
      >
        <Field className="flex flex-col gap-1">
          <FieldLabel className="text-label text-ink-soft">Filter by</FieldLabel>
          <Select
            items={fields.map((f) => ({ value: f, label: label(f) }))}
            value={field}
            onValueChange={(next) => {
              if (typeof next === "string") setPicked(next);
              setValue("");
              setProblem(null);
            }}
          >
            <SelectTrigger className="min-w-32 text-data">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {fields.map((f) => (
                <SelectItem key={f} value={f}>
                  {label(f)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Field>
        <Field className="flex flex-col gap-1">
          <FieldLabel className="text-label text-ink-soft">Match</FieldLabel>
          <Select
            items={MATCHES}
            value={match}
            onValueChange={(next) => setMatch(next === "is_not" ? "is_not" : "is")}
          >
            <SelectTrigger className="min-w-24 text-data">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {MATCHES.map((m) => (
                <SelectItem key={m.value} value={m.value}>
                  {m.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </Field>
        {closed ? (
          <Field className="flex min-w-40 flex-1 flex-col gap-1" invalid={problem !== null}>
            <FieldLabel className="text-label text-ink-soft">Value</FieldLabel>
            <Select
              items={closed.map((v) => ({ value: v, label: v }))}
              value={value === "" ? null : value}
              onValueChange={(next) => {
                setValue(typeof next === "string" ? next : "");
                setProblem(null);
              }}
              placeholder={`Choose a ${label(field).toLowerCase()}`}
            >
              <SelectTrigger
                className="text-data"
                aria-describedby={problem ? problemId : undefined}
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {closed.map((v) => (
                  <SelectItem key={v} value={v}>
                    {v}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
        ) : (
          <SuggestedValue
            key={`${source}|${field}|${range}`}
            project={project}
            source={source}
            field={field}
            range={range}
            value={value}
            invalid={problem !== null}
            describedBy={problem ? problemId : undefined}
            onValueChange={(next) => {
              setValue(next);
              setProblem(null);
            }}
          />
        )}
        <Button type="submit">Add filter</Button>
      </form>
      {problem ? (
        <p id={problemId} className="flex items-center gap-1.5 text-label text-ink-soft">
          <AlertIcon className="shrink-0" />
          {problem}
        </p>
      ) : null}
      {conditions.length > 0 ? (
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <ul aria-label="The widget's filters" className="flex min-w-0 max-w-full flex-wrap gap-2">
            {conditions.flatMap((c) =>
              c.values.map((v) => {
                const text = `${label(c.field)} ${c.negated ? "is not" : "is"} ${v}`;
                return (
                  <li key={`${c.field}|${c.negated}|${v}`} className="min-w-0 max-w-full">
                    <Button
                      aria-label={`Remove filter ${text}`}
                      onClick={() =>
                        onChange(filterOf(withoutValue(conditions, c.field, c.negated, v)))
                      }
                      className="max-w-full truncate text-data"
                    >
                      {text}
                      <span aria-hidden="true"> ×</span>
                    </Button>
                  </li>
                );
              }),
            )}
          </ul>
          <Button onClick={() => onChange(undefined)}>Clear filters</Button>
        </div>
      ) : null}
    </div>
  );
}

// A value typed freely, with the values the field holds in the range offered as the person types.
function SuggestedValue({
  project,
  source,
  field,
  range,
  value,
  invalid,
  describedBy,
  onValueChange,
}: {
  project: string;
  source: string;
  field: string;
  range: Range;
  value: string;
  invalid: boolean;
  describedBy: string | undefined;
  onValueChange: (next: string) => void;
}) {
  const values = useAsyncOptions<ValueItem>(
    async (query, { signal }) => {
      const prefix = query.trim();
      const page = await getFilterValues(
        project,
        {
          source,
          field,
          prefix: prefix === "" ? undefined : prefix,
          limit: 20,
          ...rangeWindow(range, Date.now()),
        },
        signal,
      );
      return page.data
        .filter((v) => v.value !== "")
        .map((v) => ({ value: v.value, label: v.value, count: v.count }));
    },
    { immediate: true },
  );
  const noun = label(field).toLowerCase();
  return (
    <Autocomplete
      items={values.items}
      filter={false}
      status={values.status}
      openOnFocus
      value={value}
      onValueChange={(next) => {
        onValueChange(next);
        values.setQuery(next);
      }}
    >
      <div className="flex min-w-40 flex-1 flex-col gap-1" data-invalid={invalid || undefined}>
        <AutocompleteLabel className="text-label text-ink-soft">Value</AutocompleteLabel>
        <AutocompleteInput
          className="text-data"
          placeholder={`a ${noun}`}
          aria-invalid={invalid || undefined}
          aria-describedby={describedBy}
          autoComplete="off"
          spellCheck={false}
        />
      </div>
      <AutocompleteContent className="text-data">
        <AutocompleteStatus>
          {(state) =>
            state.status === "error"
              ? `The ${noun} values couldn't load; type one as it is written.`
              : state.loading
                ? `Loading the ${noun} values`
                : ""
          }
        </AutocompleteStatus>
        <AutocompleteEmpty>
          {(state) =>
            !state.empty || state.loading || values.status === "error"
              ? null
              : state.query.trim() === ""
                ? `No ${noun} values in ${RANGE_PHRASE[range]}; type one as it is written.`
                : `No ${noun} in ${RANGE_PHRASE[range]} starts with “${state.query.trim()}”; add it as written.`
          }
        </AutocompleteEmpty>
        <AutocompleteList>
          {(state) =>
            (state.items as ValueItem[]).map((v) => (
              <AutocompleteItem
                key={v.value}
                value={v.value}
                className="flex justify-between gap-4"
              >
                <span className="truncate">{v.value}</span>
                <span className="text-ink-soft tabular-nums">{formatCount(v.count)}</span>
              </AutocompleteItem>
            ))
          }
        </AutocompleteList>
      </AutocompleteContent>
    </Autocomplete>
  );
}
