// What a scrub rule's pattern requires of every match, read once when the rules load. Patterns are in the shared
// subset of conformance/scrub-rules.json (literals, escaped punctuation, bracket classes, (?:...) groups,
// alternation and greedy quantifiers), which the conformance driver checks before any SDK loads the file.

/** A bracket class: membership of the ASCII characters, possibly negated. */
interface CharClass {
  readonly members: readonly boolean[];
  readonly negated: boolean;
}

type Atom =
  | { readonly kind: "char"; readonly char: string }
  | { readonly kind: "class"; readonly cls: CharClass }
  | { readonly kind: "group"; readonly branches: readonly (readonly Item[])[] };

interface Item {
  readonly atom: Atom;
  readonly min: number;
  readonly max: number;
  /** Offset of the item in the pattern. */
  readonly start: number;
}

/**
 * A pattern that starts with a literal prefix, then an unbounded repetition of a class that holds the prefix's
 * characters, then a literal anchor character outside that class: `[A-Za-z0-9._%+-]+@...` (empty prefix) or
 * `eyJ[A-Za-z0-9_-]+\....`. A match can start only inside the class run that ends at an anchor, and every start in
 * one run meets the same anchor, so a matcher needs one anchored attempt per anchor.
 */
export interface Lead {
  readonly prefix: string;
  readonly inClass: (char: string) => boolean;
  readonly anchor: string;
  /** The pattern from the anchor on. */
  readonly rest: string;
}

export interface PatternFacts {
  /** The fewest characters a match has. */
  readonly minLength: number;
  /** For each top-level alternative, a literal every match of it contains; undefined when one has none. */
  readonly literals: readonly string[] | undefined;
  readonly lead: Lead | undefined;
  /**
   * A regular expression that finds, in any text that holds a match, a member of a class followed by a literal
   * character that a top-level alternative always holds (`[0-9A-Fa-f]:` for the ipv6 rule); undefined when an
   * alternative holds no such pair.
   */
  readonly pairs: RegExp | undefined;
}

/** The facts of `pattern`, or none (no guard) when it does not parse as the subset. */
export function patternFacts(pattern: string): PatternFacts {
  try {
    const branches = parse(pattern);
    const literals = branches.map(literalOf);
    const pairs = branches.map(pairOf);
    return {
      minLength: minLength(branches),
      literals: literals.every((l) => l !== undefined) ? (literals as string[]) : undefined,
      lead: leadOf(branches, pattern),
      pairs: pairs.every((p) => p !== undefined)
        ? new RegExp((pairs as string[]).join("|"))
        : undefined,
    };
  } catch {
    return { minLength: 0, literals: undefined, lead: undefined, pairs: undefined };
  }
}

function parse(pattern: string): Item[][] {
  let i = 0;
  const fail = (): never => {
    throw new Error(`pattern ${pattern} is outside the scrub subset at ${i}`);
  };
  const unescape = (c: string | undefined): string =>
    c === "n" ? "\n" : c === "t" ? "\t" : (c ?? fail());
  const charClass = (): CharClass => {
    i += 1;
    const negated = pattern[i] === "^";
    if (negated) {
      i += 1;
    }
    const members: boolean[] = Array.from({ length: 128 }, () => false);
    const first = i;
    while (pattern[i] !== "]" || i === first) {
      let from = pattern[i] ?? fail();
      if (from === "\\") {
        i += 1;
        from = unescape(pattern[i]);
      }
      i += 1;
      let to = from;
      if (pattern[i] === "-" && pattern[i + 1] !== "]" && pattern[i + 1] !== undefined) {
        to = pattern[i + 1] === "\\" ? unescape(pattern[i + 2]) : (pattern[i + 1] as string);
        i += pattern[i + 1] === "\\" ? 3 : 2;
      }
      for (let code = from.charCodeAt(0); code <= to.charCodeAt(0) && code < 128; code += 1) {
        members[code] = true;
      }
    }
    i += 1;
    return { members, negated };
  };
  const quantifier = (): [number, number] => {
    const c = pattern[i];
    if (c === "?" || c === "*" || c === "+") {
      i += 1;
      return c === "?"
        ? [0, 1]
        : c === "*"
          ? [0, Number.POSITIVE_INFINITY]
          : [1, Number.POSITIVE_INFINITY];
    }
    if (c !== "{") {
      return [1, 1];
    }
    const close = pattern.indexOf("}", i);
    const [low, high] = pattern.slice(i + 1, close).split(",");
    i = close + 1;
    const min = Number(low);
    const max = high === undefined ? min : high === "" ? Number.POSITIVE_INFINITY : Number(high);
    return Number.isInteger(min) && (Number.isInteger(max) || max === Number.POSITIVE_INFINITY)
      ? [min, max]
      : fail();
  };
  const alternation = (): Item[][] => {
    const branches: Item[][] = [[]];
    while (i < pattern.length && pattern[i] !== ")") {
      if (pattern[i] === "|") {
        i += 1;
        branches.push([]);
        continue;
      }
      const start = i;
      let atom: Atom;
      if (pattern.startsWith("(?:", i)) {
        i += 3;
        const inner = alternation();
        if (pattern[i] !== ")") {
          fail();
        }
        i += 1;
        atom = { kind: "group", branches: inner };
      } else if (pattern[i] === "[") {
        atom = { kind: "class", cls: charClass() };
      } else if (pattern[i] === "\\") {
        atom = { kind: "char", char: unescape(pattern[i + 1]) };
        i += 2;
      } else {
        atom = { kind: "char", char: pattern[i] as string };
        i += 1;
      }
      const [min, max] = quantifier();
      (branches.at(-1) as Item[]).push({ atom, min, max, start });
    }
    return branches;
  };
  const branches = alternation();
  return i === pattern.length ? branches : fail();
}

function member(cls: CharClass, char: string): boolean {
  const code = char.charCodeAt(0);
  return cls.negated !== (code < 128 && cls.members[code] === true);
}

function minLength(branches: readonly (readonly Item[])[]): number {
  return Math.min(
    ...branches.map((items) =>
      items.reduce(
        (sum, item) =>
          sum + item.min * (item.atom.kind === "group" ? minLength(item.atom.branches) : 1),
        0,
      ),
    ),
  );
}

/** The character an atom always matches: a literal, or a class with one member. */
function single(atom: Atom): string | undefined {
  if (atom.kind === "char") {
    return atom.char;
  }
  if (atom.kind === "class" && !atom.cls.negated) {
    const codes = atom.cls.members.flatMap((on, code) => (on ? [code] : []));
    return codes.length === 1 ? String.fromCharCode(codes[0] as number) : undefined;
  }
  return undefined;
}

/** The longest run of literal characters every match of `items` contains, if any. */
function literalOf(items: readonly Item[]): string | undefined {
  const runs: string[] = [];
  let run = "";
  const end = (): void => {
    if (run !== "") {
      runs.push(run);
    }
    run = "";
  };
  for (const item of items) {
    const char = single(item.atom);
    if (char !== undefined && item.min >= 1) {
      run += char.repeat(item.min);
      if (item.max !== item.min) {
        end();
      }
      continue;
    }
    end();
    if (item.atom.kind === "group" && item.min >= 1 && item.atom.branches.length === 1) {
      const inner = literalOf(item.atom.branches[0] as readonly Item[]);
      if (inner !== undefined) {
        runs.push(inner);
      }
    }
  }
  end();
  let best: string | undefined;
  for (const r of runs) {
    if (best === undefined || r.length > best.length) {
      best = r;
    }
  }
  return best;
}

/** The source of a pattern that finds one character, written as an escape so that no character is special. */
function escaped(code: number): string {
  return code < 256
    ? `\\x${code.toString(16).padStart(2, "0")}`
    : `\\u${code.toString(16).padStart(4, "0")}`;
}

/**
 * A class item followed by a literal character item, both matched at least once, that every match of `items`
 * holds, as the source of a pattern that finds the pair: the last character the class item matches comes right
 * before the literal. A group matched at least once with one alternative holds the pairs of its items.
 */
function pairOf(items: readonly Item[]): string | undefined {
  for (let k = 0; k + 1 < items.length; k += 1) {
    const [first, second] = [items[k] as Item, items[k + 1] as Item];
    if (
      first.atom.kind === "class" &&
      first.min >= 1 &&
      second.atom.kind === "char" &&
      second.min >= 1
    ) {
      const { members, negated } = first.atom.cls;
      const codes = members.flatMap((on, code) => (on ? [escaped(code)] : []));
      return `[${negated ? "^" : ""}${codes.join("")}]${escaped(second.atom.char.charCodeAt(0))}`;
    }
  }
  for (const item of items) {
    if (item.atom.kind === "group" && item.min >= 1 && item.atom.branches.length === 1) {
      const inner = pairOf(item.atom.branches[0] as readonly Item[]);
      if (inner !== undefined) {
        return inner;
      }
    }
  }
  return undefined;
}

/** The character of an item that is one literal character, matched once. */
function once(item: Item | undefined): string | undefined {
  return item?.atom.kind === "char" && item.min === 1 && item.max === 1
    ? item.atom.char
    : undefined;
}

function leadOf(branches: readonly (readonly Item[])[], pattern: string): Lead | undefined {
  const [items, ...others] = branches;
  if (items === undefined || others.length > 0) {
    return undefined;
  }
  let k = 0;
  let prefix = "";
  for (let char = once(items[0]); char !== undefined; char = once(items[k])) {
    prefix += char;
    k += 1;
  }
  const repeated = items[k];
  const anchor = once(items[k + 1]);
  if (
    repeated?.atom.kind !== "class" ||
    repeated.min < 1 ||
    repeated.max !== Number.POSITIVE_INFINITY ||
    anchor === undefined
  ) {
    return undefined;
  }
  const { cls } = repeated.atom;
  const inClass = (char: string): boolean => member(cls, char);
  if (inClass(anchor) || [...prefix].some((c) => !inClass(c))) {
    return undefined;
  }
  return { prefix, inClass, anchor, rest: pattern.slice((items[k + 1] as Item).start) };
}
