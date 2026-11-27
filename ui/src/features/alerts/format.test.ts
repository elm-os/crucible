import { describe, expect, test } from "vitest";
import {
  conditionText,
  dimensionsOf,
  formatSpan,
  formatValue,
  MEASURES,
  mcpReadText,
  measuresOf,
  seriesLabel,
  testFailure,
  thresholdFromStored,
  thresholdToStored,
  urlRefusal,
} from "./format";

describe("thresholds", () => {
  test("are stored in the unit of the measure", () => {
    expect(thresholdToStored("count", "5")).toBe(5);
    expect(thresholdToStored("count", " 1,200 ")).toBe(1200);
    expect(thresholdToStored("us", "250")).toBe(250_000);
    expect(thresholdToStored("us", "0.5")).toBe(500);
    expect(thresholdToStored("nusd", "$0.25")).toBe(250_000_000);
    expect(thresholdToStored("tokens", "-3")).toBe(-3);
    // An error rate is typed in percent and stored in parts per million.
    expect(thresholdToStored("ppm", "5")).toBe(50_000);
    expect(thresholdToStored("ppm", "0.25%")).toBe(2_500);
  });

  test("that are not numbers are refused, never guessed", () => {
    for (const text of ["", " ", "five", "1e3", "1.2.3", "5 ms", "9".repeat(20)]) {
      expect(thresholdToStored("count", text), text).toBeNull();
    }
  });

  test("read back as they were typed", () => {
    for (const [unit, text] of [
      ["count", "5"],
      ["us", "250"],
      ["us", "0.5"],
      ["nusd", "0.25"],
      ["ppm", "1.5"],
    ] as const) {
      const stored = thresholdToStored(unit, text);
      expect(stored).not.toBeNull();
      expect(thresholdFromStored(unit, stored ?? 0)).toBe(text);
    }
  });
});

describe("what a rule says", () => {
  test("values read in their unit", () => {
    expect(formatValue("count", 1200)).toBe("1,200");
    expect(formatValue("us", 250_000)).toBe("250 ms");
    expect(formatValue("nusd", 250_000_000)).toBe("$0.25");
    expect(formatValue("ppm", 12_500)).toBe("1.25%");
    expect(formatValue("ppm", 50_000)).toBe("5%");
  });

  test("spans of time read in their largest whole unit", () => {
    expect(formatSpan(90)).toBe("90 s");
    expect(formatSpan(300)).toBe("5 min");
    expect(formatSpan(14_400)).toBe("4 h");
    expect(formatSpan(5_400)).toBe("90 min");
  });

  test("the condition is one sentence", () => {
    expect(
      conditionText({
        measure: "error_count",
        aggregation: "sum",
        comparison: "gt",
        threshold: 5,
        unit: "count",
        window_secs: 300,
      }),
    ).toBe("Errors above 5 over 5 min");
    expect(
      conditionText({
        measure: "duration",
        aggregation: "p95",
        comparison: "gte",
        threshold: 2_000_000,
        unit: "us",
        window_secs: 3_600,
      }),
    ).toBe("Duration p95 at or above 2.0 s over 1 h");
    // An MCP measure names its own percentile, and its rate reads in percent.
    expect(
      conditionText({
        measure: "mcp.duration_p99",
        aggregation: "p99",
        comparison: "gt",
        threshold: 500_000,
        unit: "us",
        window_secs: 300,
      }),
    ).toBe("MCP duration p99 above 500 ms over 5 min");
    expect(
      conditionText({
        measure: "mcp.error_rate",
        aggregation: "rate",
        comparison: "gt",
        threshold: 50_000,
        unit: "ppm",
        window_secs: 300,
      }),
    ).toBe("MCP error rate above 5% over 5 min");
    expect(mcpReadText({ side: "client", capture_source: "proxy" })).toBe(
      "Client side, capture source Proxy",
    );
    expect(mcpReadText({})).toBe("Server side, capture source Automatic");
  });

  test("each source has its own measures and dimensions", () => {
    expect(measuresOf("mcp_ops").map((m) => m.value)).toEqual([
      "mcp.error_rate",
      "mcp.error_count",
      "mcp.calls",
      "mcp.duration_p50",
      "mcp.duration_p90",
      "mcp.duration_p99",
    ]);
    expect(measuresOf("spans").every((m) => !m.value.startsWith("mcp."))).toBe(true);
    expect(dimensionsOf("mcp_ops").map((d) => d.label)).toEqual(["None", "Server", "Tool"]);
    expect(dimensionsOf("spans").map((d) => d.value)).not.toContain("server_name");
  });

  test("a series is named by its dimension values", () => {
    expect(seriesLabel([])).toBe("All spans");
    expect(seriesLabel([], "mcp_ops")).toBe("All calls");
    expect(
      seriesLabel([
        { dimension: "environment", value: "prod" },
        { dimension: "model", value: "" },
      ]),
    ).toBe("environment=prod, model=(empty)");
  });

  test("every measure has an aggregation to start with", () => {
    for (const measure of MEASURES) expect(measure.aggregations.length).toBeGreaterThan(0);
  });
});

describe("failures are in the page's own words", () => {
  test("a test-send's code maps to what to check", () => {
    expect(testFailure("receiver_failed")).toBe(
      "The receiver ran into an error of its own. Check the receiver's logs.",
    );
    expect(testFailure("private_address")).toContain("egress.allow_cidrs");
    expect(testFailure("something_new")).toContain("wasn't delivered");
    expect(testFailure(undefined)).toContain("wasn't delivered");
  });

  test("a refused URL says which setting allows it", () => {
    expect(urlRefusal("metadata_address")).toContain("never calls");
    expect(urlRefusal("private_address")).toContain("egress.mode = open");
    expect(urlRefusal("https_required")).toContain("https://");
    expect(urlRefusal(undefined)).toContain("full URL");
  });
});
