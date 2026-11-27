import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { ApiKey } from "@/api/generated/ApiKey";
import type { DataBounds } from "@/api/generated/DataBounds";
import type { Meta } from "@/api/generated/Meta";
import type { MetricsResponse } from "@/api/generated/MetricsResponse";
import type { MetricsSeries } from "@/api/generated/MetricsSeries";
import type { OrgMember } from "@/api/generated/OrgMember";
import type { TimeRange } from "@/api/generated/TimeRange";
import { createQueryClient } from "@/components/providers";
import { setAuth } from "@/lib/auth";
import { PROJECT_A, sessionInfo } from "@/test/session";
import { HomeOverview } from "./home-overview";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/home/?project=p-1");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": status === 200 ? "application/json" : "application/problem+json" },
  });

function series(measure: string, aggregation: string, values: (number | null)[]): MetricsSeries {
  return {
    measure,
    aggregation,
    unit: measure === "total_cost" ? "usd" : "count",
    key: [],
    other: false,
    points: values.map((value, i) => ({
      bucket: `2026-11-17T09:${String(i * 5).padStart(2, "0")}:00.000000Z`,
      value,
    })),
  };
}

function response(list: MetricsSeries[], partial = false): MetricsResponse {
  return {
    series: list,
    granularity: "five_minutes",
    truncated: false,
    source_tier: "rollup",
    as_of: "2026-11-17T09:30:00.000000Z",
    partial_last_bucket: false,
    saturated: false,
    meta: { as_of: "2026-11-17T09:30:00.000000Z", partial, consistency: "strong" },
  };
}

const WHOLE: Meta = { as_of: "2026-11-17T09:30:00.000000Z", partial: false, consistency: "strong" };
const SEEN: TimeRange = {
  first: "2026-11-17T09:00:00.000000Z",
  last: "2026-11-17T09:25:00.000000Z",
};
const NONE: TimeRange = { first: null, last: null };

function bounds(
  fields: { spans?: TimeRange; client?: TimeRange; server?: TimeRange; meta?: Meta } = {},
): DataBounds {
  return {
    spans: fields.spans ?? SEEN,
    mcp: { client: fields.client ?? NONE, server: fields.server ?? SEEN },
    retention_horizon: null,
    max_window_days: 30,
    server_time: "2026-11-17T09:30:00.000000Z",
    meta: fields.meta ?? WHOLE,
  };
}

// A project nobody has sent anything to.
const EMPTY = bounds({ spans: NONE, server: NONE });

function apiKey(fields: Partial<ApiKey> = {}): ApiKey {
  return {
    id: "k-1",
    public_key: `cpk_${"a".repeat(43)}`,
    scopes: ["ingest", "read", "write"],
    note: "checkout-service",
    expires_at: null,
    last_used_at: null,
    revoked_at: null,
    version: 1,
    ...fields,
  };
}

function member(fields: Partial<OrgMember> = {}): OrgMember {
  return {
    user_id: sessionInfo().user.id,
    email: "ada@example.com",
    name: "Ada Lovelace",
    role: "owner",
    status: "active",
    ...fields,
  };
}

const TEAM = [member(), member({ user_id: "u-2", email: "grace@example.com", role: "member" })];

type Answer = () => Response | Promise<Response>;

interface Server {
  metrics?: (measures: string[]) => Response;
  bounds?: Answer;
  keys?: Answer;
  members?: Answer;
}

const NEVER: Answer = () => new Promise<Response>(() => {});

const path = (url: unknown) => new URL(String(url), "http://localhost").pathname;

// Answers each request by its path, and each metrics query by the measures it asks for. A project with
// data unless a test says otherwise.
function serve(server: Server) {
  fetchMock.mockImplementation(async (url, init) => {
    const at = path(url);
    if (at.endsWith("/metrics/query")) {
      const request = JSON.parse(String(init?.body)) as { measures: { measure: string }[] };
      const measures = request.measures.map((m) => m.measure);
      return server.metrics
        ? server.metrics(measures)
        : json(response(measures.map((m) => series(m, m === "duration" ? "p50" : "sum", [1]))));
    }
    if (at.endsWith("/data-bounds")) return server.bounds ? server.bounds() : json(bounds());
    if (at.endsWith("/api-keys")) return server.keys ? server.keys() : json({ data: [apiKey()] });
    if (at === "/api/v1/org/members") {
      return server.members ? server.members() : json({ data: TEAM });
    }
    throw new Error(`no answer for ${at}`);
  });
}

function answer(metrics: (measures: string[]) => Response) {
  serve({ metrics });
}

const requested = (suffix: string) =>
  fetchMock.mock.calls.filter(([url]) => path(url).endsWith(suffix)).length;

function signIn(fields: Parameters<typeof sessionInfo>[0] = {}) {
  setAuth({ mode: "session", session: sessionInfo(fields) });
  window.history.replaceState(null, "", `/home/?project=${PROJECT_A}`);
}

// The row of one set-up step.
function step(title: string): HTMLElement {
  const row = within(section("Set up"))
    .getAllByRole("listitem")
    .find((item) => within(item).queryByText(title) !== null);
  if (!row) throw new Error(`no step "${title}"`);
  return row;
}

async function expectStep(title: string, status: string) {
  await waitFor(() => expect(within(step(title)).getByText(status)).toBeTruthy());
}

function renderHome() {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <HomeOverview />
    </QueryClientProvider>,
  );
}

const section = (name: string) => screen.getByRole("region", { name });

const problem = (status: number, code: string) =>
  new Response(JSON.stringify({ code, title: "raw server title", request_id: "req-1" }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });

const setupRegion = () => screen.queryByRole("region", { name: "Set up" });
const setupLink = (name: string) => within(section("Set up")).getByRole("link", { name });
const setupHref = (name: string) =>
  new URL(setupLink(name).getAttribute("href") ?? "", "http://localhost");
const precedes = (a: HTMLElement, b: HTMLElement) =>
  (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;

// Waits until the tiles have their answers and every set-up read was asked, then lets those answers land.
async function settled(reads: string[]) {
  expect(await within(section("Traces")).findByText("1")).toBeTruthy();
  await waitFor(() => {
    for (const read of reads) expect(requested(read)).toBe(1);
  });
  await act(() => new Promise((resolve) => setTimeout(resolve, 20)));
}

describe("the home tiles and charts", () => {
  test("each tile shows its headline from its own query", async () => {
    answer((measures) => {
      const [first] = measures;
      if (first === "traces") return json(response([series("traces", "sum", [2, 3])]));
      if (first === "total_cost")
        return json(
          response([series("total_cost", "sum", [0.5, 1]), series("count", "sum", [1, 1])]),
        );
      if (first === "total_tokens")
        return json(
          response([series("total_tokens", "sum", [1000, 500]), series("count", "sum", [1, 1])]),
        );
      if (first === "count")
        return json(
          response([series("count", "sum", [8, 2]), series("error_count", "sum", [1, 0])]),
        );
      return json(
        response([
          series("duration", "p50", [1500, null]),
          series("duration", "p95", [9000, null]),
        ]),
      );
    });
    renderHome();
    expect(await within(section("Traces")).findByText("5")).toBeTruthy();
    expect(await within(section("Cost")).findByText("$1.50")).toBeTruthy();
    expect(await within(section("Tokens")).findByText("1,500")).toBeTruthy();
    expect(await within(section("Error rate")).findByText("10%")).toBeTruthy();
    expect(within(section("Error rate")).getByText("1 of 10 spans")).toBeTruthy();
    expect(await within(section("Latency")).findByText("2 ms p50")).toBeTruthy();
    // The charts share the tiles' answers: five queries, no more.
    expect(requested("/metrics/query")).toBe(5);
    const latency = section("Latency percentiles");
    expect(within(latency).getByText("p50")).toBeTruthy();
    expect(within(latency).getByText("p95")).toBeTruthy();
    // Every chart has a table view.
    const toggle = within(latency).getByRole("button", { name: /^View as table/ });
    await act(async () => toggle.click());
    expect(within(latency).getByRole("table")).toBeTruthy();
  });

  test("one failing query leaves the other tiles standing, and its retry refetches only it", async () => {
    let failCost = true;
    answer((measures) => {
      if (measures[0] === "total_cost" && failCost) return json({ code: "internal_error" }, 500);
      return json(response(measures.map((m) => series(m, m === "duration" ? "p50" : "sum", [1]))));
    });
    renderHome();
    const cost = section("Cost");
    expect(await within(cost).findByText("This section couldn't load.")).toBeTruthy();
    expect(await within(section("Tokens")).findByText("1")).toBeTruthy();
    failCost = false;
    const calls = requested("/metrics/query");
    await act(async () => within(cost).getByRole("button", { name: "Retry cost" }).click());
    expect(await within(cost).findByText("$1.00")).toBeTruthy();
    expect(requested("/metrics/query")).toBe(calls + 1);
  });

  test("no data is empty only when the answer is whole", async () => {
    answer((measures) =>
      json(
        response(
          measures.map((m) => series(m, "sum", [0, 0])),
          measures[0] === "traces",
        ),
      ),
    );
    renderHome();
    expect(await within(section("Cost")).findByText("No data in the last 24 hours.")).toBeTruthy();
    expect(
      within(section("Cost")).getByRole("button", { name: "Show last 30 days: cost" }),
    ).toBeTruthy();
    expect(await within(section("Traces")).findByText("No data to show yet.")).toBeTruthy();
  });

  test("a tile holds its place while loading", () => {
    fetchMock.mockImplementation(() => new Promise(() => {}));
    renderHome();
    expect(section("Traces").getAttribute("aria-busy")).toBe("true");
  });

  test("an agent finds every control by name and can open a chart's table", async () => {
    answer((measures) =>
      json(response(measures.map((m) => series(m, m === "duration" ? "p50" : "sum", [1, 2])))),
    );
    renderHome();
    const agent = createAgent();
    await within(section("Latency percentiles")).findByRole("slider");
    const components = agent
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more");
    for (const entry of components) {
      expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
    }
    const labels = components.map((entry) => entry.label);
    expect(new Set(labels).size).toBe(labels.length);
    const result = await agent.act({
      steps: [{ ref: "View as table: span latency percentiles per bucket", action: "toggle" }],
    });
    expect(result.ok, result.error?.message).toBe(true);
    expect(within(section("Latency percentiles")).getByRole("table")).toBeTruthy();
  });
});

describe("the set-up steps", () => {
  test("each step reads the project's state: a key, a first trace, MCP data and a team", async () => {
    signIn();
    serve({
      bounds: () => json(bounds({ server: NONE })),
      keys: () => json({ data: [apiKey({ revoked_at: "2026-11-01T00:00:00.000000Z" }), apiKey()] }),
      members: () => json({ data: TEAM }),
    });
    renderHome();
    await expectStep("Create an API key", "Done");
    await expectStep("Send your first trace", "Done");
    await expectStep("Instrument an MCP server", "To do");
    await expectStep("Invite your team", "Done");
  });

  test("a revoked or expired key, a lone owner and no data leave their steps to do", async () => {
    signIn();
    serve({
      bounds: () => json(EMPTY),
      keys: () =>
        json({
          data: [
            apiKey({ revoked_at: "2026-11-01T00:00:00.000000Z" }),
            apiKey({ id: "k-2", expires_at: "2026-11-01T00:00:00.000000Z" }),
          ],
        }),
      members: () => json({ data: [member()] }),
    });
    renderHome();
    for (const title of [
      "Create an API key",
      "Send your first trace",
      "Instrument an MCP server",
      "Invite your team",
    ]) {
      await expectStep(title, "To do");
    }
  });

  test("MCP operations recorded on either side count as MCP data", async () => {
    signIn();
    serve({
      bounds: () => json(bounds({ server: NONE, client: SEEN })),
      keys: () => json({ data: [] }),
    });
    renderHome();
    await expectStep("Instrument an MCP server", "Done");
  });

  test("a project member never sees the key step stuck at to do, and no keys are asked for", async () => {
    signIn({
      org: { ...sessionInfo().org, role: "member" },
      projects: [
        {
          id: PROJECT_A,
          name: "Checkout",
          slug: "checkout",
          role: "member",
          permissions: ["read", "write_facts", "edit_content"],
        },
      ],
    });
    serve({ bounds: () => json(EMPTY) });
    renderHome();
    await waitFor(() =>
      expect(
        within(step("Create an API key")).getByText(
          "Only project admins create API keys. Ask an admin of Checkout for one.",
        ),
      ).toBeTruthy(),
    );
    expect(within(step("Create an API key")).queryByText("To do")).toBeNull();
    await expectStep("Invite your team", "Done");
    expect(requested("/api-keys")).toBe(0);
  });

  test("a members list the role can't read says who invites people", async () => {
    signIn({ org: { ...sessionInfo().org, role: "viewer" } });
    serve({
      bounds: () => json(EMPTY),
      members: () => problem(403, "forbidden"),
    });
    renderHome();
    await waitFor(() =>
      expect(
        within(step("Invite your team")).getByText(
          "Owners and admins of Analytical Engines invite people.",
        ),
      ).toBeTruthy(),
    );
    expect(within(step("Invite your team")).queryByText("To do")).toBeNull();
  });

  test("without a control plane, keys come from the keys file and nobody is invited", async () => {
    serve({ bounds: () => json(EMPTY) });
    renderHome();
    await waitFor(() =>
      expect(
        within(step("Create an API key")).getByText(
          "This server reads its keys from the keys file.",
        ),
      ).toBeTruthy(),
    );
    expect(
      within(step("Invite your team")).getByText(
        "This server runs without a control plane, so it has no sign-in or members. Set postgres.url (CRUCIBLE_POSTGRES_URL) to turn them on.",
      ),
    ).toBeTruthy();
    await expectStep("Send your first trace", "To do");
    expect(requested("/api-keys") + requested("/org/members")).toBe(0);
  });
});

describe("the set-up links", () => {
  const TRACES_GUIDE = "https://github.com/elm-os/crucible/blob/main/docs/guides/sending-traces.md";
  const MCP_GUIDE = "https://github.com/elm-os/crucible/blob/main/docs/guides/mcp.md";

  test("each step to do links to where it is done", async () => {
    signIn();
    serve({
      bounds: () => json(EMPTY),
      keys: () => json({ data: [] }),
      members: () => json({ data: [member()] }),
    });
    renderHome();
    await expectStep("Create an API key", "To do");
    await expectStep("Invite your team", "To do");
    expect(setupHref("API keys").pathname).toMatch(/^\/settings\/api-keys\/?$/);
    expect(setupHref("API keys").searchParams.get("project")).toBe(PROJECT_A);
    expect(setupHref("Members").pathname).toMatch(/^\/settings\/members\/?$/);
    expect(setupHref("Members").searchParams.get("project")).toBe(PROJECT_A);
    // The guides open beside Crucible.
    for (const [name, guide] of [
      ["How to send traces", TRACES_GUIDE],
      ["How to instrument an MCP server", MCP_GUIDE],
    ] as const) {
      expect(setupLink(name).getAttribute("href")).toBe(guide);
      expect(setupLink(name).getAttribute("target")).toBe("_blank");
      expect(setupLink(name).getAttribute("rel")).toBe("noreferrer");
    }
  });

  test("a done step drops its how-to", async () => {
    signIn();
    serve({
      bounds: () => json(bounds({ server: NONE })),
      keys: () => json({ data: [apiKey()] }),
      members: () => json({ data: TEAM }),
    });
    renderHome();
    await expectStep("Send your first trace", "Done");
    await expectStep("Invite your team", "Done");
    await expectStep("Instrument an MCP server", "To do");
    expect(
      within(section("Set up")).queryByRole("link", { name: "How to send traces" }),
    ).toBeNull();
    expect(within(section("Set up")).queryByRole("link", { name: "Members" })).toBeNull();
    expect(setupLink("How to instrument an MCP server").getAttribute("href")).toBe(MCP_GUIDE);
  });
});

describe("where the set-up card sits", () => {
  test("a project with no data leads with it, above the empty tiles", async () => {
    serve({ bounds: () => json(EMPTY) });
    renderHome();
    await expectStep("Send your first trace", "To do");
    expect(precedes(section("Set up"), section("Traces"))).toBe(true);
  });

  test("steps left before the data bounds answer wait for them, so the card never moves", async () => {
    signIn();
    const { promise: held, resolve: land } = Promise.withResolvers<Response>();
    serve({
      bounds: () => held,
      keys: () => json({ data: [] }),
      members: () => json({ data: [member()] }),
    });
    renderHome();
    await settled(["/data-bounds", "/api-keys", "/org/members"]);
    expect(setupRegion()).toBeNull();
    await act(async () => land(json(EMPTY)));
    await expectStep("Send your first trace", "To do");
    await expectStep("Create an API key", "To do");
    expect(precedes(section("Set up"), section("Traces"))).toBe(true);
  });

  test("a project with data keeps it below the charts while a step is left", async () => {
    signIn();
    serve({ keys: () => json({ data: [] }) });
    renderHome();
    await expectStep("Create an API key", "To do");
    expect(precedes(section("Latency percentiles"), section("Set up"))).toBe(true);
  });

  test("once every step is done it is gone", async () => {
    signIn();
    serve({});
    renderHome();
    await settled(["/data-bounds", "/api-keys", "/org/members"]);
    expect(setupRegion()).toBeNull();
  });

  test("steps nobody here can do don't hold it open", async () => {
    serve({});
    renderHome();
    await settled(["/data-bounds"]);
    expect(setupRegion()).toBeNull();
  });

  test("nothing shows while the steps are checked, so a set-up project never flashes it", () => {
    fetchMock.mockImplementation(() => new Promise(() => {}));
    renderHome();
    expect(section("Traces").getAttribute("aria-busy")).toBe("true");
    expect(setupRegion()).toBeNull();
  });
});

describe("the set-up card's states", () => {
  test("loading: rows still being checked hold their place while the card is busy", async () => {
    signIn();
    serve({
      bounds: () => json(EMPTY),
      keys: NEVER,
      members: NEVER,
    });
    renderHome();
    await expectStep("Send your first trace", "To do");
    const setup = section("Set up");
    expect(setup.getAttribute("aria-busy")).toBe("true");
    expect(within(step("Create an API key")).queryByText("To do")).toBeNull();
    expect(within(step("Create an API key")).queryByText("Done")).toBeNull();
    expect(await within(setup).findByText("Checking the set-up steps")).toBeTruthy();
  });

  test("error: a failed read says so in its rows, and Check again asks only it again", async () => {
    let failing = true;
    serve({ bounds: () => (failing ? problem(500, "internal_error") : json(EMPTY)) });
    renderHome();
    await expectStep("Send your first trace", "Couldn't check");
    await expectStep("Instrument an MCP server", "Couldn't check");
    const trace = step("Send your first trace");
    expect(within(trace).getByText("This section couldn't load.")).toBeTruthy();
    expect(within(trace).getByText("req-1")).toBeTruthy();
    expect(within(trace).getByText(/give your administrator this code/)).toBeTruthy();
    expect(within(trace).queryByText(/Reference/)).toBeNull();
    expect(within(section("Set up")).queryByText("raw server title")).toBeNull();
    // The tiles don't depend on it.
    expect(await within(section("Traces")).findByText("1")).toBeTruthy();
    // Every control keeps a name of its own.
    const labels = createAgent()
      .snapshot()
      .filter((entry) => !entry.offPage && entry.component !== "more")
      .map((entry) => entry.label);
    expect(labels).toContain("Check again: Send your first trace");
    expect(new Set(labels).size).toBe(labels.length);

    failing = false;
    const metricsBefore = requested("/metrics/query");
    await act(async () =>
      within(trace).getByRole("button", { name: "Check again: Send your first trace" }).click(),
    );
    await expectStep("Send your first trace", "To do");
    expect(requested("/data-bounds")).toBe(2);
    expect(requested("/metrics/query")).toBe(metricsBefore);
  });

  test("error: an answer that may miss data is never read as no data", async () => {
    serve({
      bounds: () => json(bounds({ spans: NONE, server: NONE, meta: { ...WHOLE, partial: true } })),
    });
    renderHome();
    await expectStep("Send your first trace", "Couldn't check");
    expect(
      within(step("Send your first trace")).getByText("Some data may be missing from the answer."),
    ).toBeTruthy();
    expect(
      within(step("Send your first trace")).getByRole("button", {
        name: "Check again: Send your first trace",
      }),
    ).toBeTruthy();
  });

  test("error: a failed key or members read stays in its own row", async () => {
    signIn();
    serve({
      keys: () => problem(503, "control_plane_unavailable"),
      members: () => json({ data: TEAM }),
    });
    renderHome();
    await expectStep("Create an API key", "Couldn't check");
    await expectStep("Send your first trace", "Done");
    await expectStep("Invite your team", "Done");
  });
});
