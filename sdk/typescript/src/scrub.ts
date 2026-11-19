// Scrubbing of captured MCP content, error messages and handle ids (contract 9.10), with the shared rules of
// `conformance/scrub-rules.json`. `scrub-rules.json` here holds the same rules, so the package carries them; a test
// keeps the two equal.

import { type Lead, type PatternFacts, patternFacts } from "./pattern.ts";
import rules from "./scrub-rules.json" with { type: "json" };

interface Rule {
  readonly name: string;
  readonly regex: RegExp;
  readonly replacement: string;
  readonly luhn: boolean;
  readonly facts: PatternFacts;
  /** For a lead pattern without a validator: the pattern from its anchor on, tried at one position. */
  readonly rest: RegExp | undefined;
}

const compiled: readonly Rule[] = rules.rules.map((rule) => {
  const facts = patternFacts(rule.pattern);
  const validated = "validator" in rule && rule.validator === "luhn";
  return {
    name: rule.name,
    regex: new RegExp(rule.pattern, "g"),
    replacement: rule.replacement,
    luhn: validated,
    facts,
    rest: facts.lead === undefined || validated ? undefined : new RegExp(facts.lead.rest, "y"),
  };
});

/** No rule matches text shorter than this. */
const shortest = Math.min(...compiled.map((rule) => rule.facts.minLength));

/** Called once per matcher call with the rule's name and the call's steps (see `searchSteps`). */
export type MatcherHook = (rule: string, steps: number) => void;

let matcherHook: MatcherHook | undefined;

/** Installs, or with `undefined` removes, the hook the scrubbing work checks read matcher steps from. */
export function setMatcherHook(hook: MatcherHook | undefined): void {
  matcherHook = hook;
}

/**
 * The steps of an unanchored search that tried the starts from `from` up to `to`, as a backtracking matcher takes
 * them at the least: one per start, and from a start where a lead pattern's prefix matches, the prefix and the
 * whole class run after it, which the greedy repetition consumes before it backtracks.
 */
function searchSteps(lead: Lead | undefined, text: string, from: number, to: number): number {
  if (lead === undefined) {
    return to - from;
  }
  const p = lead.prefix.length;
  let end = Math.min(text.length, to + p);
  while (end < text.length && lead.inClass(text.charAt(end))) {
    end += 1;
  }
  const runs = new Int32Array(end - from + 1);
  for (let k = end - 1; k >= from; k -= 1) {
    runs[k - from] = lead.inClass(text.charAt(k)) ? (runs[k - from + 1] ?? 0) + 1 : 0;
  }
  let steps = 0;
  for (let s = from; s < to; s += 1) {
    steps += 1;
    if (text.startsWith(lead.prefix, s)) {
      steps += p + (runs[s + p - from] ?? 0);
    }
  }
  return steps;
}

/** Applies every rule, in file order, to `text`. */
export function scrub(text: string): string {
  if (text.length < shortest) {
    return text;
  }
  let out = text;
  for (const rule of compiled) {
    out = apply(rule, out);
  }
  return out;
}

/** Applies the one rule named `name`, for the rule vectors. */
export function scrubWithRule(name: string, text: string): string {
  return apply(ruleNamed(name), text);
}

/**
 * The rules applied without their guards (one rule when `name` is given): the reference the differential tests
 * hold the guarded scrubber to.
 */
export function scrubPlain(text: string, name?: string): string {
  let out = text;
  for (const rule of name === undefined ? compiled : [ruleNamed(name)]) {
    out = applyPlain(rule, out);
  }
  return out;
}

function ruleNamed(name: string): Rule {
  const rule = compiled.find((candidate) => candidate.name === name);
  if (rule === undefined) {
    throw new Error(`no scrub rule named ${name}`);
  }
  return rule;
}

/**
 * Applies one rule behind guards that take time linear in the text and change nothing: text shorter than any
 * match, or without the rule's required literal, is left alone, and a lead pattern is tried once per anchor.
 */
function apply(rule: Rule, text: string): string {
  const { minLength, literals, lead } = rule.facts;
  if (
    text.length < minLength ||
    (literals !== undefined && !literals.some((l) => text.includes(l)))
  ) {
    return text;
  }
  return lead === undefined || rule.rest === undefined
    ? applyPlain(rule, text)
    : applyLead(rule, lead, rule.rest, text);
}

/**
 * The matches of a lead pattern, found from its anchors. A match ends its class run at an anchor, and every start in
 * that run meets the same anchor, so only the run's leftmost possible start is a candidate and the rest of the
 * pattern is tried once, anchored there. Each position is scanned a bounded number of times.
 */
function applyLead(rule: Rule, lead: Lead, rest: RegExp, text: string): string {
  let out = "";
  let copied = 0;
  let from = 0;
  let at = text.indexOf(lead.anchor);
  while (at !== -1) {
    let start = at;
    while (start > from && lead.inClass(text.charAt(start - 1))) {
      start -= 1;
    }
    let begin = -1;
    if (lead.prefix === "") {
      begin = start < at ? start : -1;
    } else {
      const k = text.slice(start, at).indexOf(lead.prefix);
      begin = k !== -1 && start + k + lead.prefix.length < at ? start + k : -1;
    }
    if (begin !== -1) {
      rest.lastIndex = at;
      const match = rest.exec(text);
      if (matcherHook !== undefined) {
        matcherHook(rule.name, attemptSteps(lead, text, at, match));
      }
      if (match !== null) {
        out += text.slice(copied, begin) + rule.replacement;
        copied = at + match[0].length;
        from = copied;
        at = text.indexOf(lead.anchor, from);
        continue;
      }
    }
    at = text.indexOf(lead.anchor, at + 1);
  }
  return out + text.slice(copied);
}

/** The characters an anchored attempt of a lead pattern's rest can read: its match, or the anchor, the class run
 * after it and the character that ends the run. */
function attemptSteps(lead: Lead, text: string, at: number, match: RegExpExecArray | null): number {
  if (match !== null) {
    return match[0].length;
  }
  let end = at + 1;
  while (end < text.length && lead.inClass(text.charAt(end))) {
    end += 1;
  }
  return end - at + 1;
}

/**
 * Replaces every non-overlapping match, leftmost first. After a match its validator rejects, the search resumes
 * after the match's first character that is neither an ASCII letter nor a digit, or after the match when it has
 * none, so a rejected run of digit groups cannot hide a card that starts at a later group.
 */
function applyPlain(rule: Rule, text: string): string {
  const { regex } = rule;
  let out = "";
  let copied = 0;
  let from = 0;
  while (from <= text.length) {
    regex.lastIndex = from;
    const match = regex.exec(text);
    if (matcherHook !== undefined) {
      const to = match === null ? text.length : match.index + 1;
      matcherHook(rule.name, searchSteps(rule.facts.lead, text, from, to));
    }
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
