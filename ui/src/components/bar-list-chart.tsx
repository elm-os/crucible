"use client";

import { Toggle } from "@ui-sdk/react/toggle";
import { type CSSProperties, useState } from "react";

export interface BarItem {
  key: string;
  label: string;
  value: number | null;
  /** The remainder beyond the ranked keys, drawn lighter. */
  other?: boolean;
}

// A breakdown: one horizontal bar per key from one baseline, one series in one color, the value at the tip,
// and a table view of the same numbers. The bars are drawn in the order given, the largest first.
export function BarListChart({
  title,
  items,
  format,
}: {
  title: string;
  items: BarItem[];
  format: (value: number) => string;
}) {
  const [table, setTable] = useState(false);
  const max = Math.max(0, ...items.map((item) => item.value ?? 0));
  const text = (value: number | null) => (value === null ? "no data" : format(value));
  return (
    <div className="flex flex-col gap-2">
      <div className="flex justify-end">
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
        <div className="max-h-64 overflow-auto">
          <table className="w-full text-data">
            <caption className="sr-only">{title}</caption>
            <thead>
              <tr className="text-left text-label text-ink-soft">
                <th scope="col" className="py-1 pe-4 font-medium">
                  Key
                </th>
                <th scope="col" className="py-1 text-end font-medium">
                  Value
                </th>
              </tr>
            </thead>
            <tbody>
              {items.map((item) => (
                <tr key={item.key} className="border-t border-ash-border">
                  <th scope="row" className="py-1 pe-4 text-left font-normal">
                    {item.label}
                  </th>
                  <td className="py-1 text-end tabular-nums">{text(item.value)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <ol className="flex flex-col gap-1.5" aria-label={title}>
          {items.map((item) => {
            const share = max > 0 && item.value !== null ? (item.value / max) * 100 : 0;
            return (
              <li
                key={item.key}
                data-other={item.other ? "" : undefined}
                className="grid grid-cols-[minmax(0,9rem)_minmax(0,1fr)] items-center gap-3 text-label"
              >
                <span className="truncate" title={item.label}>
                  {item.label}
                </span>
                {/* The track keeps room for the value, so every bar's length stays in proportion. */}
                <span className="relative block pe-20">
                  <span className="relative block h-4">
                    <span
                      aria-hidden="true"
                      data-other={item.other ? "" : undefined}
                      className="absolute inset-y-0.5 start-0 w-[var(--share)] rounded-e-[4px] bg-[var(--color-series-1)] data-other:opacity-[var(--opacity-ramp-3)]"
                      style={{ "--share": `${share}%` } as CSSProperties}
                    />
                    <span
                      className="absolute top-0 start-[var(--share)] ps-2 whitespace-nowrap tabular-nums"
                      style={{ "--share": `${share}%` } as CSSProperties}
                    >
                      {text(item.value)}
                    </span>
                  </span>
                </span>
              </li>
            );
          })}
        </ol>
      )}
    </div>
  );
}
