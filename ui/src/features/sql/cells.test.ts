import { expect, test } from "vitest";
import {
  enumValues,
  firstLine,
  formatBytes,
  formatCell,
  isNumericType,
  offsetAt,
  shortType,
} from "./cells";

test("formatCell shows null distinctly and keeps 64-bit integer strings whole", () => {
  expect(formatCell(null)).toEqual({ kind: "null", text: "null" });
  expect(formatCell("18446744073709551615")).toEqual({
    kind: "text",
    text: "18446744073709551615",
  });
  expect(formatCell(12.5)).toEqual({ kind: "number", text: "12.5" });
  expect(formatCell(true)).toEqual({ kind: "text", text: "true" });
  expect(formatCell("")).toEqual({ kind: "text", text: "" });
});

test("formatCell writes nested values as compact JSON", () => {
  expect(formatCell(["a", 1, null])).toEqual({ kind: "json", text: '["a",1,null]' });
  expect(formatCell({ k: { n: 1 } })).toEqual({ kind: "json", text: '{"k":{"n":1}}' });
});

test("isNumericType reads through Nullable and LowCardinality", () => {
  expect(isNumericType("UInt64")).toBe(true);
  expect(isNumericType("Nullable(Float64)")).toBe(true);
  expect(isNumericType("Decimal(18, 6)")).toBe(true);
  expect(isNumericType("String")).toBe(false);
  expect(isNumericType("DateTime64(6)")).toBe(false);
  expect(isNumericType("Array(UInt8)")).toBe(false);
});

test("shortType drops enum value lists, inside wrappers too, and leaves other types whole", () => {
  expect(shortType("Enum8('unset' = 0, 'ok' = 1, 'error' = 2)")).toBe("Enum8");
  expect(shortType("Nullable(Enum16('a' = 1))")).toBe("Nullable(Enum16)");
  expect(shortType("Array(Enum8('a' = 1, 'b' = 2))")).toBe("Array(Enum8)");
  expect(shortType("Enum8('it\\'s (odd)' = 1, 'x' = 2)")).toBe("Enum8");
  expect(shortType("LowCardinality(String)")).toBe("LowCardinality(String)");
  expect(shortType("DateTime64(6, 'UTC')")).toBe("DateTime64(6, 'UTC')");
});

test("enumValues lists an enum's value names in order, unescaped", () => {
  expect(enumValues("Enum8('unset' = 0, 'ok' = 1, 'error' = 2)")).toEqual(["unset", "ok", "error"]);
  expect(enumValues("Nullable(Enum16('a' = 1, 'b c' = 2))")).toEqual(["a", "b c"]);
  expect(enumValues("Enum8('it\\'s (odd)' = 1)")).toEqual(["it's (odd)"]);
  expect(enumValues("LowCardinality(String)")).toEqual([]);
});

test("formatBytes picks a binary unit", () => {
  expect(formatBytes(0)).toBe("0 B");
  expect(formatBytes(1023)).toBe("1023 B");
  expect(formatBytes(1536)).toBe("1.5 KiB");
  expect(formatBytes(16 * 1024 * 1024)).toBe("16.0 MiB");
  expect(formatBytes(300 * 1024 * 1024)).toBe("300 MiB");
});

test("firstLine skips blank lines", () => {
  expect(firstLine("\n  \n  SELECT 1\nFROM t")).toBe("SELECT 1");
  expect(firstLine("")).toBe("");
});

test("offsetAt turns a one-based line and column into a text offset", () => {
  const text = "SELECT *\nFROM spans\nWHERE x";
  expect(offsetAt(text, 1, 1)).toBe(0);
  expect(offsetAt(text, 2, 6)).toBe(14);
  expect(text.slice(offsetAt(text, 3, 7))).toBe("x");
  expect(offsetAt(text, 9, 99)).toBe(text.length);
});
