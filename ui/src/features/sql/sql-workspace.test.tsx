import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { SqlResult } from "@/api/generated/SqlResult";
import type { SqlSchema } from "@/api/generated/SqlSchema";
import { createQueryClient } from "@/components/providers";
import { SqlWorkspace } from "./sql-workspace";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const agent = createAgent();

const EXAMPLE =
  "SELECT count() FROM crucible_sql.spans WHERE start_time >= {from:DateTime64(6)} AND start_time < {to:DateTime64(6)} AND model = {model:String}";

const SCHEMA: SqlSchema = {
  status: { enabled: true, failed_checks: [] },
  relations: [
    {
      name: "crucible_sql.spans",
      description: "One row per span of this project.",
      time_column: "start_time",
      window_required: true,
      columns: [{ name: "start_time", type: "DateTime64(6)" }],
      example: EXAMPLE,
    },
  ],
  limits: {
    max_rows: 10_000,
    max_bytes: 16_777_216,
    max_cell_bytes: 65_536,
    max_execution_s: 30,
    max_sql_bytes: 65_536,
  },
};

const RESULT: SqlResult = {
  run_id: "run-1",
  columns: [{ name: "count()", type: "UInt64" }],
  rows: [["7"]],
  truncated: false,
  cut_cells: 0,
  stats: { rows_read: 7, bytes_read: 512, elapsed_ms: 3, estimated_rows: 7 },
};

const json = (body: unknown, status = 200, type = "application/json") =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": type } });

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/sql/?project=p-1&range=7d");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

function renderWorkspace(run: () => Response) {
  fetchMock.mockImplementation(async (input, init) => {
    const path = new URL(String(input), "http://localhost").pathname;
    if (path.endsWith("/sql/schema")) return json(SCHEMA);
    if (path.endsWith("/sql/run") && init?.method === "POST") return run();
    return json({ data: [] });
  });
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(
    <QueryClientProvider client={client}>
      <SqlWorkspace />
    </QueryClientProvider>,
  );
}

function runRequest(): { url: URL; body: Record<string, unknown> } {
  const call = fetchMock.mock.calls.find(([, init]) => init?.method === "POST");
  return {
    url: new URL(String(call?.[0]), "http://localhost"),
    body: JSON.parse(String(call?.[1]?.body)) as Record<string, unknown>,
  };
}

test("an agent finds every control by name, fills a parameter and runs the query", async () => {
  renderWorkspace(() => json(RESULT));
  expect(await screen.findByDisplayValue(EXAMPLE)).toBeTruthy();
  await screen.findByText("No runs yet.");
  const components = agent
    .snapshot()
    .filter((entry) => !entry.offPage && entry.component !== "more");
  for (const entry of components) {
    expect(entry.label, `${entry.component} ${entry.ref} has no name`).toBeTruthy();
  }
  const labels = components.map((entry) => entry.label);
  expect(labels).toEqual(expect.arrayContaining(["Query text", "model", "Run", "Export CSV"]));

  const result = await agent.act({
    steps: [
      { ref: "model", action: "setValue", args: ["gpt-4.1"] },
      { ref: "Run", action: "press" },
    ],
  });
  expect(result.ok, result.error?.message).toBe(true);
  expect(await screen.findByText("1 row returned. Read 7 rows (512 B) in 3 ms.")).toBeTruthy();
  expect(screen.getByRole("cell", { name: "7" })).toBeTruthy();

  const { url, body } = runRequest();
  expect(url.pathname).toBe("/api/v1/projects/p-1/sql/run");
  expect(body.sql).toBe(EXAMPLE);
  expect(body.params).toEqual({ model: "gpt-4.1" });
  expect(Date.parse(String(body.to)) - Date.parse(String(body.from))).toBe(7 * 24 * 60 * 60 * 1000);
});

test("a refused run shows the title for its code, the server's fix and what ClickHouse said", async () => {
  renderWorkspace(() =>
    json(
      {
        code: "sql_invalid",
        title: "raw title",
        detail: "Correct the query near line 1.",
        line: 1,
        column: 8,
        errors: [{ pointer: "/sql", code: "clickhouse", message: "Syntax error at position 8" }],
      },
      422,
      "application/problem+json",
    ),
  );
  await screen.findByDisplayValue(EXAMPLE);
  const result = await agent.act({ steps: [{ ref: "Run", action: "press" }] });
  expect(result.ok, result.error?.message).toBe(true);
  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("The query has an error.");
  expect(alert.textContent).toContain("Correct the query near line 1.");
  expect(alert.textContent).toContain("ClickHouse said");
  expect(alert.textContent).toContain("Syntax error at position 8");
  expect(alert.textContent).toContain("Line 1, column 8");
  expect(screen.queryByText(/raw title/)).toBeNull();
  const editor = screen.getByLabelText("Query text") as HTMLTextAreaElement;
  expect(editor.selectionStart).toBe(7);
});

test("switching the project starts over: the result goes and the new project's schema loads", async () => {
  renderWorkspace(() => json(RESULT));
  await screen.findByDisplayValue(EXAMPLE);
  const ran = await agent.act({ steps: [{ ref: "Run", action: "press" }] });
  expect(ran.ok, ran.error?.message).toBe(true);
  await screen.findByRole("cell", { name: "7" });

  act(() => window.history.replaceState(null, "", "/sql/?project=p-2"));

  expect(await screen.findByText("No result yet.")).toBeTruthy();
  expect(screen.queryByRole("cell", { name: "7" })).toBeNull();
  await screen.findByDisplayValue(EXAMPLE);
  const paths = fetchMock.mock.calls.map(([input]) => String(input).split("?")[0]);
  expect(paths).toContain("/api/v1/projects/p-2/sql/schema");
});
