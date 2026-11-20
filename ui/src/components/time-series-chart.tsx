"use client";

import { Field, FieldLabel } from "@ui-sdk/react/field";
import {
  Slider,
  SliderControl,
  SliderIndicator,
  SliderThumb,
  SliderTrack,
} from "@ui-sdk/react/slider";
import { Toggle } from "@ui-sdk/react/toggle";
import { type PointerEvent, useState } from "react";
import {
  type ChartForm,
  type ChartLayout,
  type ChartSeries,
  chartModel,
  MAX_SERIES,
  markerIndexes,
  markerPath,
  nearestBucket,
  seriesStyle,
  tableRows,
  valuesAt,
} from "@/primitives/chart/chart-model";

const LAYOUT: ChartLayout = { width: 640, height: 200, top: 12, right: 12, bottom: 24, left: 52 };

const bucketFormat = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
});
const tickFormat = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });

// Lines for time series and bars for counts per bucket, up to four series with a dash and a marker each,
// a crosshair that follows the pointer or the bucket slider's arrow keys, and a table view of the numbers.
export function TimeSeriesChart({
  title,
  series,
  form = "line",
  family = false,
  format,
  markers = [],
}: {
  title: string;
  series: ChartSeries[];
  form?: ChartForm;
  /** Percentiles of one measure: one ink family, told apart by dash. */
  family?: boolean;
  format: (value: number) => string;
  /** Instants to mark with a vertical rule, such as deploys, each with a label. */
  markers?: { t: number; label: string }[];
}) {
  const shown = series.slice(0, MAX_SERIES);
  const [active, setActive] = useState<number | null>(null);
  const [table, setTable] = useState(false);
  const model = chartModel(shown, LAYOUT, form);
  const at = active === null ? null : (model.times[active] ?? null);
  const last = Math.max(0, model.times.length - 1);
  const point = (event: PointerEvent<SVGSVGElement>) => {
    const box = event.currentTarget.getBoundingClientRect();
    const scale = box.width > 0 ? LAYOUT.width / box.width : 1;
    setActive(nearestBucket(model, (event.clientX - box.left) * scale));
  };

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        {shown.length > 1 ? <Legend series={shown} family={family} /> : <span />}
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
        <>
          <div className="relative">
            <svg
              viewBox={`0 0 ${LAYOUT.width} ${LAYOUT.height}`}
              className="h-auto w-full touch-none"
              aria-hidden="true"
              onPointerMove={point}
              onPointerLeave={() => setActive(null)}
            >
              {model.yTicks.map((tick) => (
                <g key={tick.value}>
                  <line
                    x1={LAYOUT.left}
                    x2={LAYOUT.width - LAYOUT.right}
                    y1={tick.position}
                    y2={tick.position}
                    stroke="var(--color-chart-grid)"
                  />
                  <text
                    x={LAYOUT.left - 6}
                    y={tick.position}
                    textAnchor="end"
                    dominantBaseline="middle"
                    className="fill-ink-soft text-label"
                  >
                    {format(tick.value)}
                  </text>
                </g>
              ))}
              {model.xTicks.map((tick) => (
                <text
                  key={tick.value}
                  x={tick.position}
                  y={LAYOUT.height - 6}
                  textAnchor="middle"
                  className="fill-ink-soft text-label"
                >
                  {tickFormat.format(tick.value)}
                </text>
              ))}
              {form === "bar"
                ? shown.map((s, slot) =>
                    s.points.map((p) =>
                      p.v === null ? null : (
                        <rect
                          key={`${s.id}-${p.t}`}
                          data-series={s.id}
                          x={model.x(new Date(p.t)) + (model.step * slot) / shown.length + 1}
                          width={Math.max(1, model.step / shown.length - 2)}
                          y={model.y(p.v)}
                          height={Math.max(0, model.y(0) - model.y(p.v))}
                          fill={seriesStyle(slot, family).color}
                          opacity={slot === 0 ? 1 : "var(--opacity-ramp-4)"}
                        />
                      ),
                    ),
                  )
                : shown.map((s, slot) => {
                    const style = seriesStyle(slot, family);
                    const sorted = s.points.toSorted((a, b) => a.t - b.t);
                    return (
                      <g key={s.id} data-series={s.id}>
                        <path
                          d={model.paths[slot]}
                          fill="none"
                          stroke={style.color}
                          strokeWidth={2}
                          strokeDasharray={style.dash}
                        />
                        {markerIndexes(sorted.length).map((i) => {
                          const p = sorted[i];
                          if (!p || p.v === null) return null;
                          return (
                            <path
                              key={p.t}
                              d={markerPath(slot)}
                              transform={`translate(${model.x(new Date(p.t))},${model.y(p.v)})`}
                              fill={style.color}
                            />
                          );
                        })}
                      </g>
                    );
                  })}
              {markers
                .filter((m) => model.times.length > 0 && m.t >= (model.times[0] ?? 0))
                .map((m) => (
                  <line
                    key={`${m.t}-${m.label}`}
                    data-marker-rule
                    x1={model.x(new Date(m.t))}
                    x2={model.x(new Date(m.t))}
                    y1={LAYOUT.top}
                    y2={LAYOUT.height - LAYOUT.bottom}
                    stroke="var(--color-chart-axis)"
                    strokeDasharray="2 3"
                  />
                ))}
              {at === null ? null : (
                <line
                  data-crosshair
                  x1={model.x(new Date(at))}
                  x2={model.x(new Date(at))}
                  y1={LAYOUT.top}
                  y2={LAYOUT.height - LAYOUT.bottom}
                  stroke="var(--color-chart-axis)"
                />
              )}
            </svg>
            {at === null ? null : (
              <div
                aria-hidden="true"
                className="pointer-events-none absolute top-1 right-1 rounded-md border border-ash-border bg-parchment-white px-2 py-1 text-label shadow-sm"
              >
                <p className="text-ink-soft">{bucketFormat.format(at)}</p>
                {shown.map((s, slot) => (
                  <p key={s.id} className="tabular-nums">
                    {shown.length > 1 ? `${s.label}: ` : ""}
                    {valueText(valuesAt(shown, at)[slot] ?? null, format)}
                  </p>
                ))}
              </div>
            )}
          </div>
          <Field>
            <FieldLabel className="text-label text-ink-soft">{`Step through ${title.toLowerCase()}`}</FieldLabel>
            <Slider
              min={0}
              max={last}
              step={1}
              value={active ?? last}
              onValueChange={(value) => setActive(Array.isArray(value) ? (value[0] ?? 0) : value)}
              format={(index) => describe(model.times[index] ?? 0, shown, format)}
            >
              <SliderControl>
                <SliderTrack>
                  <SliderIndicator />
                  <SliderThumb />
                </SliderTrack>
              </SliderControl>
            </Slider>
          </Field>
        </>
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

function valueText(value: number | null, format: (value: number) => string): string {
  return value === null ? "no data" : format(value);
}

function describe(t: number, series: ChartSeries[], format: (value: number) => string): string {
  const values = valuesAt(series, t)
    .map((v, i) => `${series[i]?.label ?? ""} ${valueText(v, format)}`)
    .join(", ");
  return `${bucketFormat.format(t)}: ${values}`;
}

function Legend({ series, family }: { series: ChartSeries[]; family: boolean }) {
  return (
    <ul className="flex flex-wrap gap-x-4 gap-y-1 text-label">
      {series.map((s, slot) => {
        const style = seriesStyle(slot, family);
        return (
          <li key={s.id} className="flex items-center gap-1.5" data-marker={style.marker}>
            <svg width="24" height="10" viewBox="0 0 24 10" aria-hidden="true">
              <line
                x1="0"
                x2="24"
                y1="5"
                y2="5"
                stroke={style.color}
                strokeWidth={2}
                strokeDasharray={style.dash}
              />
              <path d={markerPath(slot, 24)} transform="translate(12,5)" fill={style.color} />
            </svg>
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
