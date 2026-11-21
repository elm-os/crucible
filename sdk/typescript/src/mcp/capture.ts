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

/** Whether a code unit is a character of tokens, keys and encoded blobs: `[A-Za-z0-9._~+/=-]`. */
function token(code: number): boolean {
  return (
    (code >= 0x30 && code <= 0x39) ||
    (code >= 0x41 && code <= 0x5a) ||
    (code >= 0x61 && code <= 0x7a) ||
    code === 0x2e ||
    code === 0x5f ||
    code === 0x7e ||
    code === 0x2b ||
    code === 0x2f ||
    code === 0x3d ||
    code === 0x2d
  );
}

/**
 * The work one captured field cost, in UTF-16 code units: the JSON text serialized for it before any cut, and the
 * text handed to the scrubber.
 */
export type CaptureWorkHook = (serialized: number, scrubbed: number) => void;

let workHook: CaptureWorkHook | undefined;

/** Installs, or with `undefined` removes, the hook the capture-overhead scenario reads its work bound from. */
export function setCaptureWorkHook(hook: CaptureWorkHook | undefined): void {
  workHook = hook;
}

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
  const json = jsonPrefix(value, maxBytes + SCRUB_MARGIN);
  if (json === undefined) {
    return undefined;
  }
  const window = json.text;
  workHook?.(window.length, window.length);
  const scrubbed = scrub(window);
  let text = cutUtf8(scrubbed, maxBytes);
  const cut = json.more || text.length < scrubbed.length;
  if (cut) {
    // A credential longer than the margin can straddle the cap and no longer match its rule; drop the partial
    // token run at the end instead of keeping its prefix.
    let end = text.length;
    while (end > 0 && token(text.charCodeAt(end - 1))) {
      end -= 1;
    }
    text = text.slice(0, end);
  }
  return { text, cut };
}

/** Thrown inside the writer once the budget is spent. */
const SPENT = new Error("the capture budget is spent");

const isRawJson = (JSON as { isRawJSON?: (value: unknown) => boolean }).isRawJSON;

/** A code unit `JSON.stringify` escapes in a string: anything but these, so controls, quotes, backslashes and
 * surrogates. */
const ESCAPED = /[^\x20\x21\x23-\x5b\x5d-퟿-￿]/;

/** The prototype of plain objects. */
const PLAIN: unknown = Object.getPrototypeOf({});

/** The `valueOf` of each primitive wrapper; each throws for anything but its own kind of wrapper. */
const UNWRAP = [0, "", false, 0n].map(
  (primitive) =>
    (Object.getPrototypeOf(primitive) as { valueOf: (this: unknown) => unknown }).valueOf,
);

/**
 * The first `limit` UTF-16 code units of `JSON.stringify(value)`, and whether the whole text is longer, serialized
 * no further than that. Undefined when `JSON.stringify` returns undefined for `value`, or throws (a cycle, a
 * BigInt, a throwing `toJSON`) before the limit.
 */
export function jsonPrefix(
  value: unknown,
  limit: number,
): { text: string; more: boolean } | undefined {
  const writer = new BoundedJson(limit);
  try {
    return writer.root(value) ? { text: writer.text(), more: false } : undefined;
  } catch (error) {
    return error === SPENT ? { text: writer.text(), more: true } : undefined;
  }
}

/** The steps of `JSON.stringify` (ECMA-262 SerializeJSONProperty and its callees), writing into a budget. */
class BoundedJson {
  readonly #limit: number;
  /** The containers being written, from the root down: a value met again among them is a cycle. */
  readonly #path = new Set<object>();
  #out = "";

  constructor(limit: number) {
    this.#limit = limit;
  }

  text(): string {
    return this.#out;
  }

  /** Writes `value` and returns true, or returns false when it serializes to nothing. */
  root(value: unknown): boolean {
    return this.#value(this.#prepare(value, ""));
  }

  #write(text: string): void {
    const room = this.#limit - this.#out.length;
    if (text.length > room) {
      this.#out += text.slice(0, room);
      throw SPENT;
    }
    this.#out += text;
  }

  /** `toJSON`, then unboxing, as `JSON.stringify` applies them to each property value. */
  #prepare(value: unknown, key: string): unknown {
    if ((typeof value !== "object" || value === null) && typeof value !== "bigint") {
      return value;
    }
    let prepared: unknown = value;
    const toJson: unknown = (value as { toJSON?: unknown }).toJSON;
    if (typeof toJson === "function") {
      prepared = toJson.call(value, key);
    }
    if (
      typeof prepared !== "object" ||
      prepared === null ||
      Array.isArray(prepared) ||
      Object.getPrototypeOf(prepared) === PLAIN
    ) {
      return prepared;
    }
    // A primitive wrapper, such as `new Number(1)`, serializes as its primitive.
    for (const unwrap of UNWRAP) {
      try {
        return unwrap.call(prepared);
      } catch {
        // Not this kind of wrapper.
      }
    }
    return prepared;
  }

  #value(value: unknown): boolean {
    switch (typeof value) {
      case "boolean":
        this.#write(value ? "true" : "false");
        return true;
      case "number":
        this.#write(Number.isFinite(value) ? String(value) : "null");
        return true;
      case "string":
        this.#string(value);
        return true;
      case "bigint":
        throw new TypeError("Do not know how to serialize a BigInt");
      case "object":
        if (value === null) {
          this.#write("null");
        } else if (isRawJson?.(value) === true) {
          this.#write(JSON.stringify(value));
        } else {
          this.#container(value);
        }
        return true;
      default:
        return false;
    }
  }

  #container(value: object): void {
    if (this.#path.has(value)) {
      throw new TypeError("Converting circular structure to JSON");
    }
    this.#path.add(value);
    const holder = value as Record<string, unknown>;
    if (Array.isArray(value)) {
      this.#write("[");
      let i = 0;
      while (i < value.length) {
        // A run of flat records is written by `JSON.stringify` in one call: the same text, far fewer strings.
        const run = flatRun(value, i);
        if (run.length > 0) {
          const text = JSON.stringify(run);
          this.#write(i > 0 ? `,${text.slice(1, -1)}` : text.slice(1, -1));
          i += run.length;
          continue;
        }
        if (i > 0) {
          this.#write(",");
        }
        if (!this.#value(this.#prepare(holder[i], String(i)))) {
          this.#write("null");
        }
        i += 1;
      }
      this.#write("]");
    } else {
      this.#write("{");
      let first = true;
      for (const key of Object.keys(value)) {
        const item = this.#prepare(holder[key], key);
        if (item === undefined || typeof item === "function" || typeof item === "symbol") {
          continue;
        }
        if (!first) {
          this.#write(",");
        }
        first = false;
        this.#string(key);
        this.#write(":");
        this.#value(item);
      }
      this.#write("}");
    }
    this.#path.delete(value);
  }

  /**
   * A JSON string. One that fits the budget and needs no escape is written as it is; any other is escaped a
   * budget-sized chunk at a time, and a chunk never ends inside a surrogate pair.
   */
  #string(text: string): void {
    if (text.length + 2 <= this.#limit - this.#out.length && !ESCAPED.test(text)) {
      this.#out += `"${text}"`;
      return;
    }
    this.#write('"');
    let i = 0;
    while (i < text.length) {
      const room = this.#limit - this.#out.length;
      if (room <= 0) {
        throw SPENT;
      }
      let end = Math.min(text.length, i + room);
      const last = text.charCodeAt(end - 1);
      if (end < text.length && last >= 0xd800 && last <= 0xdbff) {
        end += 1;
      }
      this.#write(JSON.stringify(text.slice(i, end)).slice(1, -1));
      i = end;
    }
    this.#write('"');
  }
}

/** A run of flat records is at most this many items, each of at most this many members. */
const RUN_ITEMS = 64;
const RUN_MEMBERS = 16;
/** The longest name and string a member of a flat record has, so that a run writes a bounded amount of text. */
const RUN_STRING = 128;

/**
 * The run of flat records that starts at `start`: plain objects whose members all hold a short string, a number, a
 * boolean or null, each copied with the values read once, as `JSON.stringify` reads them. `JSON.stringify` writes
 * such a run exactly as the bounded writer would. A record that turns out not to be flat is written member by
 * member, so the members read before the one that ended the run are read again.
 */
function flatRun(items: readonly unknown[], start: number): Record<string, unknown>[] {
  const run: Record<string, unknown>[] = [];
  for (let at = start; at < items.length && run.length < RUN_ITEMS; at += 1) {
    const item = items[at];
    if (typeof item !== "object" || item === null || Object.getPrototypeOf(item) !== PLAIN) {
      break;
    }
    const keys = Object.keys(item);
    if (keys.length > RUN_MEMBERS) {
      break;
    }
    // No prototype, so that a member named like the prototype accessor stays a member, as in the record.
    const copy: Record<string, unknown> = Object.create(null);
    for (const key of keys) {
      if (key.length > RUN_STRING) {
        return run;
      }
      const member = (item as Record<string, unknown>)[key];
      if (!flatMember(member)) {
        return run;
      }
      copy[key] = member;
    }
    run.push(copy);
  }
  return run;
}

function flatMember(value: unknown): boolean {
  switch (typeof value) {
    case "string":
      return value.length <= RUN_STRING;
    case "number":
    case "boolean":
      return true;
    default:
      return value === null;
  }
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
