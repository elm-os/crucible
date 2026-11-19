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
import { Field, FieldDescription, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { type FormEvent, useRef, useState } from "react";
import { createAlert, listChannels, patchAlert, previewAlert } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { AlertComparison } from "@/api/generated/AlertComparison";
import type { AlertNoDataPolicy } from "@/api/generated/AlertNoDataPolicy";
import type { AlertPreview } from "@/api/generated/AlertPreview";
import type { AlertRule } from "@/api/generated/AlertRule";
import type { AlertRuleInput } from "@/api/generated/AlertRuleInput";
import type { SessionProject } from "@/api/generated/SessionProject";
import { type CopyByCode, FormAlert, failureCopy, fieldFailed } from "@/components/form-alert";
import { TickIcon } from "@/components/icons";
import { scopedHref } from "@/components/shell/nav";
import { formatTime } from "@/lib/format";
import {
  AGGREGATION_LABEL,
  COMPARISONS,
  DIMENSIONS,
  formatValue,
  KIND_LABEL,
  MEASURES,
  measureOf,
  NO_DATA,
  NO_DIMENSION,
  seriesLabel,
  THRESHOLD_UNIT,
  thresholdFromStored,
  thresholdToStored,
} from "./format";
import { alertKeys } from "./shared";

const minutes = (count: number) => ({ value: String(count * 60), label: `${count} min` });
const hours = (count: number) => ({
  value: String(count * 3_600),
  label: count === 1 ? "1 hour" : `${count} hours`,
});
const INTERVALS = [1, 2, 5, 10, 15, 30].map(minutes).concat(hours(1));
const RENOTIFY = [5, 15, 30].map(minutes).concat([1, 4, 12, 24].map(hours));

const RULE_COPY: CopyByCode = {
  precondition_failed: {
    title: "This rule changed since you opened it.",
    detail: "Close this, check the rule, then make your change again.",
  },
  not_found: {
    title: "This rule no longer exists.",
    detail: "Someone deleted it. Close this and create a new rule.",
  },
};

const PREVIEW_COPY: CopyByCode = {
  telemetry_store_unavailable: {
    title: "The preview couldn't read the data.",
    detail:
      "The telemetry store isn't answering. The rule can still be saved; try the preview again in a moment.",
  },
};

// Seconds from a field of whole minutes.
function seconds(form: FormData, name: string): number {
  return Math.round(Number(form.get(name)) * 60);
}

function isComparison(value: string): value is AlertComparison {
  return COMPARISONS.some((option) => option.value === value);
}

function isNoData(value: string): value is AlertNoDataPolicy {
  return NO_DATA.some((option) => option.value === value);
}

export function RuleFormDialog({
  project,
  rule,
  label,
  className,
}: {
  project: SessionProject;
  rule?: AlertRule;
  label: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger className={className ?? "text-data"}>{label}</DialogTrigger>
      <DialogBackdrop />
      <DialogContent className="max-h-[90dvh] w-full max-w-2xl overflow-y-auto">
        {open ? <RuleForm project={project} rule={rule} onSaved={() => setOpen(false)} /> : null}
      </DialogContent>
    </Dialog>
  );
}

function RuleForm({
  project,
  rule,
  onSaved,
}: {
  project: SessionProject;
  rule?: AlertRule;
  onSaved: () => void;
}) {
  const queryClient = useQueryClient();
  const search = useSearchParams();
  const formRef = useRef<HTMLFormElement>(null);
  const [measure, setMeasure] = useState(rule?.measure ?? MEASURES[0].value);
  const option = measureOf(measure);
  const [aggregation, setAggregation] = useState(rule?.aggregation ?? option.aggregations[0]);
  const [chosen, setChosen] = useState<readonly string[]>(rule?.channels ?? []);
  const [badThreshold, setBadThreshold] = useState(false);

  const channels = useQuery({
    queryKey: alertKeys.channels(project.id),
    queryFn: ({ signal }) => listChannels(project.id, signal),
  });
  const save = useMutation({
    mutationFn: (input: AlertRuleInput) =>
      rule ? patchAlert(project.id, rule, input) : createAlert(project.id, input),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: alertKeys.rules(project.id) });
      onSaved();
    },
  });
  const preview = useMutation({
    mutationFn: (input: AlertRuleInput) => previewAlert(project.id, input),
  });
  const failure = save.isError ? asUiError(save.error) : null;
  const previewFailure = preview.isError ? asUiError(preview.error) : null;
  const failed = (name: string) => fieldFailed(failure, name) || fieldFailed(previewFailure, name);

  // The rule the form holds, or `null` when the threshold is not a number.
  const read = (form: HTMLFormElement): AlertRuleInput | null => {
    const data = new FormData(form);
    const threshold = thresholdToStored(option.unit, String(data.get("threshold")));
    setBadThreshold(threshold === null);
    if (threshold === null) return null;
    const comparison = String(data.get("comparison"));
    const noData = String(data.get("no_data"));
    const dimensions = [String(data.get("dimension_1")), String(data.get("dimension_2"))].filter(
      (dimension, at, all) => dimension !== NO_DIMENSION && all.indexOf(dimension) === at,
    );
    return {
      name: String(data.get("name")).trim(),
      measure,
      aggregation,
      dimensions,
      comparison: isComparison(comparison) ? comparison : "gt",
      threshold,
      window_secs: seconds(data, "window"),
      interval_secs: Number(data.get("interval")),
      for_secs: seconds(data, "for"),
      no_data: isNoData(noData) ? noData : "keep",
      no_data_for_secs: seconds(data, "no_data_for"),
      renotify_secs: Number(data.get("renotify")),
      channels: [...chosen],
    };
  };

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const input = read(event.currentTarget);
    if (input !== null) save.mutate(input);
  };

  const runPreview = () => {
    const form = formRef.current;
    if (form === null || !form.reportValidity()) return;
    const input = read(form);
    if (input !== null) preview.mutate(input);
  };

  const busy = save.isPending;
  return (
    <>
      <div className="flex flex-col gap-2">
        <DialogTitle>
          {rule ? `Edit ${rule.name}` : `New alert rule for ${project.name}`}
        </DialogTitle>
        <DialogDescription>
          A rule watches one measure. Each value of its group-by is checked on its own, up to 50.
        </DialogDescription>
      </div>
      <form
        ref={formRef}
        className="flex flex-col gap-6"
        onSubmit={submit}
        onInput={() => {
          if (failure) save.reset();
          setBadThreshold(false);
        }}
      >
        <fieldset className="flex flex-col gap-4">
          <legend className="mb-3 text-label font-medium text-ink-soft">Query</legend>
          <Field name="name" invalid={failed("name") || undefined}>
            <FieldLabel>Name</FieldLabel>
            <Input required maxLength={120} autoComplete="off" defaultValue={rule?.name} />
            <FieldError match="valueMissing">Enter a name, like Checkout errors.</FieldError>
            <FieldError match={failed("name")}>Use 1 to 120 characters.</FieldError>
          </Field>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field invalid={failed("measure") || undefined}>
              <FieldLabel>Measure</FieldLabel>
              <Select
                items={MEASURES}
                value={measure}
                onValueChange={(next) => {
                  if (next === null) return;
                  setMeasure(next);
                  setAggregation(measureOf(next).aggregations[0]);
                  preview.reset();
                }}
              >
                <SelectTrigger className="text-data">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {MEASURES.map((item) => (
                    <SelectItem key={item.value} value={item.value}>
                      {item.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <FieldError match={failed("measure")}>Choose another measure.</FieldError>
            </Field>
            {option.aggregations.length > 1 ? (
              <Field invalid={failed("aggregation") || undefined}>
                <FieldLabel>Percentile</FieldLabel>
                <Select
                  items={option.aggregations.map((value) => ({
                    value,
                    label: AGGREGATION_LABEL[value] ?? value,
                  }))}
                  value={aggregation}
                  onValueChange={(next) => {
                    if (next !== null) setAggregation(next);
                  }}
                >
                  <SelectTrigger className="text-data">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {option.aggregations.map((value) => (
                      <SelectItem key={value} value={value}>
                        {AGGREGATION_LABEL[value] ?? value}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
            ) : null}
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            {(["dimension_1", "dimension_2"] as const).map((name, at) => (
              <Field key={name} invalid={failed("dimensions") || undefined}>
                <FieldLabel>{at === 0 ? "Group by" : "Then by"}</FieldLabel>
                <Select
                  name={name}
                  items={DIMENSIONS}
                  defaultValue={rule?.dimensions[at] ?? NO_DIMENSION}
                >
                  <SelectTrigger className="text-data">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {DIMENSIONS.map((item) => (
                      <SelectItem key={item.value} value={item.value}>
                        {item.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                {at === 1 ? (
                  <FieldError match={failed("dimensions")}>
                    Choose two different dimensions, or None.
                  </FieldError>
                ) : null}
              </Field>
            ))}
          </div>
        </fieldset>

        <fieldset className="flex flex-col gap-4">
          <legend className="mb-3 text-label font-medium text-ink-soft">Condition</legend>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field>
              <FieldLabel>Fire when the value</FieldLabel>
              <Select name="comparison" items={COMPARISONS} defaultValue={rule?.comparison ?? "gt"}>
                <SelectTrigger className="text-data">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {COMPARISONS.map((item) => (
                    <SelectItem key={item.value} value={item.value}>
                      {item.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
            <Field name="threshold" invalid={badThreshold || failed("threshold") || undefined}>
              <FieldLabel>
                Threshold{THRESHOLD_UNIT[option.unit] ? `, in ${THRESHOLD_UNIT[option.unit]}` : ""}
              </FieldLabel>
              <Input
                required
                inputMode="decimal"
                autoComplete="off"
                defaultValue={rule ? thresholdFromStored(rule.unit, rule.threshold) : undefined}
              />
              <FieldError match="valueMissing">Enter the threshold.</FieldError>
              <FieldError match={badThreshold || failed("threshold")}>
                Enter a number, like 5 or 0.25.
              </FieldError>
            </Field>
          </div>
          <div className="grid gap-4 sm:grid-cols-3">
            <Field name="window" invalid={failed("window_secs") || undefined}>
              <FieldLabel>Over the last (minutes)</FieldLabel>
              <Input
                type="number"
                required
                min={1}
                max={1440}
                step={1}
                defaultValue={String((rule?.window_secs ?? 300) / 60)}
              />
              <FieldError match>Use a whole number of minutes from 1 to 1440.</FieldError>
            </Field>
            <Field invalid={failed("interval_secs") || undefined}>
              <FieldLabel>Check every</FieldLabel>
              <Select
                name="interval"
                items={INTERVALS}
                defaultValue={String(rule?.interval_secs ?? 60)}
              >
                <SelectTrigger className="text-data">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {INTERVALS.map((item) => (
                    <SelectItem key={item.value} value={item.value}>
                      {item.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
            <Field name="for" invalid={failed("for_secs") || undefined}>
              <FieldLabel>For at least (minutes)</FieldLabel>
              <Input
                type="number"
                required
                min={0}
                max={1440}
                step={1}
                defaultValue={String((rule?.for_secs ?? 0) / 60)}
              />
              <FieldDescription>0 fires at the first check that breaches.</FieldDescription>
              <FieldError match>Use a whole number of minutes from 0 to 1440.</FieldError>
            </Field>
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field>
              <FieldLabel>When there is no data</FieldLabel>
              <Select name="no_data" items={NO_DATA} defaultValue={rule?.no_data ?? "keep"}>
                <SelectTrigger className="text-data">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {NO_DATA.map((item) => (
                    <SelectItem key={item.value} value={item.value}>
                      {item.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
            <Field name="no_data_for" invalid={failed("no_data_for_secs") || undefined}>
              <FieldLabel>After no data for (minutes)</FieldLabel>
              <Input
                type="number"
                required
                min={0}
                max={1440}
                step={1}
                defaultValue={String((rule?.no_data_for_secs ?? 300) / 60)}
              />
              <FieldError match>Use a whole number of minutes from 0 to 1440.</FieldError>
            </Field>
          </div>
        </fieldset>

        <fieldset className="flex flex-col gap-4">
          <legend className="mb-3 text-label font-medium text-ink-soft">Notify</legend>
          <div className="flex flex-col gap-2" data-channels>
            <p className="text-data font-medium">Channels</p>
            {channels.isPending ? (
              <output className="text-data text-ink-soft">Loading channels</output>
            ) : channels.isError ? (
              <FormAlert
                copy={failureCopy(asUiError(channels.error), "Couldn't load the channels.")}
                requestId={asUiError(channels.error).requestId}
                action={<Button onClick={() => channels.refetch()}>Retry channels</Button>}
              />
            ) : channels.data.data.length === 0 ? (
              <p className="text-data text-ink-soft">
                No channels yet, so this rule records its state without notifying anyone. Create one
                under <Link href={scopedHref("/alerts/channels", search)}>Channels</Link>, then edit
                the rule.
              </p>
            ) : (
              channels.data.data.map((channel) => (
                <label
                  key={channel.id}
                  data-slot="label"
                  className="flex items-center gap-2 text-data"
                >
                  <Checkbox
                    checked={chosen.includes(channel.id)}
                    onCheckedChange={(checked) =>
                      setChosen((before) =>
                        checked === true
                          ? [...before.filter((id) => id !== channel.id), channel.id]
                          : before.filter((id) => id !== channel.id),
                      )
                    }
                  >
                    <CheckboxIndicator>
                      <TickIcon aria-hidden="true" />
                    </CheckboxIndicator>
                  </Checkbox>
                  {channel.name} ({KIND_LABEL[channel.kind]}
                  {channel.enabled ? "" : ", turned off"})
                </label>
              ))
            )}
            {failed("channels") ? (
              <p role="alert" className="text-label">
                A chosen channel no longer exists, or more than 10 are chosen. Check the list.
              </p>
            ) : null}
          </div>
          <Field invalid={failed("renotify_secs") || undefined}>
            <FieldLabel>While it keeps firing, notify again every</FieldLabel>
            <Select
              name="renotify"
              items={RENOTIFY}
              defaultValue={String(rule?.renotify_secs ?? 14_400)}
            >
              <SelectTrigger className="text-data">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {RENOTIFY.map((item) => (
                  <SelectItem key={item.value} value={item.value}>
                    {item.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
        </fieldset>

        <section
          aria-label="Preview"
          className="flex flex-col gap-3 border-t border-ash-border pt-4"
        >
          <div className="flex flex-wrap items-center gap-3">
            <Button onClick={runPreview} disabled={preview.isPending} aria-busy={preview.isPending}>
              {preview.isPending ? "Reading the last window" : "Preview"}
            </Button>
            <p className="text-label text-ink-soft">
              Reads what the rule would see now. Nothing is saved and nobody is notified.
            </p>
          </div>
          {previewFailure ? (
            <FormAlert
              copy={failureCopy(previewFailure, "The preview couldn't run.", PREVIEW_COPY)}
              requestId={previewFailure.requestId}
            />
          ) : null}
          {preview.isSuccess ? <PreviewResult preview={preview.data} /> : null}
        </section>

        {failure ? (
          <FormAlert
            copy={failureCopy(failure, "Couldn't save the rule.", RULE_COPY)}
            requestId={failure.requestId}
          />
        ) : null}
        <div className="flex justify-end gap-2">
          <DialogClose>Cancel</DialogClose>
          <Button type="submit" data-variant="solid" disabled={busy} aria-busy={busy}>
            {busy ? "Saving the rule" : rule ? "Save changes" : "Create rule"}
          </Button>
        </div>
      </form>
    </>
  );
}

export function PreviewResult({ preview }: { preview: AlertPreview }) {
  const breaching = preview.series.filter((series) => series.breach).length;
  if (preview.series.length === 0) {
    return (
      <p data-preview="empty" className="text-data text-ink-soft">
        No data between {formatTime(preview.from)} and {formatTime(preview.to)}. The rule would
        follow its no-data setting.
      </p>
    );
  }
  return (
    <div data-preview="series" className="flex flex-col gap-2">
      <p className="text-data">
        {breaching === 0
          ? "Nothing would fire now."
          : breaching === 1
            ? "1 series would breach now."
            : `${breaching} series would breach now.`}{" "}
        <span className="text-ink-soft">
          Window: {formatTime(preview.from)} to {formatTime(preview.to)}.
        </span>
      </p>
      {preview.truncated ? (
        <p className="text-label text-ink-soft">
          More than 50 series match. The rule tracks the 50 nearest to the threshold.
        </p>
      ) : null}
      <div className="relative max-h-60 overflow-auto">
        <table className="w-full text-start text-data">
          <thead>
            <tr className="text-label text-ink-soft">
              <th scope="col" className="h-8 pe-4 text-start font-medium">
                Series
              </th>
              <th scope="col" className="h-8 pe-4 text-end font-medium">
                Value
              </th>
              <th scope="col" className="h-8 text-start font-medium">
                Result
              </th>
            </tr>
          </thead>
          <tbody>
            {preview.series.map((series) => (
              <tr key={seriesLabel(series.key)} className="h-9 border-t border-ash-border">
                <td className="pe-4">{seriesLabel(series.key)}</td>
                <td className="pe-4 text-end tabular-nums">
                  {formatValue(preview.unit, series.value)}
                </td>
                <td>{series.breach ? "Breaches" : "Within the threshold"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
