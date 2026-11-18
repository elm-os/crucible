// Content capture (contract 9.10): off by default; opted in per instrumented server by tool glob; sampled before
// any copy; cut at 16 KiB per field by default; scrubbed before it leaves the process.

import { cutUtf8, scrub } from "../scrub.ts";

/** Captures the arguments and results of the tools whose names match `tool`. The first matching rule wins. */
export interface CaptureRule {
  /** Tool name glob: `*` matches any run of characters, `?` one character. Default `*`. */
  tool?: string;
  /** Capture `params.arguments` as `gen_ai.tool.call.arguments`. Default true. */
  args?: boolean;
  /** Capture the result as `gen_ai.tool.call.result`. Default true. */
  result?: boolean;
  /** Share of matching calls captured, from 0 to 1. Default 1. */
  sampleRate?: number;
  /** Cap per field in UTF-8 bytes. Default 16,384. */
  maxBytes?: number;
}

export interface CaptureDecision {
  readonly args: boolean;
  readonly result: boolean;
  readonly sampleRate: number;
  readonly maxBytes: number;
}

/** Room past the cap that scrubbing sees, so a secret that straddles the cap is still matched whole. */
const SCRUB_MARGIN = 1_024;

/** Whether to capture one call, decided before any argument is read. */
export function decide(
  rules: readonly CaptureRule[] | undefined,
  tool: string,
): CaptureDecision | undefined {
  const rule = rules?.find((candidate) => glob(candidate.tool ?? "*", tool));
  if (rule === undefined) {
    return undefined;
  }
  const sampleRate = rule.sampleRate ?? 1;
  if (!(Math.random() < sampleRate)) {
    return undefined;
  }
  return {
    args: rule.args ?? true,
    result: rule.result ?? true,
    sampleRate,
    maxBytes: rule.maxBytes ?? 16_384,
  };
}

/** The scrubbed JSON text of `value`, cut at `maxBytes`, or undefined when it does not serialize. */
export function captured(
  value: unknown,
  maxBytes: number,
): { text: string; cut: boolean } | undefined {
  let json: string | undefined;
  try {
    json = JSON.stringify(value);
  } catch {
    return undefined;
  }
  if (json === undefined) {
    return undefined;
  }
  const window = json.slice(0, maxBytes + SCRUB_MARGIN);
  const scrubbed = scrub(window);
  const text = cutUtf8(scrubbed, maxBytes);
  return { text, cut: window.length < json.length || text.length < scrubbed.length };
}

/** Glob match with `*` and `?`, in time linear in the product of the lengths at worst. */
export function glob(pattern: string, name: string): boolean {
  let p = 0;
  let n = 0;
  let star = -1;
  let mark = 0;
  while (n < name.length) {
    if (p < pattern.length && (pattern[p] === "?" || pattern[p] === name[n])) {
      p += 1;
      n += 1;
    } else if (p < pattern.length && pattern[p] === "*") {
      star = p;
      p += 1;
      mark = n;
    } else if (star !== -1) {
      p = star + 1;
      mark += 1;
      n = mark;
    } else {
      return false;
    }
  }
  while (p < pattern.length && pattern[p] === "*") {
    p += 1;
  }
  return p === pattern.length;
}
