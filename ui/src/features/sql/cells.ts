import { unwrap } from "./params";

export type CellKind = "null" | "number" | "text" | "json";

export interface Cell {
  kind: CellKind;
  text: string;
}

export const PAGE_ROWS = 100;
export const MAX_ROWS = 10_000;

// 64-bit integers arrive as strings and stay strings, so no digit is lost.
export function formatCell(value: unknown): Cell {
  if (value === null || value === undefined) return { kind: "null", text: "null" };
  if (typeof value === "string") return { kind: "text", text: value };
  if (typeof value === "number") return { kind: "number", text: String(value) };
  if (typeof value === "boolean") return { kind: "text", text: String(value) };
  return { kind: "json", text: JSON.stringify(value) };
}

export function isNumericType(clickhouseType: string): boolean {
  return /^(U?Int\d+|Float\d+|Decimal)/.test(unwrap(clickhouseType));
}

// A type without its enum value lists, so Enum8('unset' = 0, 'ok' = 1) reads Enum8. Values are quoted, may hold
// parentheses and escape their quotes.
export function shortType(clickhouseType: string): string {
  return clickhouseType.replace(/\b(Enum(?:8|16))\((?:'(?:[^'\\]|\\.)*'|[^()'])*\)/g, "$1");
}

// The value names of the first enum in a type, in order; none for a type without one.
export function enumValues(clickhouseType: string): string[] {
  const list = /\bEnum(?:8|16)\(((?:'(?:[^'\\]|\\.)*'|[^()'])*)\)/.exec(clickhouseType)?.[1] ?? "";
  return Array.from(list.matchAll(/'((?:[^'\\]|\\.)*)'/g), (match) =>
    (match[1] ?? "").replace(/\\(.)/g, "$1"),
  );
}

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB"] as const;

export function formatBytes(bytes: number): string {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${unit === 0 || value >= 100 ? Math.round(value) : value.toFixed(1)} ${UNITS[unit]}`;
}

export function firstLine(sql: string): string {
  return (
    sql
      .split("\n")
      .map((line) => line.trim())
      .find((line) => line !== "") ?? ""
  );
}

// The offset in `text` of a one-based line and column, clamped to the text.
export function offsetAt(text: string, line: number, column: number): number {
  const lines = text.split("\n");
  const index = Math.min(Math.max(line, 1), lines.length) - 1;
  let offset = 0;
  for (let before = 0; before < index; before += 1) offset += (lines[before] ?? "").length + 1;
  return offset + Math.min(Math.max(column, 1) - 1, (lines[index] ?? "").length);
}
