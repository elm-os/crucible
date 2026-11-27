"use client";

import { Toggle } from "@ui-sdk/react/toggle";
import { type PointerEvent, type RefObject, useLayoutEffect, useRef, useState } from "react";
import {
  barPath,
  bucketTimes,
  type ChartForm,
  type ChartLayout,
  type ChartSeries,
  chartModel,
  MAX_SERIES,
  markerPath,
  mergeBuckets,
  mergeFactor,
  minStep,
  nearestBucket,
  regularize,
  type SeriesStyle,
  seriesStyle,
  tableRows,
  timeTickFormat,
  valuesAt,
} from "@/primitives/chart/chart-model";

const bucketFormat = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  hour: "numeric",
  minute: "2-digit",
});
const clockFormat = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });

// The plot is drawn at the size it is shown, so axis text stays at the label size at every width.
const PAD = { top: 16, right: 16, bottom: 30 };
const MAX_BAR = 24;
const BAR_GAP = 2;

/**
 * Lines for time series, bars for counts per bucket, or lines over a light wash down to zero. Up to four series,
 * each with its own dash. Hover or focus the plot to read a bucket: the pointer, or the arrow keys, Home and End.
 * A table view lists every value.
 */
export function TimeSeriesChart({
  title,
  series,
  form = "line",
  family = false,
  format,
  axisFormat,
  markers = [],
  summable = false,
  step,
}: {
  title: string;
  series: ChartSeries[];
  form?: ChartForm;
  /** Percentiles of one measure: one ink family, told apart by dash. */
  family?: boolean;
  /** How a value reads in the tooltip and the table. */
  format: (value: number) => string;
  /** How a value reads on the axis, where ticks are round; defaults to `format`. */
  axisFormat?: (value: number) => string;
  /** Instants to mark with a vertical rule, such as deploys, each with a label. */
  markers?: { t: number; label: string }[];
  /** Counts and sums: a missing bucket is zero, and narrow bars may be added together. */
  summable?: boolean;
  /** The bucket width in milliseconds, when the server says it. */
  step?: number | null;
}) {
  const box = useRef<HTMLDivElement>(null);
  const measured = useWidth(box);
  // Before the first measurement (and where nothing has layout, as in tests) the plot assumes a common width;
  // the layout effect measures before the first paint, so a browser never shows it.
  const width = measured > 0 ? measured : 640;
  const [active, setActive] = useState<number | null>(null);
  const [table, setTable] = useState(false);
  const tick = axisFormat ?? format;
  const height = width < 480 ? 180 : 220;

  const base = regularize(series.slice(0, MAX_SERIES), { step, zero: summable });
  // A first pass finds the y-axis labels, so the gutter is as wide as the widest one and no wider.
  const probe = chartModel(base, { width: 400, height, ...PAD, left: 0 }, form);
  const left = Math.ceil(Math.max(16, ...probe.yTicks.map((t) => textWidth(tick(t.value))))) + 10;
  const plotWidth = Math.max(1, width - left - PAD.right);
  const baseStep = step ?? minStep(bucketTimes(base));
  const factor =
    form === "bar" && summable ? mergeFactor(bucketTimes(base).length, plotWidth, baseStep) : 1;
  const shown = mergeBuckets(base, factor, baseStep);
  const layout: ChartLayout = { width: Math.max(width, 1), height, ...PAD, left };
  const model = chartModel(shown, layout, form);
  const times = model.times;
  const last = times.length - 1;
  const bucket = (minStep(times) ?? step ?? 0) || 0;
  const span = (times.at(-1) ?? 0) - (times[0] ?? 0) + bucket;
  const yTicks = model.y
    .ticks(height < 200 ? 3 : 4)
    .map((v) => ({ value: v, position: model.y(v) }));
  const xTicks = placeTimeTicks(model, plotWidth, span, width);
  const baseline = model.y(0);
  const at = active === null ? null : (times[active] ?? null);
  const banded = form === "bar" || form === "range";
  const x =
    at === null ? null : banded ? model.x(new Date(at)) + model.step / 2 : model.x(new Date(at));

  const point = (event: PointerEvent<SVGSVGElement>) => {
    const rect = event.currentTarget.getBoundingClientRect();
    const position = event.clientX - rect.left - (banded ? model.step / 2 : 0);
    setActive(nearestBucket(model, position));
  };
  return (
    <div ref={box} className="flex w-full min-w-0 flex-col gap-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        {shown.length > 1 ? <Legend series={shown} family={family} form={form} /> : <span />}
        <Toggle
          pressed={table}
          onPressedChange={setTable}
          aria-label={`View as table: ${title.toLowerCase()}`}
          className="text-label"
        >
          View as table
        </Toggle>
      </div>
      {table ? (
        <ChartTable title={title} series={shown} format={format} />
      ) : (
        <div
          className="relative w-full rounded-sm has-[input:focus-visible]:ring-2 has-[input:focus-visible]:ring-midnight-ink/40"
          style={{ height }}
        >
          {/* The keyboard reads the plot through a native range: arrow keys, Home and End move the crosshair,
              and its value text says the bucket and its values. It is invisible and lets the pointer through. */}
          <input
            type="range"
            min={0}
            max={Math.max(0, last)}
            step={1}
            value={active ?? Math.max(0, last)}
            aria-label={`Step through ${title.toLowerCase()}`}
            aria-valuetext={at === null ? undefined : describe(at, shown, format)}
            onChange={(event) => setActive(Number(event.currentTarget.value))}
            onFocus={() => setActive((current) => current ?? (last >= 0 ? last : null))}
            onBlur={() => setActive(null)}
            onKeyDown={(event) => {
              if (event.key === "Escape") setActive(null);
            }}
            className="pointer-events-none absolute inset-0 h-full w-full opacity-0"
          />
          {
            <svg
              width={width}
              height={height}
              // Out of flow, so the plot never sets its own container's width: the container's width sets the plot's.
              className="absolute inset-0 block touch-none select-none"
              aria-hidden="true"
              onPointerMove={point}
              onPointerLeave={() => setActive(null)}
            >
              {yTicks.map((t) => (
                <g key={t.value}>
                  <line
                    x1={left}
                    x2={width - PAD.right}
                    y1={Math.round(t.position) + 0.5}
                    y2={Math.round(t.position) + 0.5}
                    stroke="var(--color-chart-grid)"
                  />
                  <text
                    x={left - 10}
                    y={t.position}
                    textAnchor="end"
                    dominantBaseline="middle"
                    className="fill-ink-soft text-label tabular-nums"
                  >
                    {tick(t.value)}
                  </text>
                </g>
              ))}
              <line
                x1={left}
                x2={width - PAD.right}
                y1={Math.round(baseline) + 0.5}
                y2={Math.round(baseline) + 0.5}
                stroke="var(--color-chart-axis)"
              />
              {xTicks.map((t) => (
                <text
                  key={t.value}
                  x={t.position}
                  y={height - 8}
                  textAnchor={t.anchor}
                  className="fill-ink-soft text-label tabular-nums"
                >
                  {t.label}
                </text>
              ))}
              {banded && x !== null ? (
                <rect
                  data-active-band
                  x={model.x(new Date(at ?? 0))}
                  width={model.step}
                  y={PAD.top}
                  height={Math.max(0, baseline - PAD.top)}
                  fill="color-mix(in srgb, var(--color-midnight-ink) 6%, transparent)"
                />
              ) : null}
              {form === "bar" ? (
                <Bars model={model} series={shown} family={family} />
              ) : form === "range" ? (
                <Ranges model={model} series={shown} />
              ) : (
                <Lines model={model} series={shown} family={family} />
              )}
              {markers
                .filter((m) => times.length > 0 && m.t >= (times[0] ?? 0))
                .filter((m) => model.x(new Date(m.t)) <= width - PAD.right + 1)
                .map((m) => (
                  <line
                    key={`${m.t}-${m.label}`}
                    data-marker-rule
                    x1={model.x(new Date(m.t))}
                    x2={model.x(new Date(m.t))}
                    y1={PAD.top}
                    y2={baseline}
                    stroke="var(--color-chart-axis)"
                    strokeDasharray="3 3"
                  />
                ))}
              {x !== null && !banded ? (
                <>
                  <line
                    data-crosshair
                    x1={x}
                    x2={x}
                    y1={PAD.top}
                    y2={baseline}
                    stroke="var(--color-chart-axis)"
                  />
                  {shown.map((s, slot) => {
                    const v = valuesAt(shown, at ?? 0)[slot];
                    if (v === null || v === undefined) return null;
                    const style = seriesStyle(slot, family);
                    return (
                      <path
                        key={s.id}
                        d={markerPath(slot, 72)}
                        transform={`translate(${x},${model.y(v)})`}
                        fill={style.color}
                        stroke="var(--color-warm-sand)"
                        strokeWidth={2}
                      />
                    );
                  })}
                </>
              ) : null}
            </svg>
          }
          {x !== null && at !== null ? (
            <div
              aria-hidden="true"
              className="pointer-events-none absolute top-2 z-10 flex min-w-32 flex-col gap-1 rounded-md border border-ash-border bg-parchment-white px-2.5 py-2 shadow-sm"
              style={x > width / 2 ? { right: width - x + 12 } : { left: x + 12 }}
            >
              <p className="text-label text-ink-soft">{rangeLabel(at, bucket)}</p>
              {shown.map((s, slot) => (
                <p key={s.id} className="flex items-center gap-2 whitespace-nowrap">
                  <LineKey style={seriesStyle(slot, family)} form={form} slot={slot} />
                  <span className="text-data font-medium tabular-nums">
                    {valueText(valuesAt(shown, at)[slot] ?? null, format)}
                  </span>
                  {shown.length > 1 ? (
                    <span className="text-label text-ink-soft">{s.label}</span>
                  ) : null}
                </p>
              ))}
            </div>
          ) : null}
        </div>
      )}
      {markers.length > 0 ? (
        <ul data-markers className="flex flex-wrap gap-x-4 gap-y-1 text-label text-ink-soft">
          {markers.map((m) => (
            <li key={`${m.t}-${m.label}`}>{`${m.label}, ${bucketFormat.format(m.t)}`}</li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

function Bars({
  model,
  series,
  family,
}: {
  model: ReturnType<typeof chartModel>;
  series: ChartSeries[];
  family: boolean;
}) {
  const n = series.length;
  const room = Math.max(1, model.step - BAR_GAP);
  const barWidth = Math.max(1, Math.min(MAX_BAR, (room - (n - 1) * BAR_GAP) / n));
  const groupWidth = barWidth * n + (n - 1) * BAR_GAP;
  const baseline = model.y(0);
  return (
    <>
      {series.map((s, slot) => {
        const style = seriesStyle(slot, family);
        return (
          <g key={s.id} data-series={s.id}>
            {s.points.map((p) => {
              if (p.v === null || p.v <= 0) return null;
              const start =
                model.x(new Date(p.t)) +
                (model.step - groupWidth) / 2 +
                slot * (barWidth + BAR_GAP);
              return (
                <path
                  key={p.t}
                  d={barPath(start, model.y(p.v), barWidth, baseline, Math.min(4, barWidth / 2))}
                  fill={style.color}
                  opacity={slot === 0 || family ? 1 : 0.85}
                />
              );
            })}
          </g>
        );
      })}
    </>
  );
}

// One mark per bucket: a faint bar from the lowest to the highest series (p50 to p99), and each series' marker on it.
// Busy buckets read as a band and sparse ones as single marks, where a line would zigzag between lone values.
function Ranges({
  model,
  series,
}: {
  model: ReturnType<typeof chartModel>;
  series: ChartSeries[];
}) {
  const width = Math.max(2, Math.min(6, (model.step - BAR_GAP) / 2));
  return (
    <>
      {model.times.map((t) => {
        const values = valuesAt(series, t);
        const present = values.filter((v): v is number => v !== null);
        if (present.length === 0) return null;
        const cx = model.x(new Date(t)) + model.step / 2;
        return (
          <g key={t} data-range={t}>
            <line
              x1={cx}
              x2={cx}
              y1={model.y(Math.min(...present))}
              y2={model.y(Math.max(...present))}
              stroke="var(--color-series-1)"
              strokeOpacity={0.3}
              strokeWidth={width}
              strokeLinecap="round"
            />
            {values.map((v, slot) =>
              v === null ? null : (
                <path
                  key={series[slot]?.id ?? slot}
                  d={markerPath(slot, 36)}
                  transform={`translate(${cx},${model.y(v)})`}
                  fill="var(--color-series-1)"
                  stroke="var(--color-warm-sand)"
                  strokeWidth={1.5}
                />
              ),
            )}
          </g>
        );
      })}
    </>
  );
}

function Lines({
  model,
  series,
  family,
}: {
  model: ReturnType<typeof chartModel>;
  series: ChartSeries[];
  family: boolean;
}) {
  return (
    <>
      {model.areas.map((d, slot) => {
        const s = series[slot];
        return s ? (
          <path
            key={`${s.id}-area`}
            data-area={s.id}
            d={d}
            fill={seriesStyle(slot, family).color}
            fillOpacity={0.1}
          />
        ) : null;
      })}
      {series.map((s, slot) => {
        const style = seriesStyle(slot, family);
        const points = s.points;
        return (
          <g key={s.id} data-series={s.id}>
            <path
              d={model.paths[slot]}
              fill="none"
              stroke={style.color}
              strokeWidth={2}
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeDasharray={style.dash}
            />
            {/* A value with no neighbour has no line to sit on, so it shows as a dot. */}
            {points.map((p, i) => {
              if (p.v === null) return null;
              const before = points[i - 1]?.v ?? null;
              const after = points[i + 1]?.v ?? null;
              if (before !== null || after !== null) return null;
              return (
                <circle
                  key={p.t}
                  cx={model.x(new Date(p.t))}
                  cy={model.y(p.v)}
                  r={2.5}
                  fill={style.color}
                />
              );
            })}
          </g>
        );
      })}
    </>
  );
}

interface PlacedTick {
  value: number;
  position: number;
  label: string;
  anchor: "start" | "middle" | "end";
}

// About one label per 110 px, kept inside the plot and never overlapping its neighbour.
function placeTimeTicks(
  model: ReturnType<typeof chartModel>,
  plotWidth: number,
  span: number,
  width: number,
): PlacedTick[] {
  const label = timeTickFormat(span);
  const count = Math.max(2, Math.floor(plotWidth / 110));
  const placed: PlacedTick[] = [];
  let lastEnd = Number.NEGATIVE_INFINITY;
  for (const d of model.x.ticks(count)) {
    const text = label(d.getTime());
    const w = textWidth(text);
    const position = model.x(d);
    let anchor: PlacedTick["anchor"] = "middle";
    let startX = position - w / 2;
    if (startX < 0) {
      anchor = "start";
      startX = position;
    } else if (position + w / 2 > width) {
      anchor = "end";
      startX = position - w;
    }
    if (startX < lastEnd + 12) continue;
    placed.push({ value: d.getTime(), position, label: text, anchor });
    lastEnd = startX + w;
  }
  return placed;
}

function rangeLabel(t: number, bucket: number): string {
  if (bucket <= 0) return bucketFormat.format(t);
  return `${bucketFormat.format(t)} – ${clockFormat.format(t + bucket)}`;
}

function valueText(value: number | null, format: (value: number) => string): string {
  return value === null ? "no data" : format(value);
}

function describe(t: number, series: ChartSeries[], format: (value: number) => string): string {
  const values = valuesAt(series, t)
    .map((v, i) => `${series[i]?.label ?? ""} ${valueText(v, format)}`)
    .join(", ");
  return `${bucketFormat.format(t)}: ${values}`;
}

function LineKey({
  style,
  form,
  slot = 0,
}: {
  style: SeriesStyle;
  form: ChartForm;
  slot?: number;
}) {
  if (form === "range") {
    return (
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true" className="shrink-0">
        <path d={markerPath(slot, 36)} transform="translate(6,6)" fill="var(--color-series-1)" />
      </svg>
    );
  }
  return form === "bar" ? (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" className="shrink-0">
      <rect width="10" height="10" rx="2" fill={style.color} />
    </svg>
  ) : (
    <svg width="18" height="10" viewBox="0 0 18 10" aria-hidden="true" className="shrink-0">
      <line
        x1="1"
        x2="17"
        y1="5"
        y2="5"
        stroke={style.color}
        strokeWidth={2}
        strokeLinecap="round"
        strokeDasharray={style.dash}
      />
    </svg>
  );
}

function Legend({
  series,
  family,
  form,
}: {
  series: ChartSeries[];
  family: boolean;
  form: ChartForm;
}) {
  return (
    <ul className="flex flex-wrap gap-x-4 gap-y-1 text-label">
      {series.map((s, slot) => {
        const style = seriesStyle(slot, family);
        return (
          <li key={s.id} className="flex items-center gap-1.5" data-marker={style.marker}>
            <LineKey style={style} form={form} slot={slot} />
            {s.label}
          </li>
        );
      })}
    </ul>
  );
}

function ChartTable({
  title,
  series,
  format,
}: {
  title: string;
  series: ChartSeries[];
  format: (value: number) => string;
}) {
  return (
    <div className="max-h-64 overflow-auto">
      <table className="w-full text-data">
        <caption className="sr-only">{title}</caption>
        <thead>
          <tr className="text-left text-label text-ink-soft">
            <th scope="col" className="py-1 pe-4 font-medium">
              Time
            </th>
            {series.map((s) => (
              <th key={s.id} scope="col" className="py-1 pe-4 font-medium">
                {s.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {tableRows(series).map((row) => (
            <tr key={row.t} className="border-t border-ash-border">
              <th scope="row" className="py-1 pe-4 text-left font-normal">
                {bucketFormat.format(row.t)}
              </th>
              {row.values.map((value, i) => (
                <td key={series[i]?.id ?? i} className="py-1 pe-4 tabular-nums">
                  {valueText(value, format)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function useWidth(ref: RefObject<HTMLDivElement | null>): number {
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const update = () => setWidth(Math.round(el.getBoundingClientRect().width));
    update();
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

let measureContext: CanvasRenderingContext2D | null | undefined;
let measureFont = "";

/** The rendered width of an axis label at the label size, so the plot leaves exactly enough room. */
function textWidth(text: string): number {
  if (typeof document === "undefined") return text.length * 7;
  if (measureContext === undefined) {
    measureContext = document.createElement("canvas").getContext("2d");
    measureFont = `12px ${getComputedStyle(document.body).fontFamily}`;
  }
  if (!measureContext) return text.length * 7;
  measureContext.font = measureFont;
  return measureContext.measureText(text).width;
}
