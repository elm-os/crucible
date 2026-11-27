"use client";

import type { UseQueryResult } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Field, FieldDescription, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@ui-sdk/react/select";
import { Switch, SwitchThumb } from "@ui-sdk/react/switch";
import { Textarea } from "@ui-sdk/react/textarea";
import { useEffect, useId, useState } from "react";
import type { ChartForm } from "@/api/generated/ChartForm";
import type { Granularity } from "@/api/generated/Granularity";
import type { McpSideParam } from "@/api/generated/McpSideParam";
import type { MeasureRequest } from "@/api/generated/MeasureRequest";
import type { MetricsFilter } from "@/api/generated/MetricsFilter";
import type { MetricsSchema } from "@/api/generated/MetricsSchema";
import type { SourceParam } from "@/api/generated/SourceParam";
import type { Widget } from "@/api/generated/Widget";
import type { WidgetKind } from "@/api/generated/WidgetKind";
import type { WidgetQuery } from "@/api/generated/WidgetQuery";
import { asUiError } from "@/api/errors";
import { FormAlert } from "@/components/form-alert";
import { AlertIcon } from "@/components/icons";
import { CAPTURE_SOURCES, SIDES } from "@/features/alerts/format";
import type { Range } from "@/lib/time-range";
import {
  KIND_LABEL,
  KINDS,
  kindShape,
  MAX_SERIES_KEYS,
  MAX_TEXT,
  MAX_TITLE,
  maxChartKeys,
  shapeProblem,
} from "./rules";
import { WidgetFilters } from "./widget-filters";
import { WidgetView } from "./widget-view";

const SOURCE_LABEL: Record<string, string> = {
  spans: "Agent spans",
  traces: "Traces",
  mcp_ops: "MCP operations",
};

const GRANULARITY: { value: Granularity; label: string }[] = [
  { value: "auto", label: "Automatic" },
  { value: "minute", label: "Minute" },
  { value: "five_minutes", label: "5 minutes" },
  { value: "fifteen_minutes", label: "15 minutes" },
  { value: "hour", label: "Hour" },
  { value: "day", label: "Day" },
  { value: "week", label: "Week" },
];

const NONE = "none";
const PREVIEW_DELAY_MS = 400;

export interface WidgetDraft {
  query?: WidgetQuery;
  display: Widget["display"];
}

function measureKey(m: MeasureRequest): string {
  return `${m.measure}|${m.aggregation}`;
}

function parseMeasure(value: string): MeasureRequest {
  const [measure = "", aggregation = ""] = value.split("|");
  return { measure, aggregation };
}

// The query a kind can hold, made from what the form held for another kind: as much of it as fits.
function fitKind(
  kind: WidgetKind,
  measures: MeasureRequest[],
  dimensions: string[],
  available: string[],
): { measures: MeasureRequest[]; dimensions: string[] } {
  const shape = kindShape(kind);
  const kept = measures.slice(0, shape.measures[1]);
  let dims = dimensions.slice(0, shape.dimensions[1]);
  if (dims.length < shape.dimensions[0] && available[0]) dims = [available[0]];
  return { measures: kept, dimensions: dims };
}

export function WidgetForm({
  project,
  schemaQuery,
  range,
  initial,
  onApply,
  onCancel,
}: {
  project: string;
  schemaQuery: UseQueryResult<MetricsSchema>;
  range: Range;
  initial: WidgetDraft;
  onApply: (draft: WidgetDraft) => void;
  onCancel: () => void;
}) {
  const schema = schemaQuery.data;
  const sources = schema?.sources ?? [];
  const [kind, setKind] = useState<WidgetKind>(initial.display.kind);
  const [title, setTitle] = useState(initial.display.title);
  const [text, setText] = useState(initial.display.text ?? "");
  const [chart, setChart] = useState<ChartForm>(initial.display.chart ?? "line");
  const [source, setSource] = useState(initial.query?.source ?? sources[0]?.source ?? "spans");
  const schemaOf = sources.find((s) => s.source === source);
  const catalog = schemaOf?.measures ?? [];
  const dimensionIds = (schemaOf?.dimensions ?? []).map((d) => d.id);
  const [measures, setMeasures] = useState<MeasureRequest[]>(
    initial.query?.measures ?? [{ measure: "count", aggregation: "sum" }],
  );
  const [dimensions, setDimensions] = useState<string[]>(initial.query?.dimensions ?? []);
  const [topN, setTopN] = useState<number | null>(initial.query?.top_n ?? null);
  const [granularity, setGranularity] = useState<Granularity>(initial.query?.granularity ?? "auto");
  const [side, setSide] = useState<McpSideParam>(initial.query?.side ?? "server");
  const [capture, setCapture] = useState<SourceParam>(initial.query?.capture_source ?? "auto");
  const [callerFaults, setCallerFaults] = useState(initial.query?.include_caller_faults ?? false);
  const [filter, setFilter] = useState<MetricsFilter | undefined>(initial.query?.filter);
  const filterFields = (schemaOf?.filter_fields ?? [])
    .filter((f) => f.operator === "in")
    .map((f) => f.field);
  const markdown = kind === "markdown";
  const shape = kindShape(kind);
  const maxKeys = kind === "timeseries" ? maxChartKeys(measures, catalog) : MAX_SERIES_KEYS;
  const keys = dimensions.length === 0 ? null : Math.min(topN ?? maxKeys, maxKeys);

  // An `mcp_ops` query reads one side under a capture source, with or without the calls the caller caused;
  // no other source takes these. The automatic source and leaving caller faults out stay off the wire.
  const mcp = source === "mcp_ops";
  const query: WidgetQuery | undefined = markdown
    ? undefined
    : {
        ...initial.query,
        source,
        measures,
        dimensions: dimensions.length > 0 ? dimensions : undefined,
        filter,
        top_n: keys ?? undefined,
        granularity: granularity === "auto" ? undefined : granularity,
        side: mcp ? side : undefined,
        capture_source: mcp && capture !== "auto" ? capture : undefined,
        include_caller_faults: (mcp && callerFaults) || undefined,
      };
  const display: Widget["display"] = {
    kind,
    title: title.trim(),
    chart: kind === "timeseries" ? chart : undefined,
    text: markdown ? text : undefined,
  };
  const problems = [
    title.trim() === "" ? "Give the widget a title." : null,
    markdown && text.trim() === "" ? "Write the text the widget shows." : null,
    !markdown && schema === undefined ? "The measures haven't loaded yet." : null,
    markdown || schema === undefined
      ? null
      : shapeProblem(kind, { measures, dimensions, topN: keys }, catalog),
  ].filter((p): p is string => p !== null);

  // The preview reads the draft a moment after it stops changing, not at every keystroke.
  const [previewed, setPreviewed] = useState<WidgetDraft>({ query, display });
  const previewKey = JSON.stringify({ query, display });
  useEffect(() => {
    const timer = setTimeout(() => setPreviewed(JSON.parse(previewKey)), PREVIEW_DELAY_MS);
    return () => clearTimeout(timer);
  }, [previewKey]);

  const changeKind = (next: WidgetKind) => {
    setKind(next);
    if (next === "markdown") return;
    const fitted = fitKind(next, measures, dimensions, dimensionIds);
    setMeasures(
      fitted.measures.length > 0 ? fitted.measures : [{ measure: "count", aggregation: "sum" }],
    );
    setDimensions(fitted.dimensions);
  };
  const changeSource = (next: string) => {
    setSource(next);
    // Each source has its own filter fields.
    setFilter(undefined);
    const first = sources.find((s) => s.source === next)?.measures[0];
    setMeasures(first ? [{ measure: first.id, aggregation: first.aggregations[0] ?? "sum" }] : []);
    const dims = sources.find((s) => s.source === next)?.dimensions.map((d) => d.id) ?? [];
    setDimensions(kindShape(kind).dimensions[0] > 0 && dims[0] ? [dims[0]] : []);
  };
  const measureOptions = catalog.flatMap((m) =>
    m.aggregations.map((a) => ({
      value: measureKey({ measure: m.id, aggregation: a }),
      label: a === "sum" ? m.label : `${m.label} ${a}`,
    })),
  );
  const problemId = useId();

  return (
    <div className="flex flex-col gap-5">
      <Field>
        <FieldLabel>Title</FieldLabel>
        <Input value={title} onValueChange={setTitle} maxLength={MAX_TITLE} autoComplete="off" />
      </Field>
      <div className="flex flex-col gap-1">
        <span className="text-label font-medium text-ink-soft" id={`${problemId}-kind`}>
          Shows
        </span>
        <SegmentedControl
          aria-labelledby={`${problemId}-kind`}
          value={kind}
          onValueChange={(next) => {
            const found = KINDS.find((k) => k === next);
            if (found) changeKind(found);
          }}
        >
          <SegmentedControlIndicator />
          {KINDS.map((k) => (
            <SegmentedControlItem key={k} value={k} className="text-data">
              {KIND_LABEL[k]}
            </SegmentedControlItem>
          ))}
        </SegmentedControl>
      </div>
      {markdown ? (
        <Field>
          <FieldLabel>Text</FieldLabel>
          <Textarea value={text} onValueChange={setText} maxLength={MAX_TEXT} rows={8} />
          <FieldDescription>
            Headings with #, lists with -, **bold**, *italic*, `code` and links to http or https
            addresses.
          </FieldDescription>
        </Field>
      ) : schemaQuery.isPending ? (
        <output className="text-data text-ink-soft">Loading the measures</output>
      ) : schemaQuery.isError ? (
        <FormAlert
          copy={{
            title: "The measures couldn't load.",
            detail: "A widget's query is built from them. Try again in a moment.",
            reference: asUiError(schemaQuery.error).reference,
          }}
          action={<Button onClick={() => schemaQuery.refetch()}>Retry the measures</Button>}
        />
      ) : (
        <fieldset className="flex flex-col gap-4">
          <legend className="mb-2 text-label font-medium text-ink-soft">Query</legend>
          {sources.length > 1 ? (
            <Field>
              {/* Not "Source": an agent's step by that name would also find Capture source. */}
              <FieldLabel>Data</FieldLabel>
              <Select
                items={sources.map((s) => ({
                  value: s.source,
                  label: SOURCE_LABEL[s.source] ?? s.source,
                }))}
                value={source}
                onValueChange={(next) => next !== null && changeSource(next)}
              >
                <SelectTrigger className="text-data">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {sources.map((s) => (
                    <SelectItem key={s.source} value={s.source}>
                      {SOURCE_LABEL[s.source] ?? s.source}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
          ) : null}
          {mcp ? (
            <>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field>
                  <FieldLabel>Side</FieldLabel>
                  <Select
                    items={SIDES}
                    value={side}
                    onValueChange={(next) => setSide(next === "client" ? "client" : "server")}
                  >
                    <SelectTrigger className="text-data">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {SIDES.map((s) => (
                        <SelectItem key={s.value} value={s.value}>
                          {s.label}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </Field>
                <Field>
                  <FieldLabel>Capture source</FieldLabel>
                  <Select
                    items={CAPTURE_SOURCES}
                    value={capture}
                    onValueChange={(next) => {
                      const found = CAPTURE_SOURCES.find((c) => c.value === next);
                      if (found) setCapture(found.value);
                    }}
                  >
                    <SelectTrigger className="text-data">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {CAPTURE_SOURCES.map((c) => (
                        <SelectItem key={c.value} value={c.value}>
                          {c.label}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <FieldDescription>
                    Automatic reads each server from its SDK, else its proxy, else OTLP.
                  </FieldDescription>
                </Field>
              </div>
              <Field className="grid grid-cols-[auto_minmax(0,1fr)] items-center gap-x-2 gap-y-1">
                <Switch checked={callerFaults} onCheckedChange={setCallerFaults}>
                  <SwitchThumb />
                </Switch>
                <FieldLabel className="text-data">Include caller faults</FieldLabel>
                <FieldDescription className="col-span-2">
                  Calls that failed because of what the caller sent, such as invalid arguments,
                  count only when this is on.
                </FieldDescription>
              </Field>
            </>
          ) : null}
          {measures.map((m, i) => (
            <div key={`${measureKey(m)}-${i}`} className="flex items-end gap-2">
              <Field className="grow">
                <FieldLabel>{measures.length > 1 ? `Measure ${i + 1}` : "Measure"}</FieldLabel>
                <Select
                  items={measureOptions}
                  value={measureKey(m)}
                  onValueChange={(next) => {
                    if (next === null) return;
                    setMeasures(measures.map((old, j) => (j === i ? parseMeasure(next) : old)));
                  }}
                >
                  <SelectTrigger className="text-data">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {measureOptions.map((o) => (
                      <SelectItem key={o.value} value={o.value}>
                        {o.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </Field>
              {measures.length > 1 ? (
                <Button
                  aria-label={`Remove measure ${i + 1}`}
                  onClick={() => setMeasures(measures.filter((_, j) => j !== i))}
                >
                  Remove
                </Button>
              ) : null}
            </div>
          ))}
          {measures.length < shape.measures[1] && measureOptions[0] ? (
            <div>
              <Button
                onClick={() =>
                  setMeasures([...measures, parseMeasure(measureOptions[0]?.value ?? "")])
                }
              >
                Add measure
              </Button>
            </div>
          ) : null}
          {shape.dimensions[1] > 0 && dimensionIds.length > 0 ? (
            <div className="grid grid-cols-2 gap-3">
              {[0, 1].slice(0, Math.min(shape.dimensions[1], dimensions.length + 1)).map((at) => {
                const options = [
                  ...(at >= shape.dimensions[0] ? [{ value: NONE, label: "None" }] : []),
                  ...dimensionIds
                    .filter((d) => !dimensions.includes(d) || dimensions[at] === d)
                    .map((d) => ({ value: d, label: d.replaceAll("_", " ") })),
                ];
                return (
                  <Field key={at}>
                    <FieldLabel>{at === 0 ? "Group by" : "Then by"}</FieldLabel>
                    <Select
                      items={options}
                      value={dimensions[at] ?? NONE}
                      onValueChange={(next) => {
                        if (next === null) return;
                        const kept = dimensions.slice(0, at);
                        setDimensions(next === NONE ? kept : [...kept, next]);
                      }}
                    >
                      <SelectTrigger className="text-data">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        {options.map((o) => (
                          <SelectItem key={o.value} value={o.value}>
                            {o.label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </Field>
                );
              })}
            </div>
          ) : null}
          {dimensions.length > 0 && maxKeys > 0 ? (
            <Field>
              <FieldLabel>Keys shown</FieldLabel>
              <Select
                items={Array.from({ length: maxKeys }, (_, n) => ({
                  value: String(n + 1),
                  label: String(n + 1),
                }))}
                value={String(keys ?? maxKeys)}
                onValueChange={(next) => next !== null && setTopN(Number(next))}
              >
                <SelectTrigger className="text-data">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {Array.from({ length: maxKeys }, (_, n) => (
                    <SelectItem key={n + 1} value={String(n + 1)}>
                      {n + 1}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              <FieldDescription>
                The keys with the largest values; the rest add up to Other where the measure adds
                up.
              </FieldDescription>
            </Field>
          ) : null}
          {filterFields.length > 0 || filter !== undefined ? (
            <WidgetFilters
              project={project}
              source={source}
              range={range}
              fields={filterFields}
              filter={filter}
              onChange={setFilter}
            />
          ) : null}
          <Field>
            <FieldLabel>Buckets</FieldLabel>
            <Select
              items={GRANULARITY}
              value={granularity}
              onValueChange={(next) => {
                const found = GRANULARITY.find((g) => g.value === next);
                if (found) setGranularity(found.value);
              }}
            >
              <SelectTrigger className="text-data">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {GRANULARITY.map((g) => (
                  <SelectItem key={g.value} value={g.value}>
                    {g.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <FieldDescription>
              Automatic picks the finest that fits the time range.
            </FieldDescription>
          </Field>
          {kind === "timeseries" ? (
            <div className="flex flex-col gap-1">
              <span className="text-label font-medium text-ink-soft" id={`${problemId}-chart`}>
                Draw as
              </span>
              <SegmentedControl
                aria-labelledby={`${problemId}-chart`}
                value={chart}
                onValueChange={(next) =>
                  setChart(next === "bar" || next === "area" ? next : "line")
                }
              >
                <SegmentedControlIndicator />
                <SegmentedControlItem value="line" className="text-data">
                  Lines
                </SegmentedControlItem>
                <SegmentedControlItem value="bar" className="text-data">
                  Bars
                </SegmentedControlItem>
                <SegmentedControlItem value="area" className="text-data">
                  Areas
                </SegmentedControlItem>
              </SegmentedControl>
            </div>
          ) : null}
        </fieldset>
      )}
      <div className="flex flex-col gap-2">
        <h3 className="text-label font-medium text-ink-soft">Preview</h3>
        {problems.length > 0 ? (
          <p className="text-data text-ink-soft">The preview shows once the widget is complete.</p>
        ) : (
          <WidgetView
            widget={{ id: "preview", ...previewed, layout: { x: 0, y: 0, w: 6, h: 3 } }}
            context={{
              project,
              owner: "preview",
              range,
              catalog,
              onWiden: () => undefined,
            }}
          />
        )}
      </div>
      {problems.length > 0 ? (
        <ul id={problemId} className="flex flex-col gap-1 text-data">
          {problems.map((p) => (
            <li key={p} className="flex items-center gap-2">
              <AlertIcon className="shrink-0" />
              {p}
            </li>
          ))}
        </ul>
      ) : null}
      <div className="flex justify-end gap-2">
        <Button onClick={onCancel}>Cancel</Button>
        <Button
          data-variant="solid"
          aria-describedby={problems.length > 0 ? problemId : undefined}
          aria-disabled={problems.length > 0 || undefined}
          onClick={() => {
            if (problems.length === 0) onApply({ query, display });
          }}
        >
          Apply
        </Button>
      </div>
    </div>
  );
}
