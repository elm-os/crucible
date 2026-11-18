// Scrubbing of captured MCP content, error messages and handle ids (contract 9.10). The rules are data in
// `scrub-rules.json`, with test vectors.

import rules from "./scrub-rules.json" with { type: "json" };

interface Pattern {
  readonly rule: string;
  readonly literal: string | undefined;
  readonly luhn: boolean;
  readonly regex: RegExp;
}

/** A JSON member `"key": value` whose value is a string (possibly cut short) or a scalar. */
const MEMBER =
  /"((?:[^"\\]|\\.){1,128})"(\s{0,16}:\s{0,16})(?:"(?:[^"\\]|\\.)*"?|-?\d[\d.eE+-]{0,64}|true|false|null)/g;

const sensitiveNames: readonly string[] = rules.keys.names;
const patterns: readonly Pattern[] = rules.patterns.map((pattern) => ({
  rule: pattern.rule,
  literal: "literal" in pattern ? pattern.literal : undefined,
  luhn: "check" in pattern && pattern.check === "luhn",
  regex: new RegExp(pattern.regex, "g"),
}));

function marker(rule: string): string {
  return rules.replacement.replace("{rule}", rule);
}

/** Replaces every match of the scrub rules in `text`. */
export function scrub(text: string): string {
  let out = text.replace(MEMBER, (match: string, key: string, separator: string) =>
    sensitive(key) ? `"${key}"${separator}"${marker(rules.keys.rule)}"` : match,
  );
  for (const pattern of patterns) {
    if (pattern.literal !== undefined && !out.includes(pattern.literal)) {
      continue;
    }
    out = out.replace(pattern.regex, (match: string) =>
      pattern.luhn && !luhn(match) ? match : marker(pattern.rule),
    );
  }
  return out;
}

function sensitive(key: string): boolean {
  const normalized = key.toLowerCase().replace(/[^a-z0-9]/g, "");
  return sensitiveNames.some((name) => normalized.endsWith(name));
}

function luhn(candidate: string): boolean {
  const digits = candidate.replace(/[^0-9]/g, "");
  let sum = 0;
  for (let i = 0; i < digits.length; i += 1) {
    let digit = Number(digits[digits.length - 1 - i]);
    if (i % 2 === 1) {
      digit *= 2;
      if (digit > 9) {
        digit -= 9;
      }
    }
    sum += digit;
  }
  return sum % 10 === 0;
}

/** The scrub rule vectors, for tests. */
export const scrubVectors: readonly { readonly input: string; readonly output: string }[] =
  rules.vectors;

const encoder = new TextEncoder();

/** The longest prefix of `text` that fits in `maxBytes` of UTF-8 and ends on a character boundary. */
export function cutUtf8(text: string, maxBytes: number): string {
  if (text.length * 3 <= maxBytes) {
    return text;
  }
  const { read } = encoder.encodeInto(text, new Uint8Array(maxBytes));
  return read >= text.length ? text : text.slice(0, read);
}
