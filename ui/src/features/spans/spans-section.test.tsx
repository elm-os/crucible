import { act, fireEvent, screen } from "@testing-library/react";
import { createAgent } from "@ui-sdk/react/agent";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { json, page, problem, renderWithClient, requests, spanItem } from "@/test/render";
import { SpansSection } from "./spans-section";

vi.mock("next/navigation", () => import("@/test/navigation"));

const fetchMock = vi.fn<typeof fetch>();
const agent = createAgent();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

const search = () => new URLSearchParams(window.location.search);

describe("spans", () => {
  test("success: rows link to the span in its trace; the URL's filters reach the request", async () => {
    window.history.replaceState(
      null,
      "",
      "/spans/?project=p-1&level=error&observation_type=tool&observation_type=bogus&name=send",
    );
    fetchMock.mockResolvedValue(json(page([spanItem({ level: "error" })])));
    renderWithClient(<SpansSection />);
    const link = await screen.findByRole("link", { name: "plan reply" });
    const href = new URL(link.getAttribute("href") ?? "", "http://localhost");
    expect(Object.fromEntries(href.searchParams)).toEqual({
      project: "p-1",
      trace: "4bf92f3577b34da6a3ce929d0e0e4736",
      span: "a000000000000002",
    });
    expect(screen.getByText("Error").getAttribute("data-status")).toBe("error");
    const [url] = requests(fetchMock.mock.calls);
    expect(url?.pathname).toBe("/api/v1/projects/p-1/spans");
    expect(url?.searchParams.getAll("level")).toEqual(["error"]);
    // A value the API would refuse never leaves the URL for the request.
    expect(url?.searchParams.getAll("observation_type")).toEqual(["tool"]);
    expect(url?.searchParams.get("name")).toBe("send");
    // Each condition shows as a chip that removes it.
    expect(screen.getByRole("button", { name: "Remove filter Level: error" })).toBeTruthy();
  });

  test("filters: adding and removing conditions rewrites the URL and refetches", async () => {
    window.history.replaceState(null, "", "/spans/?project=p-1");
    fetchMock.mockImplementation(async () => json(page([spanItem()])));
    renderWithClient(<SpansSection />);
    await screen.findByRole("link", { name: "plan reply" });

    const value = screen.getByRole("textbox", { name: "Value" });
    fireEvent.input(value, { target: { value: "checkout" } });
    await act(async () => screen.getByRole("button", { name: "Add filter" }).click());
    expect(search().get("name")).toBe("checkout");
    expect(requests(fetchMock.mock.calls).at(-1)?.searchParams.get("name")).toBe("checkout");

    // An empty value says what is missing instead of doing nothing.
    await act(async () => screen.getByRole("button", { name: "Add filter" }).click());
    expect(screen.getByText("Enter a value to filter by.")).toBeTruthy();

    await act(async () =>
      screen.getByRole("button", { name: "Remove filter Name contains: checkout" }).click(),
    );
    expect(search().get("name")).toBeNull();

    const result = await agent.act({
      steps: [{ ref: "Filter by", action: "setValue", args: ["Metadata"] }],
    });
    expect(result.ok, result.error?.message).toBe(true);
    fireEvent.input(screen.getByRole("textbox", { name: "Value" }), {
      target: { value: "no-equals" },
    });
    await act(async () => screen.getByRole("button", { name: "Add filter" }).click());
    expect(screen.getByText("Write metadata as key=value, for example team=search.")).toBeTruthy();
    fireEvent.input(screen.getByRole("textbox", { name: "Value" }), {
      target: { value: "team=search" },
    });
    await act(async () => screen.getByRole("button", { name: "Add filter" }).click());
    expect(search().getAll("metadata")).toEqual(["team=search"]);
    await act(async () => screen.getByRole("button", { name: "Clear filters" }).click());
    expect(search().getAll("metadata")).toEqual([]);
  });

  test("error, and empty under filters offers to clear them", async () => {
    window.history.replaceState(null, "", "/spans/?project=p-1&tags=rare");
    fetchMock.mockResolvedValueOnce(problem(422, "query_too_broad"));
    fetchMock.mockImplementation(async () => json(page([])));
    renderWithClient(<SpansSection />);
    expect(await screen.findByText("The range is longer than this project allows.")).toBeTruthy();
    expect(screen.queryByText(/raw server title/)).toBeNull();
    window.history.replaceState(null, "", "/spans/?project=p-1&tags=rare&tags=common");
    const empty = await screen.findByText("No spans match these filters in the last 24 hours.");
    expect(empty.closest("[data-empty]")?.getAttribute("data-empty")).toBe("filtered");
    await act(async () => screen.getAllByRole("button", { name: "Clear filters" })[0]!.click());
    expect(search().getAll("tags")).toEqual([]);
  });
});
