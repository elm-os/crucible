import { AxeBuilder } from "@axe-core/playwright";
import { test as base, expect, type Page, type Request, type Route } from "@playwright/test";
import type { SavedQuery } from "../src/api/generated/SavedQuery";
import type { SessionInfo } from "../src/api/generated/SessionInfo";
import type { SessionItem } from "../src/api/generated/SessionItem";
import type { SignInOptions } from "../src/api/generated/SignInOptions";
import type { Span } from "../src/api/generated/Span";
import type { SpanListItem } from "../src/api/generated/SpanListItem";
import type { SpanPage } from "../src/api/generated/SpanPage";
import type { SqlRelation } from "../src/api/generated/SqlRelation";
import type { SqlResult } from "../src/api/generated/SqlResult";
import type { SqlRun } from "../src/api/generated/SqlRun";
import type { SqlSchema } from "../src/api/generated/SqlSchema";
import type { TraceDetail } from "../src/api/generated/TraceDetail";
import type { TraceListItem } from "../src/api/generated/TraceListItem";
import type { TraceListPage } from "../src/api/generated/TraceListPage";
import type { UserItem } from "../src/api/generated/UserItem";
import type { UserSessionItem } from "../src/api/generated/UserSessionItem";

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
    input_truncated: null,
    output_truncated: null,
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
    summary: null,
    spans,
    facts: { tags: [], bookmarked: false, public: null, scores: [], correction: null },
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
    facts: { tags: [], bookmarked: false, public: null, scores: [], correction: null },
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

export type Reply = Parameters<Route["fulfill"]>[0];

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
export const METRICS_PATH = new RegExp(`/projects/${PROJECT}/metrics/query$`);

// A metrics answer whose series hold these values, one bucket every five minutes.
export function metrics(
  series: { measure: string; aggregation?: string; values: (number | null)[] }[],
) {
  return {
    series: series.map((s) => ({
      measure: s.measure,
      aggregation: s.aggregation ?? "sum",
      unit: "count",
      key: [],
      other: false,
      points: s.values.map((value, i) => ({
        bucket: `2026-11-17T13:${String(i * 5).padStart(2, "0")}:00.000000Z`,
        value,
      })),
    })),
    granularity: "five_minutes",
    truncated: false,
    source_tier: "rollup",
    as_of: AS_OF,
    partial_last_bucket: false,
    saturated: false,
    meta: { as_of: AS_OF, partial: false, consistency: "strong" },
  };
}

// The measures a metrics request asks for, by name.
export function measuresOf(route: Route): string[] {
  const body = route.request().postDataJSON() as { measures: { measure: string }[] };
  return body.measures.map((m) => m.measure);
}
export const CHILDREN_PATH = new RegExp(`/projects/${PROJECT}/traces/${TRACE}/spans$`);
export const LIST_PATH = new RegExp(`/projects/${PROJECT}/traces$`);
export const PROVIDERS_PATH = /\/api\/v1\/auth\/providers$/;
export const SESSION_PATH = /\/api\/v1\/auth\/session$/;

// The shell's sign-in check: the request a page makes before any of its own.
export const SIGN_IN_CHECK = "/api/v1/auth/providers";

export const PROJECT_B = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a2c";
export const CSRF = "csrf-token-e2e";

export function signInOptions(fields: Partial<SignInOptions> = {}): SignInOptions {
  return { providers: [], password_login_enabled: true, setup_required: false, ...fields };
}

export function sessionInfo(fields: Partial<SessionInfo> = {}): SessionInfo {
  return {
    user: {
      id: "0193a1b2-0000-7000-8000-000000000001",
      email: "ada@example.com",
      name: "Ada Lovelace",
    },
    org: { id: "0193a1b2-0000-7000-8000-0000000000aa", name: "Analytical Engines", role: "owner" },
    projects: [
      {
        id: PROJECT,
        name: "Checkout",
        slug: "checkout",
        role: "admin",
        permissions: ["read", "write_facts", "edit_content", "manage"],
      },
      {
        id: PROJECT_B,
        name: "Search",
        slug: "search",
        role: "viewer",
        permissions: ["read", "run_saved_query"],
      },
    ],
    providers: [],
    password_login_enabled: true,
    csrf_token: CSRF,
    session: {
      expires_at: "2026-12-18T12:00:00.000000Z",
      idle_expires_at: "2026-11-25T12:00:00.000000Z",
      auth_method: "password",
    },
    features: {
      sql_workspace: { enabled: false },
      media: { enabled: false, max_bytes: 0 },
      smtp: false,
      signup_mode: "invite_only",
    },
    server: { version: "0.4.0" },
    ...fields,
  };
}

export const PROJECTS_PATH = /\/api\/v1\/projects$/;

// `GET projects` as the server answers it for this session.
export function projectList(session: SessionInfo = sessionInfo()) {
  return {
    data: session.projects.map(({ id, name, slug, role }) => ({
      id,
      name,
      slug,
      role,
      version: 1,
    })),
  };
}

// The server has a control plane, and this browser holds a session. Signed in, Crucible opens on home, whose
// tiles and charts read metrics: they get an empty answer unless a test sets its own. Settings opens on the
// project page, which reads the project list.
export function signedIn(api: Api, session: SessionInfo = sessionInfo()): void {
  api.on(PROVIDERS_PATH, json(signInOptions()));
  api.on(SESSION_PATH, json(session));
  api.on(/\/projects\/[^/]+\/metrics\/query$/, json(metrics([])));
  api.on(PROJECTS_PATH, json(projectList(session)));
}

// Answers `path` with `reply` and keeps each request, to check what the page sent.
export function capture(api: Api, path: RegExp, reply: Reply): Request[] {
  const requests: Request[] = [];
  api.on(path, (route) => {
    requests.push(route.request());
    return route.fulfill(reply);
  });
  return requests;
}

export const test = base.extend<{ api: Api }>({
  api: async ({ page }, use) => {
    const api = new Api();
    const pageErrors: string[] = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    // Unless a test signs in, the server has no control plane, so the Connect dialog applies.
    api.on(PROVIDERS_PATH, problem(404, "feature_disabled"));
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

export const SESSION = "chat-42";
export const USER = "ada";
export const SESSIONS_PATH = new RegExp(`/projects/${PROJECT}/sessions$`);
export const SESSION_DETAIL_PATH = new RegExp(`/projects/${PROJECT}/sessions/${SESSION}$`);
export const USERS_PATH = new RegExp(`/projects/${PROJECT}/users$`);
export const USER_PATH = new RegExp(`/projects/${PROJECT}/users/${USER}$`);
export const USER_SESSIONS_PATH = new RegExp(`/projects/${PROJECT}/users/${USER}/sessions$`);
export const SPANS_PATH = new RegExp(`/projects/${PROJECT}/spans$`);

const WHOLE = { as_of: AS_OF, partial: false, consistency: "strong" } as const;

export function listPage<T>(data: T[], next: string | null = null) {
  return { data, next_cursor: next, has_more: next !== null, meta: WHOLE };
}

export function sessionItem(fields: Partial<SessionItem> = {}): SessionItem {
  return {
    session_id: SESSION,
    first_seen: at(0),
    last_seen: at(4_200_000),
    trace_count: 3,
    span_count: 24,
    error_count: 1,
    total_cost_usd: 0.012,
    input_tokens: 3_600,
    output_tokens: 500,
    total_tokens: 4_100,
    user_ids: [USER],
    saturated: false,
    ...fields,
  };
}

export function userItem(fields: Partial<UserItem> = {}): UserItem {
  return {
    user_id: USER,
    first_seen: at(0),
    last_seen: at(4_200_000),
    trace_count: 5,
    session_count: 2,
    span_count: 40,
    error_count: 0,
    total_cost_usd: 0.03,
    input_tokens: 2_000,
    output_tokens: 500,
    total_tokens: 2_500,
    saturated: false,
    ...fields,
  };
}

export function spanListItem(fields: Partial<SpanListItem> = {}): SpanListItem {
  return {
    trace_id: TRACE,
    span_id: "a000000000000008",
    parent_span_id: "a000000000000001",
    name: "send_reply",
    observation_type: "tool",
    start_time: at(3_560_000),
    duration_us: 600_000,
    status_code: "error",
    level: "error",
    model: "",
    environment: "production",
    user_id: USER,
    session_id: SESSION,
    total_cost_usd: 0,
    total_tokens: 0,
    saturated: false,
    ...fields,
  };
}

export function userSessionItem(fields: Partial<UserSessionItem> = {}): UserSessionItem {
  return {
    session_id: SESSION,
    first_trace_at: at(0),
    last_trace_at: at(4_200_000),
    trace_count: 2,
    saturated: false,
    ...fields,
  };
}
