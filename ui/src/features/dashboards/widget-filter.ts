import type { MetricsFilter } from "@/api/generated/MetricsFilter";

// One condition the sheet edits: the field is, or is not, one of the values.
export interface FilterCondition {
  field: string;
  negated: boolean;
  values: string[];
}

function condition(node: MetricsFilter): FilterCondition | null {
  if ("in" in node) return { field: node.in.field, negated: false, values: node.in.values };
  if ("not" in node && "in" in node.not) {
    return { field: node.not.in.field, negated: true, values: node.not.in.values };
  }
  return null;
}

// The conditions of a widget's filter, or null when the filter has a shape the sheet doesn't edit (an `or`,
// a nested `and`), which the sheet then keeps as it is.
export function conditionsOf(filter: MetricsFilter | undefined): FilterCondition[] | null {
  if (filter === undefined) return [];
  const conditions = ("and" in filter ? filter.and : [filter]).map(condition);
  return conditions.every((c) => c !== null) ? (conditions as FilterCondition[]) : null;
}

// The filter the conditions spell: none, one condition, or all of them under `and`.
export function filterOf(conditions: readonly FilterCondition[]): MetricsFilter | undefined {
  const nodes = conditions
    .filter((c) => c.values.length > 0)
    .map((c): MetricsFilter => {
      const node: MetricsFilter = { in: { field: c.field, values: [...c.values] } };
      return c.negated ? { not: node } : node;
    });
  if (nodes.length === 0) return undefined;
  return nodes.length === 1 ? nodes[0] : { and: nodes };
}

export function withValue(
  conditions: readonly FilterCondition[],
  field: string,
  negated: boolean,
  value: string,
): FilterCondition[] {
  const at = conditions.findIndex((c) => c.field === field && c.negated === negated);
  if (at < 0) return [...conditions, { field, negated, values: [value] }];
  return conditions.map((c, i) =>
    i === at && !c.values.includes(value) ? { ...c, values: [...c.values, value] } : c,
  );
}

export function withoutValue(
  conditions: readonly FilterCondition[],
  field: string,
  negated: boolean,
  value: string,
): FilterCondition[] {
  return conditions
    .map((c) =>
      c.field === field && c.negated === negated
        ? { ...c, values: c.values.filter((v) => v !== value) }
        : c,
    )
    .filter((c) => c.values.length > 0);
}
