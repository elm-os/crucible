import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import type { McpClientList } from "@/api/generated/McpClientList";
import type { McpCompare } from "@/api/generated/McpCompare";
import type { McpOperationDetail } from "@/api/generated/McpOperationDetail";
import type { McpOperationPage } from "@/api/generated/McpOperationPage";
import type { McpOperationRow } from "@/api/generated/McpOperationRow";
import type { McpServerList } from "@/api/generated/McpServerList";
import type { McpServerSource } from "@/api/generated/McpServerSource";
import type { McpStats } from "@/api/generated/McpStats";
import type { McpToolDetail } from "@/api/generated/McpToolDetail";
import type { McpToolList } from "@/api/generated/McpToolList";
import type { Meta } from "@/api/generated/Meta";
import { createQueryClient } from "@/components/providers";
import { ClientsSection } from "./clients-section";
import { CompareSection } from "./compare-section";
import { formatRate, sourcesSentence } from "./format";
import { McpBar } from "./mcp-bar";
import { OperationDetail } from "./operation-detail";
import { OperationsSection } from "./operations-section";
import { ServersSection } from "./servers-section";
import { ToolDetail } from "./tool-detail";
import { ToolsSection } from "./tools-section";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const agent = createAgent();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/mcp/servers/?project=p-1");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

function renderWith(children: ReactNode) {
  const client = createQueryClient();
  client.setDefaultOptions({ queries: { ...client.getDefaultOptions().queries, retry: false } });
  return render(<QueryClientProvider client={client}>{children}</QueryClientProvider>);
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

const META: Meta = { as_of: "2026-11-17T14:00:05.000000Z", partial: false, consistency: "strong" };
const LAGGING: Meta = { ...META, consistency: "lagging" };

function stats(fields: Partial<McpStats> = {}): McpStats {
  return {
    calls: 120,
    errors: 6,
    error_rate: 0.05,
    caller_faults: 2,
    latency: { p50_us: 12_000, p90_us: 40_000, p99_us: 250_000 },
    last_seen: "2026-11-17T13:59:00.000000Z",
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

function lastUrl(): URL {
  return new URL(String(fetchMock.mock.calls.at(-1)?.[0]), "http://localhost");
}

describe("servers", () => {
  test("rows link to the server's tools, and the note names the source read for each server", async () => {
    fetchMock.mockImplementation(async () => json(servers()));
    renderWith(<ServersSection />);
    const link = await screen.findByRole("link", { name: "files" });
    const href = new URL(link.getAttribute("href") ?? "", "http://localhost");
    expect(href.pathname).toMatch(/^\/mcp\/tools\/?$/);
    expect(href.searchParams.get("server")).toBe("files");
    expect(href.searchParams.get("project")).toBe("p-1");
    expect(screen.getByText("Read files from the SDK, db from OTLP.")).toBeTruthy();
    expect(screen.getByText("1.4.0")).toBeTruthy();
    expect(screen.getByText("Unknown")).toBeTruthy();
    expect(screen.getAllByText("5.0%")).toHaveLength(2);

    const url = lastUrl();
    expect(url.pathname).toBe("/api/v1/projects/p-1/mcp/servers");
    // The defaults stay off the wire: the automatic source and the caller-fault rule.
    expect([...url.searchParams.keys()]).toEqual(["side", "from", "to"]);
    expect(url.searchParams.get("side")).toBe("server");
  });

  test("a source a server lacks is named as not read, with the sources it has", async () => {
    window.history.replaceState(null, "", "/mcp/servers/?project=p-1&source=proxy");
    fetchMock.mockImplementation(async () =>
      json(
        servers({
          data: servers().data.slice(0, 1),
          capture_sources: [
            { server_name: "files", available: ["proxy", "otlp"], chosen: "proxy" },
            { server_name: "db", available: ["otlp"], chosen: null },
          ],
        }),
      ),
    );
    renderWith(<ServersSection />);
    expect(
      await screen.findByText(
        "Read files from the proxy. Not read: db (no rows of this capture source; it has OTLP).",
      ),
    ).toBeTruthy();
    expect(lastUrl().searchParams.get("source")).toBe("proxy");
  });

  test("empty: an empty answer that may be incomplete offers to check again", async () => {
    fetchMock.mockImplementation(async () => json(servers({ data: [], meta: LAGGING })));
    renderWith(<ServersSection />);
    expect(await screen.findByText("No MCP servers to show yet.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Check again" })).toBeTruthy();
  });

  test("empty: a whole empty answer offers the widest range, which invites the first server", async () => {
    fetchMock.mockImplementation(async () => json(servers({ data: [], capture_sources: [] })));
    renderWith(<ServersSection />);
    expect(await screen.findByText("No MCP servers in the last 24 hours.")).toBeTruthy();
    const result = await agent.act({ steps: [{ ref: "Show last 30 days", action: "press" }] });
    expect(result.ok, result.error?.message).toBe(true);
    expect(await screen.findByText("No MCP servers in the last 30 days.")).toBeTruthy();
    expect(screen.getByText(/Wrap your MCP server with a Crucible SDK/)).toBeTruthy();
  });

  test("error: an unreachable API is an in-section error with a retry", async () => {
    fetchMock.mockRejectedValue(new TypeError("Failed to fetch"));
    renderWith(<ServersSection />);
    expect(await screen.findByText("Couldn't reach Crucible.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Retry servers" })).toBeTruthy();
  });
});

describe("the MCP bar", () => {
  test("side, source and caller faults go to the URL and into the next request", async () => {
    fetchMock.mockImplementation(async () => json(servers()));
    renderWith(
      <>
        <McpBar active="servers" />
        <ServersSection />
      </>,
    );
    await screen.findByRole("link", { name: "files" });
    expect(screen.getByRole("link", { name: "Servers" }).getAttribute("aria-current")).toBe("page");

    const side = await agent.act({
      steps: [{ ref: "Side", action: "setValue", args: ["client"] }],
    });
    expect(side.ok, side.error?.message).toBe(true);
    await waitFor(() => expect(lastUrl().searchParams.get("side")).toBe("client"));

    const source = await agent.act({
      steps: [{ ref: "Capture source", action: "setValue", args: ["Proxy"] }],
    });
    expect(source.ok, source.error?.message).toBe(true);
    await waitFor(() => expect(lastUrl().searchParams.get("source")).toBe("proxy"));

    await act(async () => screen.getByRole("switch", { name: "Include caller faults" }).click());
    await waitFor(() => expect(lastUrl().searchParams.get("include_caller_faults")).toBe("true"));
    const url = new URLSearchParams(window.location.search);
    expect(Object.fromEntries(url)).toEqual({
      project: "p-1",
      side: "client",
      source: "proxy",
      faults: "1",
    });
    // Links between MCP pages keep the scope.
    const tools = new URL(
      screen.getByRole("link", { name: "Tools" }).getAttribute("href") ?? "",
      "http://localhost",
    );
    expect(tools.searchParams.get("source")).toBe("proxy");
    expect(tools.searchParams.get("faults")).toBe("1");
  });
});

function toolList(): McpToolList {
  return {
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
}

describe("tools", () => {
  test("called tools link to their detail; unused ones list their schema hash", async () => {
    fetchMock.mockImplementation(async () => json(toolList()));
    renderWith(<ToolsSection />);
    const link = await screen.findByRole("link", { name: "read_file" });
    const href = new URL(link.getAttribute("href") ?? "", "http://localhost");
    expect(href.pathname).toMatch(/^\/mcp\/tool\/?$/);
    expect(href.searchParams.get("name")).toBe("read_file");
    expect(screen.getByRole("tab", { name: "Unused (1)" })).toBeTruthy();
    await act(async () => screen.getByRole("tab", { name: "Unused (1)" }).click());
    expect(await screen.findByText("fedcba9876543210")).toBeTruthy();
  });
});

const bucket = (minute: number) => `2026-11-17T13:${String(minute).padStart(2, "0")}:00.000000Z`;

function toolDetail(fields: Partial<McpToolDetail> = {}): McpToolDetail {
  return {
    tool_name: "read_file",
    stats: stats(),
    request_bytes: 100,
    response_bytes: 2048,
    granularity: "minute",
    series: [0, 1, 2].map((minute) => ({
      bucket: bucket(minute),
      calls: 40,
      errors: 2,
      latency: { p50_us: 10_000 + minute, p90_us: 40_000, p99_us: 250_000 },
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
        first_seen: bucket(1),
        last_seen: bucket(2),
      },
    ],
    capture_sources: SOURCES.slice(0, 1),
    saturated: false,
    meta: META,
    ...fields,
  };
}

describe("tool detail", () => {
  test("percentiles chart one family with deploy markers, and every chart has a table view", async () => {
    window.history.replaceState(null, "", "/mcp/tool/?project=p-1&name=read_file");
    fetchMock.mockImplementation(async () => json(toolDetail()));
    renderWith(<ToolDetail />);
    await screen.findByText("Tool failed");
    const latency = screen.getByRole("region", { name: "Latency percentiles" });
    expect(within(latency).getByText("p50")).toBeTruthy();
    expect(within(latency).getByText("p99")).toBeTruthy();
    expect(within(latency).getByText(/Deploy d2 of files/)).toBeTruthy();
    expect(latency.querySelectorAll("[data-marker-rule]")).toHaveLength(1);
    await act(async () =>
      within(latency).getByRole("button", { name: "View as table: latency percentiles" }).click(),
    );
    expect(within(latency).getByRole("table")).toBeTruthy();
    expect(within(latency).getAllByRole("row")).toHaveLength(4);
    expect(screen.getByText("Tool failed")).toBeTruthy();
    expect(screen.getByText("Client names are self-reported.")).toBeTruthy();
    expect(lastUrl().pathname).toBe("/api/v1/projects/p-1/mcp/tools/read_file");
  });

  test("without a tool name it points to the tools list and fetches nothing", () => {
    window.history.replaceState(null, "", "/mcp/tool/?project=p-1");
    renderWith(<ToolDetail />);
    expect(screen.getByText("No tool is selected.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe("clients", () => {
  test("names are labelled self-reported; the version view reads the breakdown", async () => {
    window.history.replaceState(null, "", "/mcp/clients/?project=p-1&view=breakdown");
    const list: McpClientList = {
      view: "breakdown",
      self_reported: true,
      data: [],
      versions: [
        {
          client_name: "claude-desktop",
          client_version: "0.9.1",
          calls: 30,
          last_seen: "2026-11-17T13:59:00.000000Z",
        },
      ],
      truncated: true,
      capture_sources: SOURCES,
      saturated: false,
      meta: META,
    };
    fetchMock.mockImplementation(async () => json(list));
    renderWith(<ClientsSection />);
    expect(await screen.findByText("0.9.1")).toBeTruthy();
    expect(screen.getByText(/^Self-reported:/)).toBeTruthy();
    expect(screen.getByText(/Showing the 1 versions with the most calls/)).toBeTruthy();
    expect(lastUrl().searchParams.get("view")).toBe("breakdown");
  });
});

function operation(fields: Partial<McpOperationRow> = {}): McpOperationRow {
  return {
    trace_id: "4bf92f3577b34da6a3ce929d0e0e4736",
    span_id: "00f067aa0ba902b7",
    parent_span_id: null,
    start_time: "2026-11-17T13:58:00.000000Z",
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

describe("operations", () => {
  test("load more reads the next page with its cursor; a filter from the URL can be cleared", async () => {
    window.history.replaceState(null, "", "/mcp/operations/?project=p-1&target=read_file");
    const first: McpOperationPage = {
      data: [operation()],
      next_cursor: "c-2",
      has_more: true,
      capture_sources: SOURCES,
      meta: META,
    };
    const second: McpOperationPage = {
      ...first,
      data: [
        operation({
          span_id: "11f067aa0ba902b7",
          status: "tool_error",
          error_class: "validation",
          caller_fault: true,
        }),
      ],
      next_cursor: null,
      has_more: false,
    };
    fetchMock.mockResolvedValueOnce(json(first)).mockResolvedValueOnce(json(second));
    renderWith(<OperationsSection />);
    expect(await screen.findByText("OK")).toBeTruthy();
    expect(lastUrl().searchParams.get("target")).toBe("read_file");
    await act(async () => screen.getByRole("button", { name: "Load more operations" }).click());
    const failed = await screen.findByText("Invalid arguments (caller)");
    expect(failed.getAttribute("data-status")).toBe("error");
    expect(lastUrl().searchParams.get("cursor")).toBe("c-2");

    fetchMock.mockImplementation(async () =>
      json({ ...first, has_more: false, next_cursor: null }),
    );
    await act(async () => screen.getByRole("button", { name: "Clear the target filter" }).click());
    await waitFor(() => expect(lastUrl().searchParams.get("target")).toBeNull());
    expect(new URLSearchParams(window.location.search).get("target")).toBeNull();
  });
});

describe("operation detail", () => {
  test("shows the captured content, the other side's row and a link into the trace", async () => {
    window.history.replaceState(
      null,
      "",
      "/mcp/operation/?project=p-1&trace=4bf92f3577b34da6a3ce929d0e0e4736&span=00f067aa0ba902b7",
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
        result: { preview: "x".repeat(64), bytes: 40_000, truncated: true },
        other_side: operation({ span_id: "22f067aa0ba902b7", duration_us: 15_000 }),
        chain: [],
      },
      meta: META,
    };
    fetchMock.mockImplementation(async () => json(detail));
    renderWith(<OperationDetail />);
    expect(await screen.findByText('{"path":"/etc/hosts"}')).toBeTruthy();
    expect(screen.getByText(/Showing the start of 39.1 KB/)).toBeTruthy();
    expect(screen.getByRole("link", { name: "Client side, 15 ms" })).toBeTruthy();
    const trace = new URL(
      screen.getByRole("link", { name: "Open in the trace" }).getAttribute("href") ?? "",
      "http://localhost",
    );
    expect(trace.searchParams.get("span")).toBe("00f067aa0ba902b7");
    expect(lastUrl().pathname).toBe(
      "/api/v1/projects/p-1/mcp/operations/4bf92f3577b34da6a3ce929d0e0e4736/00f067aa0ba902b7",
    );
  });

  test("an operation outside the window is a designed empty state", async () => {
    window.history.replaceState(null, "", "/mcp/operation/?project=p-1&trace=t&span=s");
    fetchMock.mockImplementation(async () => json({ operation: null, meta: META }));
    renderWith(<OperationDetail />);
    expect(
      await screen.findByText("This operation isn't in the selected time range."),
    ).toBeTruthy();
  });
});

describe("compare", () => {
  test("without two deploys it explains how to pick them and fetches nothing", () => {
    window.history.replaceState(null, "", "/mcp/compare/?project=p-1");
    renderWith(<CompareSection />);
    expect(screen.getByText("Pick two deploys to compare.")).toBeTruthy();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  test("per-tool deltas and the tools added, removed or changed", async () => {
    window.history.replaceState(
      null,
      "",
      "/mcp/compare/?project=p-1&server=files&baseline=d1&candidate=d2",
    );
    const compare: McpCompare = {
      server_name: "files",
      tools: [
        {
          tool_name: "read_file",
          baseline: stats({ calls: 100 }),
          candidate: stats({ calls: 150, error_rate: 0.1 }),
        },
      ],
      added: ["move_file"],
      removed: [],
      changed: ["read_file"],
      capture_sources: SOURCES.slice(0, 1),
      saturated: false,
      meta: META,
    };
    fetchMock.mockImplementation(async () => json(compare));
    renderWith(<CompareSection />);
    expect(await screen.findByText("move_file")).toBeTruthy();
    expect(screen.getByText("Removed (0)")).toBeTruthy();
    const row = screen.getAllByRole("row")[1];
    expect(row?.textContent).toContain("100 to 150");
    expect(row?.textContent).toContain("5.0% to 10%");
    const url = lastUrl();
    expect(url.searchParams.get("server")).toBe("files");
    expect(url.searchParams.get("baseline_deploy")).toBe("d1");
    expect(url.searchParams.get("candidate_deploy")).toBe("d2");
  });
});

describe("format", () => {
  test("rates and the source sentence", () => {
    expect(formatRate(null)).toBe("No calls");
    expect(formatRate(0)).toBe("0%");
    expect(formatRate(0.0004)).toBe("<0.1%");
    expect(formatRate(0.25)).toBe("25%");
    expect(sourcesSentence([])).toBeNull();
    expect(sourcesSentence([{ server_name: "a", available: ["proxy"], chosen: "proxy" }])).toBe(
      "a from the proxy",
    );
  });
});
