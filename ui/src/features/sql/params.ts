import type { SavedQueryParameterType } from "@/api/generated/SavedQueryParameterType";

export interface DetectedParameter {
  name: string;
  type: string;
}

const PARAMETER = /\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*:\s*([^{}]+?)\s*\}/g;
const WINDOW = new Set(["from", "to"]);

// The `{name:Type}` parameters of a query text other than the window, each once, in order of first use.
export function detectParameters(sql: string): DetectedParameter[] {
  const found = new Map<string, DetectedParameter>();
  for (const match of sql.matchAll(PARAMETER)) {
    const name = match[1] ?? "";
    if (!WINDOW.has(name) && !found.has(name)) found.set(name, { name, type: match[2] ?? "" });
  }
  return [...found.values()];
}

export const PARAMETER_TYPES = [
  "string",
  "int",
  "float",
  "bool",
  "timestamp",
  "uuid",
] as const satisfies readonly SavedQueryParameterType[];

export function parseParameterType(value: unknown): SavedQueryParameterType | null {
  return PARAMETER_TYPES.find((type) => type === value) ?? null;
}

// The declared type that fits a ClickHouse type, so the save form starts with a sensible choice.
export function guessParameterType(clickhouseType: string): SavedQueryParameterType {
  const type = unwrap(clickhouseType);
  if (/^U?Int\d+$/.test(type)) return "int";
  if (/^(Float\d+|Decimal)/.test(type)) return "float";
  if (type === "Bool") return "bool";
  if (type.startsWith("Date")) return "timestamp";
  if (type === "UUID") return "uuid";
  return "string";
}

export function unwrap(clickhouseType: string): string {
  let type = clickhouseType.trim();
  for (;;) {
    const inner = /^(?:Nullable|LowCardinality)\((.*)\)$/.exec(type);
    if (!inner) return type;
    type = (inner[1] ?? "").trim();
  }
}

// Only the values of parameters the text uses are sent.
export function parameterValues(
  parameters: readonly { name: string }[],
  values: Readonly<Record<string, string>>,
): Record<string, string> {
  return Object.fromEntries(parameters.map(({ name }) => [name, values[name] ?? ""]));
}
