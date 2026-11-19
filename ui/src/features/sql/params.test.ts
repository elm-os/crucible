import { describe, expect, test } from "vitest";
import {
  detectParameters,
  guessParameterType,
  parameterValues,
  parseParameterType,
  unwrap,
} from "./params";

describe("detectParameters", () => {
  test("finds each parameter once, in order, and leaves the window out", () => {
    const sql = `SELECT name FROM crucible_sql.spans
WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}
  AND model = {model:String} AND total_tokens > {min_tokens:UInt64} AND name != {model:String}`;
    expect(detectParameters(sql)).toEqual([
      { name: "model", type: "String" },
      { name: "min_tokens", type: "UInt64" },
    ]);
  });

  test("accepts spaces and nested type arguments", () => {
    expect(detectParameters("{ ids : Array(String) } {m:Map(String, UInt8)}")).toEqual([
      { name: "ids", type: "Array(String)" },
      { name: "m", type: "Map(String, UInt8)" },
    ]);
  });

  test("ignores braces that are not parameters", () => {
    expect(detectParameters("SELECT '{}' , {1:String}, {a}, map('k', 1)")).toEqual([]);
  });
});

test("guessParameterType maps ClickHouse types to declared types", () => {
  expect(guessParameterType("String")).toBe("string");
  expect(guessParameterType("UInt64")).toBe("int");
  expect(guessParameterType("Nullable(Int32)")).toBe("int");
  expect(guessParameterType("Float64")).toBe("float");
  expect(guessParameterType("Decimal(18, 6)")).toBe("float");
  expect(guessParameterType("Bool")).toBe("bool");
  expect(guessParameterType("DateTime64(6)")).toBe("timestamp");
  expect(guessParameterType("UUID")).toBe("uuid");
  expect(guessParameterType("Array(String)")).toBe("string");
});

test("unwrap removes Nullable and LowCardinality", () => {
  expect(unwrap("LowCardinality(Nullable(String))")).toBe("String");
  expect(unwrap("Array(Nullable(String))")).toBe("Array(Nullable(String))");
});

test("parseParameterType accepts only declared types", () => {
  expect(parseParameterType("uuid")).toBe("uuid");
  expect(parseParameterType("json")).toBeNull();
});

test("parameterValues sends a value for each used parameter and nothing else", () => {
  expect(parameterValues([{ name: "model" }, { name: "n" }], { model: "a", stale: "x" })).toEqual({
    model: "a",
    n: "",
  });
});
