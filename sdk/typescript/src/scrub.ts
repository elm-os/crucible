// Scrubbing of captured MCP content, error messages and handle ids (contract 9.10), with the shared rules of
// `conformance/scrub-rules.json`. `scrub-rules.json` here holds the same rules, so the package carries them; a test
// keeps the two equal.

import rules from "./scrub-rules.json" with { type: "json" };

interface Rule {
  readonly name: string;
  readonly regex: RegExp;
  readonly replacement: string;
  readonly luhn: boolean;
}

const compiled: readonly Rule[] = rules.rules.map((rule) => ({
  name: rule.name,
  regex: new RegExp(rule.pattern, "g"),
  replacement: rule.replacement,
  luhn: "validator" in rule && rule.validator === "luhn",
}));

/** Applies every rule, in file order, to `text`. */
export function scrub(text: string): string {
  let out = text;
  for (const rule of compiled) {
    out = apply(rule, out);
  }
  return out;
}

/** Applies the one rule named `name`, for the rule vectors. */
export function scrubWithRule(name: string, text: string): string {
  const rule = compiled.find((candidate) => candidate.name === name);
  if (rule === undefined) {
    throw new Error(`no scrub rule named ${name}`);
  }
  return apply(rule, text);
}

/**
 * Replaces every non-overlapping match, leftmost first. After a match its validator rejects, the search resumes
 * after the match's first character that is neither an ASCII letter nor a digit, or after the match when it has
 * none, so a rejected run of digit groups cannot hide a card that starts at a later group.
 */
function apply(rule: Rule, text: string): string {
  const { regex } = rule;
  let out = "";
  let copied = 0;
  let from = 0;
  while (from <= text.length) {
    regex.lastIndex = from;
    const match = regex.exec(text);
    if (match === null) {
      break;
    }
    const start = match.index;
    const end = start + match[0].length;
    if (end === start) {
      from = start + 1;
      continue;
    }
    if (rule.luhn && !luhn(match[0])) {
      const offset = match[0].search(/[^A-Za-z0-9]/);
      from = offset === -1 ? end : start + offset + 1;
      continue;
    }
    out += text.slice(copied, start) + rule.replacement;
    copied = end;
    from = end;
  }
  return out + text.slice(copied);
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

const encoder = new TextEncoder();

/** The longest prefix of `text` that fits in `maxBytes` of UTF-8 and ends on a character boundary. */
export function cutUtf8(text: string, maxBytes: number): string {
  if (text.length * 3 <= maxBytes) {
    return text;
  }
  const { read } = encoder.encodeInto(text, new Uint8Array(maxBytes));
  return read >= text.length ? text : text.slice(0, read);
}
