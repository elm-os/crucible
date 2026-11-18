// Scrub rules, content capture and the keyed hashes, as pure functions.

import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";
import { captured, decide, glob } from "../src/mcp/capture.ts";
import { sessionKey, stateHash } from "../src/mcp/wire.ts";
import packaged from "../src/scrub-rules.json" with { type: "json" };
import { cutUtf8, scrub, scrubWithRule } from "../src/scrub.ts";

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
