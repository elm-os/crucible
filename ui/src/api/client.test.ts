import { afterEach, expect, test, vi } from "vitest";
import { newIdempotencyKey } from "./client";

afterEach(() => {
  vi.unstubAllGlobals();
});

test("an idempotency key needs no secure context and has the form the server accepts", () => {
  // Over plain HTTP the browser's `crypto` has `getRandomValues` but no `randomUUID`.
  const real = globalThis.crypto;
  vi.stubGlobal("crypto", { getRandomValues: (array: Uint8Array) => real.getRandomValues(array) });
  const first = newIdempotencyKey();
  expect(first).toMatch(/^[0-9a-f]{32}$/);
  expect(newIdempotencyKey()).not.toBe(first);
});
