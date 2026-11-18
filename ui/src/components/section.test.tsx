import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { act, render, screen } from "@testing-library/react";
import { afterEach, expect, test, vi } from "vitest";
import { UiError } from "@/api/errors";
import { Section } from "./section";

afterEach(() => {
  vi.useRealTimers();
});

function Probe({ queryFn }: { queryFn: () => Promise<string[]> }) {
  const query = useQuery({ queryKey: ["probe"], queryFn, retry: false });
  return (
    <Section
      title="Probe rows"
      query={query}
      skeleton={<p>Skeleton rows</p>}
      isEmpty={(rows) => rows.length === 0}
      empty={<p>Nothing here</p>}
    >
      {(rows) => <p>{rows.join(", ")}</p>}
    </Section>
  );
}

function renderProbe(queryFn: () => Promise<string[]>) {
  const client = new QueryClient();
  render(
    <QueryClientProvider client={client}>
      <Probe queryFn={queryFn} />
    </QueryClientProvider>,
  );
  return client;
}

test("loading ladder: nothing before 300 ms, a skeleton until 5 s, then a label", () => {
  vi.useFakeTimers();
  renderProbe(() => new Promise<string[]>(() => {}));
  const skeleton = screen.getByText("Skeleton rows");
  const section = screen.getByRole("region", { name: "Probe rows" });

  expect(section.getAttribute("aria-busy")).toBe("true");
  expect(skeleton.parentElement?.className).toContain("invisible");
  expect(screen.getByRole("status").textContent).toBe("");

  act(() => vi.advanceTimersByTime(299));
  expect(skeleton.parentElement?.className).toContain("invisible");

  act(() => vi.advanceTimersByTime(1));
  expect(skeleton.parentElement?.className).not.toContain("invisible");
  expect(screen.getByRole("status").textContent).toBe("Loading probe rows");
  expect(screen.queryByText(/Still loading/)).toBeNull();

  act(() => vi.advanceTimersByTime(4_700));
  expect(screen.getByText("Still loading. Long time ranges take longer.")).toBeTruthy();
});

test("a failed refresh keeps the last rows and offers a retry", async () => {
  let call = 0;
  const client = renderProbe(async () => {
    call += 1;
    if (call > 1) throw new UiError({ kind: "network" });
    return ["alpha", "beta"];
  });
  expect(await screen.findByText("alpha, beta")).toBeTruthy();

  await act(async () => {
    await client.refetchQueries({ queryKey: ["probe"] });
  });

  expect(await screen.findByText(/Couldn't refresh\. Showing data from/)).toBeTruthy();
  expect(screen.getByText("alpha, beta")).toBeTruthy();
  expect(screen.getByRole("button", { name: "Retry probe rows" })).toBeTruthy();
});
