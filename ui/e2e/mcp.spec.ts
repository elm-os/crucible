import type { McpClientList } from "../src/api/generated/McpClientList";
import type { McpCompare } from "../src/api/generated/McpCompare";
import type { McpDeployList } from "../src/api/generated/McpDeployList";
import type { McpErrors } from "../src/api/generated/McpErrors";
import type { McpOperationDetail } from "../src/api/generated/McpOperationDetail";
import type { McpOperationPage } from "../src/api/generated/McpOperationPage";
import type { McpOperationRow } from "../src/api/generated/McpOperationRow";
import type { McpServerList } from "../src/api/generated/McpServerList";
import type { McpServerSource } from "../src/api/generated/McpServerSource";
import type { McpSessionList } from "../src/api/generated/McpSessionList";
import type { McpStats } from "../src/api/generated/McpStats";
import type { McpToolDetail } from "../src/api/generated/McpToolDetail";
import type { McpToolList } from "../src/api/generated/McpToolList";
import type { McpUserList } from "../src/api/generated/McpUserList";
import {
  AS_OF,
  expect,
  expectAccessible,
  json,
  PROJECT,
  problem,
  SERVER_TEXT,
  test,
} from "./fixtures";

const META = { as_of: AS_OF, partial: false, consistency: "strong" } as const;
const mcpPath = (rest: string) => new RegExp(`/projects/${PROJECT}/mcp/${rest}$`);
const SERVERS = mcpPath("servers");
const TOOLS = mcpPath("tools");
const TOOL = mcpPath("tools/read_file");
const ERRORS = mcpPath("errors");
const CLIENTS = mcpPath("clients");
const USERS = mcpPath("users");
const SESSIONS = mcpPath("sessions");
const OPERATIONS = mcpPath("operations");
const OPERATION = mcpPath("operations/[0-9a-f]{32}/[0-9a-f]{16}");
const DEPLOYS = mcpPath("deploys");
const COMPARE = mcpPath("compare");

const at = (minute: number) => `2026-11-17T13:${String(minute).padStart(2, "0")}:00.000000Z`;

function stats(fields: Partial<McpStats> = {}): McpStats {
  return {
    calls: 120,
    errors: 6,
    error_rate: 0.05,
    caller_faults: 2,
    latency: { p50_us: 12_000, p90_us: 40_000, p99_us: 250_000 },
    last_seen: at(59),
    ...fields,
  };
}

const SOURCES: McpServerSource[] = [
  { server_name: "files", available: ["sdk", "otlp"], chosen: "sdk" },
  { server_name: "db", available: ["otlp"], chosen: "otlp" },
];

function servers(fields: Partial<McpServerList> = {}): McpServerList {
  return {
    data: [
      { server_name: "files", stats: stats(), clients: 3, version: "1.4.0", deploys: 2 },
      { server_name: "db", stats: stats({ calls: 40 }), clients: 1, version: null, deploys: 0 },
    ],
    capture_sources: SOURCES,
    saturated: false,
    meta: META,
    ...fields,
  };
}

function operation(fields: Partial<McpOperationRow> = {}): McpOperationRow {
  return {
    trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
    span_id: "00f067aa0ba902b7",
    parent_span_id: null,
    start_time: at(58),
    duration_us: 12_000,
    server_name: "files",
    server_version: "1.4.0",
    deploy_id: "d2",
    capture_source: "sdk",
    era: "modern",
    transport: "stdio",
    method: "tools/call",
    target_kind: "tool",
    target_name: "read_file",
    status: "ok",
    error_class: "none",
    error_subclass: "none",
    caller_fault: false,
    jsonrpc_code: null,
    error_type: "",
    error_message: "",
    client_name: "claude-desktop",
    client_version: "0.9.1",
    user_id: "u-7",
    user_subject_hash: null,
    identity_source: "identify_hook",
    group_key: "session-1",
    group_key_kind: "handle",
    request_bytes: 100,
    response_bytes: 2048,
    content_captured: true,
    result_type: "complete",
    saturated: false,
    ...fields,
  };
}

const url = (page: string, params: Record<string, string> = {}) =>
  `/mcp/${page}/?${new URLSearchParams({ project: PROJECT, ...params })}`;

test.describe("mcp: servers", () => {
  test("loading: skeleton rows after 300 ms, announced to screen readers", async ({
    page,
    api,
  }) => {
    const release = api.hold(SERVERS, json(servers()));
    await page.goto(url("servers"));
    const section = page.getByRole("region", { name: "Servers" });
    await expect(section).toHaveAttribute("aria-busy", "true");
    await expect(section.getByRole("status")).toHaveText("Loading servers");
    await expectAccessible(page, "mcp servers loading");
    release();
    await expect(section.getByRole("table")).toBeVisible();
  });

  test("success: one row per server, the source read for each, links that keep the scope", async ({
    page,
    api,
  }) => {
    api.on(SERVERS, json(servers()));
    await page.goto(url("servers"));
    const table = page.getByRole("table");
    await expect(table.getByRole("columnheader")).toHaveText([
      "Server",
      "Version",
      "Calls",
      "Error rate",
      "Caller faults",
      "p50",
      "p90",
      "p99",
      "Clients",
      "Last seen",
    ]);
    await expect(table.locator("tbody tr")).toHaveCount(2);
    await expect(page.getByText("Read files from the SDK, db from OTLP.")).toBeVisible();
    const href = new URL(
      (await table.getByRole("link", { name: "files" }).getAttribute("href")) ?? "",
      "http://localhost",
    );
    expect(href.pathname).toMatch(/^\/mcp\/tools\/?$/);
    expect(Object.fromEntries(href.searchParams)).toEqual({ project: PROJECT, server: "files" });
    const call = api.calls.find((c) => SERVERS.test(c.split("?")[0] ?? ""));
    const request = new URL(`http://localhost${call}`);
    expect([...request.searchParams.keys()]).toEqual(["side", "from", "to"]);
    expect(request.searchParams.get("side")).toBe("server");
    await expect(page.getByRole("link", { name: "Servers" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    await expectAccessible(page, "mcp servers success");
  });

  test("scope: side, source and caller faults go to the URL and into the request", async ({
    page,
    api,
  }) => {
    api.on(SERVERS, json(servers()));
    await page.goto(url("servers"));
    await expect(page.getByRole("link", { name: "files" })).toBeVisible();

    await page.getByRole("radio", { name: "Client side" }).click();
    await expect(page).toHaveURL(/side=client/);
    await page.getByRole("switch", { name: "Include caller faults" }).click();
    await expect(page).toHaveURL(/faults=1/);
    await page.getByRole("combobox", { name: "Capture source" }).click();
    await page.getByRole("option", { name: "Proxy" }).click();
    await expect(page).toHaveURL(/source=proxy/);

    await expect
      .poll(() => {
        const last = api.calls.filter((c) => SERVERS.test(c.split("?")[0] ?? "")).at(-1) ?? "";
        return Object.fromEntries(new URL(`http://localhost${last}`).searchParams);
      })
      .toMatchObject({ side: "client", source: "proxy", include_caller_faults: "true" });
    await expectAccessible(page, "mcp scope controls");
  });

  test("error: a failed list is an in-section error whose retry refetches only the list", async ({
    page,
    api,
  }) => {
    api.on(SERVERS, problem(503, "telemetry_store_unavailable", "req-mcp"));
    await page.goto(url("servers"));
    const section = page.getByRole("region", { name: "Servers" });
    await expect(section.getByText("This section couldn't load.")).toBeVisible({ timeout: 10_000 });
    await expect(section.getByText("req-mcp")).toBeVisible();
    await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
    await expectAccessible(page, "mcp servers error");

    api.on(SERVERS, json(servers()));
    await section.getByRole("button", { name: "Retry servers" }).click();
    await expect(section.getByRole("table")).toBeVisible();
  });

  test("empty: an empty window offers a wider range; the widest invites the first server", async ({
    page,
    api,
  }) => {
    api.on(SERVERS, json(servers({ data: [], capture_sources: [] })));
    await page.goto(url("servers"));
    await expect(page.getByText("No MCP servers in the last 24 hours.")).toBeVisible();
    await expectAccessible(page, "mcp servers empty");
    await page.getByRole("button", { name: "Show last 30 days" }).click();
    await expect(page.getByText("No MCP servers in the last 30 days.")).toBeVisible();
    await expect(page.getByText(/Wrap your MCP server with a Crucible SDK/)).toBeVisible();
  });
});

const TOOL_LIST: McpToolList = {
  data: [
    {
      server_name: "files",
      tool_name: "read_file",
      stats: stats(),
      request_bytes: 100,
      response_bytes: 2048,
      schema_hash: "0123456789abcdef",
      unused: false,
    },
    {
      server_name: "files",
      tool_name: "delete_file",
      stats: stats({ calls: 0, errors: 0, error_rate: null, caller_faults: 0 }),
      request_bytes: 0,
      response_bytes: 0,
      schema_hash: "fedcba9876543210",
      unused: true,
    },
  ],
  capture_sources: SOURCES,
  saturated: false,
  meta: META,
};

function toolDetail(fields: Partial<McpToolDetail> = {}): McpToolDetail {
  return {
    tool_name: "read_file",
    stats: stats(),
    request_bytes: 100,
    response_bytes: 2048,
    granularity: "minute",
    series: [0, 1, 2, 3].map((minute) => ({
      bucket: at(minute),
      calls: 30 + minute,
      errors: minute,
      latency: { p50_us: 10_000, p90_us: 40_000 + minute, p99_us: 250_000 },
    })),
    error_classes: [
      { error_class: "tool_execution", calls: 4 },
      { error_class: "timeout", calls: 2 },
    ],
    clients: [{ client_name: "claude-desktop", stats: stats() }],
    deploys: [
      {
        server_name: "files",
        deploy_id: "d2",
        server_version: "1.4.0",
        capture_source: "sdk",
        first_seen: at(2),
        last_seen: at(3),
      },
    ],
    capture_sources: SOURCES.slice(0, 1),
    saturated: false,
    meta: META,
    ...fields,
  };
}

test.describe("mcp: tools", () => {
  test("success: called tools open their detail; the unused tab lists schema hashes", async ({
    page,
    api,
  }) => {
    api.on(TOOLS, json(TOOL_LIST));
    api.on(TOOL, json(toolDetail()));
    await page.goto(url("tools"));
    await page.getByRole("tab", { name: "Unused (1)" }).click();
    await expect(page.getByText("fedcba9876543210")).toBeVisible();
    await expectAccessible(page, "mcp tools unused");
    await page.getByRole("tab", { name: "Called (1)" }).click();
    await page.getByRole("link", { name: "read_file" }).click();
    await expect(page).toHaveURL(/\/mcp\/tool\/\?.*name=read_file/);
    await expect(page.getByRole("region", { name: "Latency percentiles" })).toBeVisible();
  });

  test("tool detail: percentiles in one ink family with deploy markers, and a table view", async ({
    page,
    api,
  }) => {
    api.on(TOOL, json(toolDetail()));
    await page.goto(url("tool", { name: "read_file" }));
    const latency = page.getByRole("region", { name: "Latency percentiles" });
    await expect(latency.getByText("Deploy d2 of files", { exact: false })).toBeVisible();
    await expect(latency.locator("[data-marker-rule]")).toHaveCount(1);
    await expect(latency.locator("[data-marker]")).toHaveCount(3);
    await latency.getByRole("button", { name: "View as table: latency percentiles" }).click();
    await expect(latency.getByRole("table").locator("tbody tr")).toHaveCount(4);
    await expect(
      page.getByRole("region", { name: "Error classes" }).getByText("Tool failed"),
    ).toBeVisible();
    await expect(page.getByText("Client names are self-reported.")).toBeVisible();
    await expectAccessible(page, "mcp tool detail");
  });

  test("tool detail: a tool without calls in the range offers a wider one", async ({
    page,
    api,
  }) => {
    api.on(
      TOOL,
      json(
        toolDetail({
          stats: stats({ calls: 0, errors: 0, error_rate: null, caller_faults: 0 }),
          series: [],
          error_classes: [],
          clients: [],
          deploys: [],
        }),
      ),
    );
    await page.goto(url("tool", { name: "read_file" }));
    await expect(
      page
        .getByRole("region", { name: "Summary" })
        .getByText("No MCP calls of this tool in the last 24 hours."),
    ).toBeVisible();
    await expectAccessible(page, "mcp tool empty");
  });
});

const ERRORS_SUMMARY: McpErrors = {
  view: "summary",
  stats: stats(),
  classes: [{ error_class: "validation", calls: 6 }],
  targets: [
    { method: "tools/call", target_name: "read_file", error_class: "validation", calls: 6 },
  ],
  breakdown: [],
  truncated: false,
  capture_sources: SOURCES,
  saturated: false,
  meta: META,
};

const CLIENT_LIST: McpClientList = {
  view: "summary",
  self_reported: true,
  data: [{ client_name: "claude-desktop", stats: stats() }],
  versions: [],
  truncated: false,
  capture_sources: SOURCES,
  saturated: false,
  meta: META,
};

const USER_LIST: McpUserList = {
  data: [
    {
      user_id: "u-7",
      subject_hash: null,
      identity_source: "identify_hook",
      calls: 12,
      errors: 1,
      sessions: 2,
      first_seen: at(10),
      last_seen: at(50),
    },
  ],
  truncated: false,
  capture_sources: SOURCES,
  saturated: false,
  meta: META,
};

const SESSION_LIST: McpSessionList = {
  data: [
    {
      group_key: "session-1",
      group_key_kind: "handle",
      calls: 12,
      errors: 1,
      client_name: "claude-desktop",
      first_seen: at(10),
      last_seen: at(50),
    },
  ],
  truncated: true,
  capture_sources: SOURCES,
  saturated: false,
  meta: META,
};

const OPERATION_PAGE: McpOperationPage = {
  data: [operation()],
  next_cursor: "c-2",
  has_more: true,
  capture_sources: SOURCES,
  meta: META,
};

const DEPLOY_LIST: McpDeployList = {
  data: [
    {
      server_name: "files",
      deploy_id: "d2",
      server_version: "1.4.0",
      capture_source: "sdk",
      first_seen: at(30),
      last_seen: at(59),
    },
    {
      server_name: "files",
      deploy_id: "d1",
      server_version: "1.3.0",
      capture_source: "sdk",
      first_seen: at(0),
      last_seen: at(29),
    },
  ],
  capture_sources: SOURCES.slice(0, 1),
  meta: META,
};

test.describe("mcp: errors, clients, users and sessions", () => {
  test("errors: the summary by class and target; the breakdown by message", async ({
    page,
    api,
  }) => {
    const summary = ERRORS_SUMMARY;
    const breakdown: McpErrors = {
      ...summary,
      view: "breakdown",
      stats: null,
      classes: [],
      targets: [],
      breakdown: [
        {
          server_name: "files",
          method: "tools/call",
          target_name: "read_file",
          error_class: "validation",
          error_subclass: "none",
          jsonrpc_code: -32602,
          error_message: "path must be absolute",
          calls: 6,
          last_seen: at(57),
        },
      ],
    };
    api.on(ERRORS, (route, request) =>
      route.fulfill(json(request.searchParams.get("view") === "breakdown" ? breakdown : summary)),
    );
    await page.goto(url("errors"));
    await expect(
      page.getByText("6 of 120 calls failed (5.0%). 2 more were caller faults."),
    ).toBeVisible();
    await expect(page.getByRole("list", { name: "Calls per error class" })).toContainText(
      "Invalid arguments",
    );
    await expectAccessible(page, "mcp errors summary");
    await page.getByRole("radio", { name: "By message" }).click();
    await expect(page.getByText("path must be absolute")).toBeVisible();
    await expect(page).toHaveURL(/view=breakdown/);
    await expectAccessible(page, "mcp errors breakdown");
  });

  test("clients: names are labelled self-reported", async ({ page, api }) => {
    api.on(CLIENTS, json(CLIENT_LIST));
    await page.goto(url("clients"));
    await expect(page.getByText(/^Self-reported:/)).toBeVisible();
    await expect(page.getByRole("link", { name: "claude-desktop" })).toBeVisible();
    await expectAccessible(page, "mcp clients");
  });

  test("users and sessions: rows link to their operations", async ({ page, api }) => {
    api.on(USERS, json(USER_LIST));
    api.on(SESSIONS, json(SESSION_LIST));
    await page.goto(url("users"));
    const user = new URL(
      (await page.getByRole("link", { name: "u-7" }).getAttribute("href")) ?? "",
      "http://localhost",
    );
    expect(user.searchParams.get("user")).toBe("u-7");
    await expect(page.getByRole("cell", { name: "Identify hook" })).toBeVisible();
    await expectAccessible(page, "mcp users");
    await page
      .getByRole("navigation", { name: "MCP pages" })
      .getByRole("link", { name: "Sessions" })
      .click();
    await expect(page.getByRole("link", { name: "session-1" })).toBeVisible();
    await expect(page.getByText(/Showing the 1 sessions seen most recently/)).toBeVisible();
    await expectAccessible(page, "mcp sessions");
  });

  test("users: empty at the widest range explains where users come from", async ({ page, api }) => {
    api.on(
      USERS,
      json({ data: [], truncated: false, capture_sources: [], saturated: false, meta: META }),
    );
    await page.goto(url("users", { range: "30d" }));
    await expect(page.getByText("No MCP users in the last 30 days.")).toBeVisible();
    await expect(page.getByText(/identify hook names them/)).toBeVisible();
  });
});

test.describe("mcp: operations, deploys and compare", () => {
  test("operations: load more by cursor, then an operation's detail with its content", async ({
    page,
    api,
  }) => {
    const first = OPERATION_PAGE;
    const second: McpOperationPage = {
      ...first,
      data: [
        operation({
          span_id: "11f067aa0ba902b7",
          start_time: at(57),
          status: "tool_error",
          error_class: "validation",
          caller_fault: true,
        }),
      ],
      next_cursor: null,
      has_more: false,
    };
    api.on(OPERATIONS, (route, request) =>
      route.fulfill(json(request.searchParams.get("cursor") === "c-2" ? second : first)),
    );
    const detail: McpOperationDetail = {
      operation: {
        operation: operation(),
        side: "server",
        instance_id: "i-1",
        protocol_version: "2025-11-25",
        jsonrpc_id: "7",
        http_status: null,
        oauth_client_id: "",
        tenant: "",
        args: { preview: '{"path":"/etc/hosts"}', bytes: 21, truncated: false },
        result: { preview: "127.0.0.1 localhost", bytes: 19, truncated: false },
        other_side: null,
        chain: [],
      },
      meta: META,
    };
    api.on(OPERATION, json(detail));
    await page.goto(url("operations"));
    await page.getByRole("button", { name: "Load more operations" }).click();
    await expect(page.getByText("Invalid arguments (caller)")).toBeVisible();
    await expect(page.locator("tbody tr")).toHaveCount(2);
    await expectAccessible(page, "mcp operations");

    await page.locator("tbody tr").first().getByRole("link").click();
    await expect(page).toHaveURL(/\/mcp\/operation\/\?.*span=00f067aa0ba902b7/);
    await expect(page.getByText('{"path":"/etc/hosts"}')).toBeVisible();
    await expect(page.getByText("The other side of this call wasn't captured.")).toBeVisible();
    await expectAccessible(page, "mcp operation detail");
  });

  test("deploys: Compare sets a deploy against the one before it", async ({ page, api }) => {
    const compare: McpCompare = {
      server_name: "files",
      tools: [
        {
          tool_name: "read_file",
          baseline: stats({ calls: 100 }),
          candidate: stats({ calls: 150 }),
        },
      ],
      added: ["move_file"],
      removed: [],
      changed: ["read_file"],
      capture_sources: SOURCES.slice(0, 1),
      saturated: false,
      meta: META,
    };
    api.on(DEPLOYS, json(DEPLOY_LIST));
    api.on(COMPARE, json(compare));
    await page.goto(url("deploys"));
    await expect(page.getByText("First in range")).toBeVisible();
    await expectAccessible(page, "mcp deploys");
    await page.getByRole("link", { name: "Compare with d1" }).click();
    await expect(page).toHaveURL(/\/mcp\/compare\/\?.*baseline=d1.*candidate=d2/);
    await expect(page.getByText("move_file")).toBeVisible();
    await expect(page.locator("tbody tr").first()).toContainText("100 to 150");
    const call = api.calls.find((c) => COMPARE.test(c.split("?")[0] ?? ""));
    const request = new URL(`http://localhost${call}`);
    expect(request.searchParams.get("server")).toBe("files");
    expect(request.searchParams.get("baseline_deploy")).toBe("d1");
    expect(request.searchParams.get("candidate_deploy")).toBe("d2");
    await expectAccessible(page, "mcp compare");
  });

  test("compare: without two deploys it says how to pick them and requests nothing", async ({
    page,
    api,
  }) => {
    await page.goto(url("compare"));
    await expect(page.getByText("Pick two deploys to compare.")).toBeVisible();
    expect(api.count(COMPARE)).toBe(0);
    await expectAccessible(page, "mcp compare idle");
  });
});

// Every list page's loading, error and empty states, beside the success checks above.
const LISTS = [
  { page: "tools", path: TOOLS, title: "Tools", noun: "tools", body: TOOL_LIST },
  { page: "errors", path: ERRORS, title: "Error classes", noun: "errors", body: ERRORS_SUMMARY },
  { page: "clients", path: CLIENTS, title: "Clients", noun: "clients", body: CLIENT_LIST },
  { page: "users", path: USERS, title: "Users", noun: "users", body: USER_LIST },
  { page: "sessions", path: SESSIONS, title: "Sessions", noun: "sessions", body: SESSION_LIST },
  {
    page: "operations",
    path: OPERATIONS,
    title: "Operations",
    noun: "operations",
    body: OPERATION_PAGE,
  },
  { page: "deploys", path: DEPLOYS, title: "Deploys", noun: "deploys", body: DEPLOY_LIST },
] as const;

function emptied(body: (typeof LISTS)[number]["body"]): unknown {
  if ("classes" in body) return { ...body, stats: stats({ errors: 0 }), classes: [], targets: [] };
  if ("versions" in body) return { ...body, data: [], versions: [] };
  if ("has_more" in body) return { ...body, data: [], has_more: false, next_cursor: null };
  return { ...body, data: [] };
}

for (const list of LISTS) {
  test.describe(`mcp: ${list.page} states`, () => {
    test("loading, then an error whose retry refetches only the section", async ({ page, api }) => {
      const release = api.hold(list.path, problem(503, "telemetry_store_unavailable", "req-list"));
      await page.goto(url(list.page));
      const section = page.getByRole("region", { name: list.title });
      await expect(section).toHaveAttribute("aria-busy", "true");
      await expect(section.getByRole("status")).toHaveText(`Loading ${list.title.toLowerCase()}`);
      await expectAccessible(page, `mcp ${list.page} loading`);
      api.on(list.path, problem(503, "telemetry_store_unavailable", "req-list"));
      release();
      await expect(section.getByText("This section couldn't load.")).toBeVisible({
        timeout: 10_000,
      });
      await expect(section.getByText("req-list")).toBeVisible();
      await expect(page.getByText(SERVER_TEXT)).toHaveCount(0);
      await expectAccessible(page, `mcp ${list.page} error`);

      api.on(list.path, json(list.body));
      await section.getByRole("button", { name: `Retry ${list.title.toLowerCase()}` }).click();
      await expect(section.getByRole("table").first()).toBeVisible();
    });

    test("empty: a whole empty answer offers the widest range", async ({ page, api }) => {
      api.on(list.path, json(emptied(list.body)));
      await page.goto(url(list.page));
      await expect(page.getByText(`No MCP ${list.noun} in the last 24 hours.`)).toBeVisible();
      await expectAccessible(page, `mcp ${list.page} empty`);
      await page.getByRole("button", { name: "Show last 30 days" }).click();
      await expect(page.getByText(`No MCP ${list.noun} in the last 30 days.`)).toBeVisible();
    });
  });
}
