// Scrub rules, content capture and the keyed hashes, as pure functions.

import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { captured, decide, glob, setCaptureWorkHook } from "../src/mcp/capture.ts";
import { sessionKey, stateHash } from "../src/mcp/wire.ts";
import { patternFacts } from "../src/pattern.ts";
import packaged from "../src/scrub-rules.json" with { type: "json" };
import { cutUtf8, scrub, scrubPlain, scrubWithRule, setMatcherHook } from "../src/scrub.ts";

const shared = JSON.parse(
  readFileSync(new URL("../../../conformance/scrub-rules.json", import.meta.url), "utf8"),
) as typeof packaged;

describe("scrub rules", () => {
  test("the packaged rules are the shared conformance rules", () => {
    expect(packaged).toEqual(shared);
  });

  for (const rule of shared.rules) {
    test(`${rule.name}: its positive inputs change and its negative inputs do not`, () => {
      for (const input of rule.positive) {
        expect(scrubWithRule(rule.name, input)).not.toBe(input);
      }
      for (const input of rule.negative) {
        expect(scrubWithRule(rule.name, input)).toBe(input);
      }
    });
  }

  test.each(shared.examples.map((example) => [example.input, example.output]))(
    "example %#: the whole list",
    (input, output) => {
      expect(scrub(input)).toBe(output);
    },
  );

  test("16 KiB of text without a match scrubs in linear time", () => {
    const words = "lorem ipsum 0123456789abcdef dolor@ sit-amet 12:34 a.b.c ".repeat(300);
    const text = words.slice(0, 16_384);
    const started = performance.now();
    for (let i = 0; i < 20; i += 1) {
      scrub(text);
    }
    const perCallMs = (performance.now() - started) / 20;
    expect(perCallMs).toBeLessThan(20);
  });
});

const run = (unit: string, length: number): string =>
  unit.repeat(Math.ceil(length / unit.length)).slice(0, length);

/** Inputs that make a backtracking matcher slow on the shared rules, about 16 KiB each. */
const ADVERSARIAL: [string, string][] = [
  ["letters", "a".repeat(16_384)],
  ["digits", "1".repeat(16_384)],
  ["digit pairs", "1 ".repeat(8_192)],
  ["at signs", "a@".repeat(8_192)],
  ["dots", "a.".repeat(8_192)],
  ["one at sign", `${"a".repeat(8_000)}@${"a".repeat(8_000)}`],
  ["hex", run("0123456789abcdef", 16_384)],
  ["alphanumeric", run("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789", 16_384)],
  [
    "base64",
    Buffer.from(Uint8Array.from({ length: 12_288 }, (_, i) => i % 256)).toString("base64"),
  ],
  ["pem header", `-----BEGIN A-----${"A".repeat(16_000)}`],
  ["jwt prefixes", "eyJ".repeat(5_461)],
  ["jwt chain", `${"eyJ".repeat(5_000)}${".eyJ".repeat(400)}`],
  ["bearer", "bearer ".repeat(2_340)],
  ["colons", "1:".repeat(8_192)],
  ["plus ones", "+1".repeat(8_192)],
  ["phone groups", "(415) ".repeat(2_730)],
  ["secret key prefixes", "csk_".repeat(4_096)],
];

/** A seeded xorshift generator of numbers in [0, 1). */
function seeded(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    return state / 2 ** 32;
  };
}

/** Pieces of matches, near misses and separators that the generated text is built from. */
const FRAGMENTS = [
  " ",
  "a",
  "0",
  "7",
  "@",
  ".",
  ":",
  "-",
  "_",
  "+",
  "(",
  ")",
  "/",
  "=",
  "%",
  "\n",
  "é",
  "😀",
  "eyJ",
  "csk_",
  "bearer ",
  "Bearer\t",
  "-----BEGIN ",
  "-----END ",
  "-----",
  "KEY",
  "::",
  "@@",
  "a@b",
  "a@b.c",
  "x@y.zz",
  "ada.lovelace@example.com",
  "ops+alerts@mail.example.co.uk",
  "+1 415 555 0134",
  "(415) 555-0134",
  "4111 1111 1111 1111",
  "4111-1111-1111-1112",
  "378282246310005",
  "Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0",
  "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo",
  "203.0.113.42",
  "2001:db8::8a2e:370:7334",
  "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhZGEifQ.c2lnbmF0dXJl",
  "eyJa.b",
  "eyJeyJ.x.",
  "eyJ.",
  "-----BEGIN KEY-----\nMIIB\n-----END KEY-----",
];
const RUN_UNITS = ["a", "Z", "9", "f", ".", "-", "_", "+", "%", "ab@", "eyJ", "1 "];

function generated(random: () => number): string {
  let text = "";
  const parts = Math.floor(random() * 14);
  for (let i = 0; i < parts; i += 1) {
    text +=
      random() < 0.7
        ? (FRAGMENTS[Math.floor(random() * FRAGMENTS.length)] ?? "")
        : (RUN_UNITS[Math.floor(random() * RUN_UNITS.length)] ?? "").repeat(
            1 + Math.floor(random() * 40),
          );
  }
  return text;
}

/** Matcher steps of scrubbing `text` with the whole rule list. */
function matcherSteps(text: string): number {
  let steps = 0;
  setMatcherHook((_, s) => {
    steps += s;
  });
  try {
    scrub(text);
  } finally {
    setMatcherHook(undefined);
  }
  return steps;
}

describe("scrubbing work", () => {
  test("the rules' patterns yield the facts the guards use", () => {
    const facts = Object.fromEntries(
      shared.rules.map((rule) => {
        const { minLength, literals, lead } = patternFacts(rule.pattern);
        return [rule.name, { minLength, literals, lead: lead && [lead.prefix, lead.anchor] }];
      }),
    );
    expect(facts).toEqual({
      pem: { minLength: 32, literals: ["-----BEGIN "], lead: undefined },
      jwt: { minLength: 7, literals: ["eyJ"], lead: ["eyJ", "."] },
      bearer: { minLength: 23, literals: undefined, lead: undefined },
      secret_key: { minLength: 12, literals: ["csk_"], lead: undefined },
      card: { minLength: 14, literals: undefined, lead: undefined },
      email: { minLength: 6, literals: ["@"], lead: ["", "@"] },
      ipv6: { minLength: 6, literals: [":", ":"], lead: undefined },
      ipv4: { minLength: 7, literals: ["."], lead: undefined },
      phone: { minLength: 8, literals: undefined, lead: undefined },
    });
  });

  test.each(ADVERSARIAL)("%s: matcher steps stay linear in the length", (_, text) => {
    expect(matcherSteps(text)).toBeLessThanOrEqual(2 * shared.rules.length * text.length);
  });

  test("guarded application equals plain application over generated text", () => {
    const random = seeded(20_261_118);
    const mismatches: string[] = [];
    for (let i = 0; i < 3_000; i += 1) {
      const text = generated(random);
      for (const rule of shared.rules) {
        if (scrubWithRule(rule.name, text) !== scrubPlain(text, rule.name)) {
          mismatches.push(`${rule.name}: ${JSON.stringify(text)}`);
        }
      }
      if (scrub(text) !== scrubPlain(text)) {
        mismatches.push(`all rules: ${JSON.stringify(text)}`);
      }
      if (matcherSteps(text) > 2 * shared.rules.length * Math.max(text.length, 1)) {
        mismatches.push(`steps: ${JSON.stringify(text)}`);
      }
    }
    expect(mismatches.slice(0, 5)).toEqual([]);
  });

  test("a long run scrubs within a generous ceiling", () => {
    const text = `${"a".repeat(65_536)}@${"a".repeat(65_536)}`;
    const started = performance.now();
    scrub(text);
    expect(performance.now() - started).toBeLessThan(2_000);
  });
});

describe("capture", () => {
  test("glob matches * and ?", () => {
    expect(glob("*", "anything")).toBe(true);
    expect(glob("lookup_*", "lookup_order")).toBe(true);
    expect(glob("lookup_*", "order_lookup")).toBe(false);
    expect(glob("?cho", "echo")).toBe(true);
    expect(glob("e*o", "echo")).toBe(true);
    expect(glob("e*x", "echo")).toBe(false);
    expect(glob("a*b*c", "aXbYbZc")).toBe(true);
  });

  test("no rule means no capture; the first matching rule wins", () => {
    expect(decide(undefined, "echo")).toBeUndefined();
    expect(decide([{ tool: "other" }], "echo")).toBeUndefined();
    expect(decide([{ tool: "e*", args: false }, { tool: "*" }], "echo")).toEqual({
      args: false,
      result: true,
      sampleRate: 1,
      maxBytes: 16_384,
    });
  });

  test("a sampled-out call never reads its arguments", () => {
    let reads = 0;
    const args = {
      toJSON() {
        reads += 1;
        throw new Error("read");
      },
    };
    for (let i = 0; i < 100; i += 1) {
      const decision = decide([{ tool: "*", sampleRate: 0 }], "echo");
      if (decision !== undefined) {
        captured(args, decision.maxBytes);
      }
    }
    expect(reads).toBe(0);
  });

  test("content is cut at the cap on a character boundary and marked", () => {
    const value = { text: "é".repeat(20_000) };
    const result = captured(value, 16_384);
    expect(result?.cut).toBe(true);
    expect(Buffer.byteLength(result?.text ?? "")).toBeLessThanOrEqual(16_384);
    expect(Buffer.byteLength(result?.text ?? "")).toBeGreaterThan(16_380);
    expect(result?.text.endsWith("é")).toBe(true);
    expect(captured({ text: "short" }, 16_384)).toEqual({ text: '{"text":"short"}', cut: false });
  });

  test("a secret that straddles the cap is scrubbed before the cut", () => {
    const padding = "x".repeat(16_370);
    const result = captured({ note: `${padding} alice@example.com` }, 16_384);
    expect(result?.text).not.toContain("alice@");
    expect(result?.text).not.toContain("@example");
  });

  test("a token longer than the scrub margin that straddles the cap leaves no prefix behind", () => {
    const token = `eyJhbGciOiJIUzI1NiJ9.eyJ${"a".repeat(3_000)}.c2lnbmF0dXJl`;
    const padding = "x".repeat(16_384 - 200);
    const result = captured({ note: `${padding} ${token}` }, 16_384);
    expect(result?.cut).toBe(true);
    expect(result?.text).not.toContain("eyJ");
    expect(result?.text).toBe(`{"note":"${padding} `);
  });

  test("a large value is serialized no further than the cap and the scrub margin", () => {
    let serialized = 0;
    setCaptureWorkHook((s) => {
      serialized = Math.max(serialized, s);
    });
    try {
      for (const value of [
        { text: "x".repeat(5 * 1024 * 1024) },
        ["a".repeat(70_000)],
        Array.from({ length: 100_000 }, (_, i) => i),
        { nested: { deep: [{ s: "é\n😀".repeat(100_000) }] } },
      ]) {
        captured(value, 16_384);
      }
    } finally {
      setCaptureWorkHook(undefined);
    }
    expect(serialized).toBeLessThanOrEqual(16_384 + 1_024);
  });

  test("a value that does not serialize is not captured", () => {
    expect(captured({ big: 1n }, 16_384)).toBeUndefined();
  });

  test("cutUtf8 never splits a surrogate pair", () => {
    expect(cutUtf8("ab😀", 5)).toBe("ab");
    expect(cutUtf8("ab😀", 6)).toBe("ab😀");
  });
});

describe("keyed hashes", () => {
  // Vectors from the SDK conformance registry draft (contract 9.4 and 9.6).
  test("mcp.session.id is the first 16 bytes of SHA-256 of the raw id", () => {
    expect(sessionKey("3f8e1c2a-5b7d-4e9f-a1c3-d5e7f9a1b3c5")).toBe(
      "84757436d5e0791bd4a8eaa6f1d05986",
    );
    expect(sessionKey("conformance-session")).toBe("b1427bda7462603423a9e4a7b73fbdd0");
  });

  test("the multi-round state hash is an HMAC keyed with the secret key", () => {
    const key = "csk_conformance00000000000000000000000000000000";
    expect(stateHash(key, "crucible-conformance-request-state-1")).toBe(
      "59c50f1c773bc1354d420d974c06c5af",
    );
    expect(stateHash(key, "eyJzdGVwIjoiY29uZmlybWVkIn0.c2ln")).toBe(
      "bec039d76aab792bd526d369321fb0c0",
    );
  });
});
