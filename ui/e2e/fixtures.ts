import { AxeBuilder } from "@axe-core/playwright";
import { test as base, expect, type Page, type Route } from "@playwright/test";
import type { SavedQuery } from "../src/api/generated/SavedQuery";
import type { Span } from "../src/api/generated/Span";
import type { SpanPage } from "../src/api/generated/SpanPage";
import type { SqlRelation } from "../src/api/generated/SqlRelation";
import type { SqlResult } from "../src/api/generated/SqlResult";
import type { SqlRun } from "../src/api/generated/SqlRun";
import type { SqlSchema } from "../src/api/generated/SqlSchema";
import type { TraceDetail } from "../src/api/generated/TraceDetail";
import type { TraceListItem } from "../src/api/generated/TraceListItem";
import type { TraceListPage } from "../src/api/generated/TraceListPage";

export const PROJECT = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";
export const TRACE = "4bf92f3577b34da6a3ce929d0e0e4736";
export const AS_OF = "2026-11-17T14:00:05.000000Z";

const BASE_US = Date.parse("2026-11-17T14:00:00Z") * 1_000;

function at(offsetUs: number): string {
  const us = BASE_US + offsetUs;
  const seconds = new Date(Math.floor(us / 1_000_000) * 1_000).toISOString().slice(0, 19);
  return `${seconds}.${String(us % 1_000_000).padStart(6, "0")}Z`;
}

export function span(
  id: string,
  parent: string | null,
  startUs: number,
  durationUs: number,
  fields: Partial<Span> = {},
): Span {
  return {
    span_id: id,
    parent_span_id: parent,
    name: `span ${id}`,
    observation_type: "span",
    start_time: at(startUs),
    end_time: at(startUs + durationUs),
    duration_us: durationUs,
    status_code: "ok",
    level: "default",
    model: "",
    total_cost_usd: 0,
    input_tokens: 0,
    output_tokens: 0,
    total_tokens: 0,
    child_count: null,
    io_shape: null,
    saturated: false,
    ...fields,
  };
}

const generation = (model: string, input: number, output: number, cost: number): Partial<Span> => ({
  observation_type: "generation",
  model,
  input_tokens: input,
  output_tokens: output,
  total_tokens: input + output,
  total_cost_usd: cost,
});

// A support agent that plans, searches twice, hands off to a reviewer agent and fails to send.
export const AGENT_SPANS: Span[] = [
  span("a000000000000001", null, 0, 4_200_000, {
    name: "support agent",
    observation_type: "agent",
  }),
  span("a000000000000002", "a000000000000001", 20_000, 900_000, {
    name: "plan reply",
    ...generation("gpt-4.1", 1_200, 180, 0.0031),
  }),
  span("a000000000000003", "a000000000000001", 940_000, 610_000, {
    name: "search_docs",
    observation_type: "tool",
  }),
  span("a000000000000004", "a000000000000001", 1_560_000, 420, {
    name: "search_docs",
    observation_type: "tool",
  }),
  span("a000000000000005", "a000000000000001", 1_600_000, 1_900_000, {
    name: "review agent",
    observation_type: "agent",
  }),
  span("a000000000000006", "a000000000000005", 1_650_000, 1_200_000, {
    name: "draft review",
    ...generation("gpt-4.1-mini", 2_400, 320, 0.0009),
  }),
  span("a000000000000007", "a000000000000005", 2_900_000, 520_000, {
    name: "policy check",
    observation_type: "guardrail",
  }),
  span("a000000000000008", "a000000000000001", 3_560_000, 600_000, {
    name: "send_reply",
    observation_type: "tool",
    status_code: "error",
    level: "error",
  }),
];

export function traceDetail(fields: Partial<TraceDetail> = {}): TraceDetail {
  const spans = fields.spans ?? AGENT_SPANS;
  return {
    trace_id: TRACE,
    span_count: spans.length,
    paged: false,
    spans,
    meta: { as_of: AS_OF, partial: false, consistency: "strong" },
    saturated: false,
    ...fields,
  };
}

export function spanPage(data: Span[], next: string | null = null): SpanPage {
  return {
    data,
    next_cursor: next,
    has_more: next !== null,
    meta: { as_of: AS_OF, partial: false, consistency: "strong" },
  };
}

export function traceItem(fields: Partial<TraceListItem> = {}): TraceListItem {
  return {
    trace_id: TRACE,
    name: "support agent",
    trace_name: "support agent",
    head_kind: "root",
    start_time: at(0),
    duration_us: 4_200_000,
    status_code: "ok",
    level: "default",
    span_count: 8,
    error_count: 1,
    total_cost_usd: 0.004,
    input_tokens: 3_600,
    output_tokens: 500,
    total_tokens: 4_100,
    user_id: "",
    session_id: "",
    environment: "production",
    release: "",
    tags: [],
    saturated: false,
    ...fields,
  };
}

export function tracePage(
  data: TraceListItem[],
  fields: Partial<TraceListPage> = {},
): TraceListPage {
  return {
    data,
    next_cursor: null,
    has_more: false,
    head_cursor: null,
    meta: { as_of: AS_OF, partial: false, consistency: "strong" },
    ...fields,
  };
}

type Reply = Parameters<Route["fulfill"]>[0];

export function json(body: unknown, status = 200): Reply {
  return { status, contentType: "application/json", body: JSON.stringify(body) };
}

// Server text in `title` and `detail` must never reach the page; tests assert it doesn't.
export const SERVER_TEXT = "raw server detail";

export function problem(status: number, code: string, requestId = "0193a5b2-7c1d-7e4f"): Reply {
  return {
    status,
    contentType: "application/problem+json",
    headers: { "x-request-id": requestId },
    body: JSON.stringify({
      type: `urn:crucible:problem:${code}`,
      title: SERVER_TEXT,
      status,
      detail: SERVER_TEXT,
      code,
      request_id: requestId,
    }),
  };
}

type Handler = (route: Route, url: URL) => Promise<void> | void;

const NO_OP = () => {};

export class Api {
  readonly calls: string[] = [];
  readonly unexpected: string[] = [];
  private readonly handlers: { path: RegExp; handler: Handler }[] = [];

  // A later handler for the same path wins.
  on(path: RegExp, handler: Handler | Reply): void {
    const run: Handler =
      typeof handler === "function" ? handler : (route) => route.fulfill(handler);
    this.handlers.unshift({ path, handler: run });
  }

  // Answers once `release` is called, so the page stays in its loading state until then.
  hold(path: RegExp, reply: Reply): () => void {
    let release = NO_OP;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    this.on(path, async (route) => {
      await gate;
      await route.fulfill(reply).catch(() => {});
    });
    return release;
  }

  count(path: RegExp): number {
    return this.calls.filter((call) => path.test(call.split("?")[0] ?? "")).length;
  }

  async handle(route: Route): Promise<void> {
    const url = new URL(route.request().url());
    this.calls.push(`${url.pathname}${url.search}`);
    const match = this.handlers.find(({ path }) => path.test(url.pathname));
    if (!match) {
      this.unexpected.push(url.pathname);
      await route.fulfill(problem(404, "not_found"));
      return;
    }
    await match.handler(route, url);
  }
}

export const TRACE_PATH = new RegExp(`/projects/${PROJECT}/traces/${TRACE}$`);
export const CHILDREN_PATH = new RegExp(`/projects/${PROJECT}/traces/${TRACE}/spans$`);
export const LIST_PATH = new RegExp(`/projects/${PROJECT}/traces$`);

export const test = base.extend<{ api: Api }>({
  api: async ({ page }, use) => {
    const api = new Api();
    const pageErrors: string[] = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    await page.route("**/api/v1/**", (route) => api.handle(route));
    await use(api);
    expect(api.unexpected, "API requests no handler answered").toEqual([]);
    expect(pageErrors, "uncaught errors in the page").toEqual([]);
  },
});

export { expect };

export function traceUrl(params: Record<string, string> = {}): string {
  return `/trace/?${new URLSearchParams({ project: PROJECT, trace: TRACE, ...params })}`;
}

// No serious or critical violations: the bar for every state of every surface.
export async function expectAccessible(page: Page, state: string): Promise<void> {
  const { violations } = await new AxeBuilder({ page }).analyze();
  const serious = violations
    .filter((violation) => violation.impact === "serious" || violation.impact === "critical")
    .map((violation) => `${violation.id}: ${violation.help} (${violation.nodes.length} nodes)`);
  expect(serious, `axe in ${state}`).toEqual([]);
}

export async function expectNoPageScrollX(page: Page): Promise<void> {
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  expect(overflow, "the page scrolls sideways").toBeLessThanOrEqual(0);
}

// The SQL workspace.

export const SQL_SCHEMA_PATH = new RegExp(`/projects/${PROJECT}/sql/schema$`);
export const SQL_RUN_PATH = new RegExp(`/projects/${PROJECT}/sql/run$`);
export const SQL_RUNS_PATH = new RegExp(`/projects/${PROJECT}/sql/runs$`);
export const SQL_EXPORT_PATH = new RegExp(`/projects/${PROJECT}/sql/export$`);
export const SQL_SAVED_PATH = new RegExp(`/projects/${PROJECT}/sql/saved-queries$`);
export const SQL_SAVED_ONE_PATH = new RegExp(`/projects/${PROJECT}/sql/saved-queries/[^/]+$`);
export const SQL_SAVED_RUN_PATH = new RegExp(`/projects/${PROJECT}/sql/saved-queries/[^/]+/run$`);

export const SQL_EXAMPLE = `SELECT name, count() AS spans
FROM crucible_sql.spans
WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}
GROUP BY name
ORDER BY spans DESC
LIMIT 100`;

export function sqlRelation(fields: Partial<SqlRelation> = {}): SqlRelation {
  return {
    name: "crucible_sql.spans",
    description: "One row per span of this project.",
    time_column: "start_time",
    window_required: true,
    columns: [
      { name: "start_time", type: "DateTime64(6)" },
      { name: "name", type: "String" },
      { name: "total_tokens", type: "UInt64" },
    ],
    example: SQL_EXAMPLE,
    ...fields,
  };
}

export function sqlSchema(fields: Partial<SqlSchema> = {}): SqlSchema {
  return {
    status: { enabled: true, failed_checks: [] },
    relations: [
      sqlRelation(),
      sqlRelation({
        name: "crucible_sql.span_metrics",
        description: "Span counts and token totals per minute.",
        time_column: "minute",
        window_required: false,
        columns: [
          { name: "minute", type: "DateTime" },
          { name: "spans", type: "UInt64" },
        ],
        example:
          "SELECT minute, spans FROM crucible_sql.span_metrics WHERE minute >= {from:DateTime64(6)} AND minute < {to:DateTime64(6)}",
      }),
    ],
    limits: {
      max_rows: 10_000,
      max_bytes: 16_777_216,
      max_cell_bytes: 65_536,
      max_execution_s: 30,
      max_sql_bytes: 65_536,
    },
    ...fields,
  };
}

export function sqlResult(fields: Partial<SqlResult> = {}): SqlResult {
  return {
    run_id: "0193a5b2-0000-7000-8000-000000000001",
    columns: [
      { name: "name", type: "String" },
      { name: "spans", type: "UInt64" },
      { name: "model", type: "Nullable(String)" },
      { name: "tags", type: "Array(String)" },
    ],
    rows: [
      ["support agent", "18446744073709551615", "gpt-4.1", ["prod", "eu"]],
      ["search_docs", "42", null, []],
    ],
    truncated: false,
    cut_cells: 0,
    stats: { rows_read: 120_000, bytes_read: 3_145_728, elapsed_ms: 85, estimated_rows: 130_000 },
    ...fields,
  };
}

export function sqlRun(fields: Partial<SqlRun> = {}): SqlRun {
  return {
    id: "0193a5b2-0000-7000-8000-00000000000a",
    actor: { kind: "api_key", id: "key-1" },
    kind: "ad_hoc",
    sql: "\nSELECT count() FROM crucible_sql.spans\nWHERE start_time >= {from:DateTime64(6)}",
    started_at: at(0),
    finished_at: at(85_000),
    outcome: "succeeded",
    result_rows: 1,
    truncated: false,
    rows_read: 120_000,
    elapsed_ms: 85,
    ...fields,
  };
}

export function savedQuery(fields: Partial<SavedQuery> = {}): SavedQuery {
  return {
    id: "0193a5b2-0000-7000-8000-0000000000f1",
    owner: { kind: "api_key", id: "key-1" },
    name: "Spans by model",
    description: "Span counts for one model.",
    visibility: "project",
    sql: `SELECT name, count() AS spans FROM crucible_sql.spans
WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)}
  AND model = {model:String} AND total_tokens >= {min_tokens:UInt64}
GROUP BY name`,
    parameters: [
      { name: "model", type: "string", default: "gpt-4.1" },
      { name: "min_tokens", type: "int" },
    ],
    version: 3,
    created_at: at(0),
    updated_at: at(0),
    can_edit: true,
    can_run: true,
    ...fields,
  };
}

// A workspace refusal: `detail` is the sentence that says how to fix it, and the page shows it.
export function sqlProblem(
  status: number,
  code: string,
  fields: {
    detail?: string;
    clickhouse?: string;
    line?: number;
    column?: number;
    retry_after_s?: number;
    reason?: string;
  } = {},
  requestId = "0193a5b2-7c1d-7e4f",
): Reply {
  const { clickhouse, ...rest } = fields;
  return {
    status,
    contentType: "application/problem+json",
    headers: { "x-request-id": requestId },
    body: JSON.stringify({
      type: `urn:crucible:problem:${code}`,
      title: SERVER_TEXT,
      status,
      code,
      request_id: requestId,
      ...rest,
      ...(clickhouse === undefined
        ? {}
        : { errors: [{ pointer: "/sql", code: "clickhouse", message: clickhouse }] }),
    }),
  };
}
