import { describe, expect, test } from "vitest";
import type { MetricsFilter } from "@/api/generated/MetricsFilter";
import { conditionsOf, filterOf, withoutValue, withValue } from "./widget-filter";

const prod: MetricsFilter = { in: { field: "environment", values: ["prod"] } };
const notErrors: MetricsFilter = { not: { in: { field: "level", values: ["error"] } } };

describe("a widget's filter in the sheet", () => {
  test("a flat list of conditions reads as conditions and writes back the same filter", () => {
    expect(conditionsOf(undefined)).toEqual([]);
    expect(conditionsOf(prod)).toEqual([
      { field: "environment", negated: false, values: ["prod"] },
    ]);
    const both: MetricsFilter = { and: [prod, notErrors] };
    const conditions = conditionsOf(both);
    expect(conditions).toEqual([
      { field: "environment", negated: false, values: ["prod"] },
      { field: "level", negated: true, values: ["error"] },
    ]);
    expect(filterOf(conditions ?? [])).toEqual(both);
    expect(filterOf([{ field: "environment", negated: false, values: ["prod"] }])).toEqual(prod);
    expect(filterOf([])).toBeUndefined();
  });

  test("a shape the sheet can't show is reported, so the sheet keeps it as it is", () => {
    expect(conditionsOf({ or: [prod, notErrors] })).toBeNull();
    expect(conditionsOf({ and: [prod, { or: [prod] }] })).toBeNull();
    expect(conditionsOf({ not: { and: [prod] } })).toBeNull();
  });

  test("a value joins its field's condition of the same match, and leaving the last one drops it", () => {
    let conditions = withValue([], "environment", false, "prod");
    conditions = withValue(conditions, "environment", false, "staging");
    conditions = withValue(conditions, "environment", false, "prod");
    conditions = withValue(conditions, "environment", true, "dev");
    expect(conditions).toEqual([
      { field: "environment", negated: false, values: ["prod", "staging"] },
      { field: "environment", negated: true, values: ["dev"] },
    ]);
    expect(filterOf(conditions)).toEqual({
      and: [
        { in: { field: "environment", values: ["prod", "staging"] } },
        { not: { in: { field: "environment", values: ["dev"] } } },
      ],
    });
    conditions = withoutValue(conditions, "environment", true, "dev");
    expect(conditions).toEqual([
      { field: "environment", negated: false, values: ["prod", "staging"] },
    ]);
    expect(withoutValue(conditions, "environment", false, "prod")).toEqual([
      { field: "environment", negated: false, values: ["staging"] },
    ]);
  });
});
