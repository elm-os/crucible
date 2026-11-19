import { QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { listTraces } from "@/api/client";
import { createQueryClient } from "@/components/providers";
import { ConnectDialog } from "./connect-dialog";

vi.mock("next/navigation", () => import("@/test/navigation"));

const PROJECT = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";
const KEY = `csk_${"7".repeat(43)}`;
const fetchMock = vi.fn<typeof fetch>();

beforeEach(() => {
  vi.stubGlobal("fetch", fetchMock);
  window.history.replaceState(null, "", "/traces/?range=7d");
});

afterEach(() => {
  fetchMock.mockReset();
  vi.unstubAllGlobals();
});

const answer = (status: number) =>
  new Response("{}", { status, headers: { "content-type": "application/json" } });

function renderDialog() {
  return render(
    <QueryClientProvider client={createQueryClient()}>
      <ConnectDialog />
    </QueryClientProvider>,
  );
}

async function open() {
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Connect" }));
  });
  return screen.getByRole("dialog", { name: "Connect to a project" });
}

function submitButton(dialog: HTMLElement) {
  const button = dialog.querySelector<HTMLButtonElement>('button[type="submit"]');
  if (button === null) throw new Error("the dialog has no submit button");
  return button;
}

test("the dialog says why it is there and names its fields", async () => {
  renderDialog();
  const dialog = await open();
  expect(dialog.textContent).toContain("This server runs without a control plane");
  expect(dialog.textContent).toContain("With Postgres configured, people sign in instead");
  expect(screen.getByLabelText("Project id")).toBeTruthy();
  expect(screen.getByLabelText("Secret key").getAttribute("type")).toBe("password");
});

test("a key the server accepts is remembered in this browser and sent as a bearer token", async () => {
  renderDialog();
  const dialog = await open();
  fetchMock.mockResolvedValueOnce(answer(200));
  fireEvent.change(screen.getByLabelText("Project id"), {
    target: { value: `  ${PROJECT.toUpperCase()} ` },
  });
  fireEvent.change(screen.getByLabelText("Secret key"), { target: { value: `${KEY}\n` } });
  await act(async () => {
    fireEvent.submit(submitButton(dialog).form as HTMLFormElement);
  });

  const [url, init] = fetchMock.mock.calls[0] ?? [];
  expect(url).toBe(`/api/v1/projects/${PROJECT}/data-bounds`);
  expect(new Headers(init?.headers).get("authorization")).toBe(`Bearer ${KEY}`);
  await waitFor(() => expect(screen.getByText("Connected")).toBeTruthy());
  expect(localStorage.getItem("crucible.connection.key")).toBe(KEY);
  expect(localStorage.getItem("crucible.connection.project")).toBe(PROJECT);
  expect(JSON.stringify({ ...sessionStorage })).not.toContain(KEY);
  expect(window.location.search).toBe(`?range=7d&project=${PROJECT}`);

  fetchMock.mockResolvedValueOnce(answer(200));
  await listTraces(PROJECT, { from: "2026-11-17T00:00:00Z", to: "2026-11-18T00:00:00Z" });
  const [, listInit] = fetchMock.mock.calls[1] ?? [];
  expect(new Headers(listInit?.headers).get("authorization")).toBe(`Bearer ${KEY}`);
});

test.each([
  [401, "rejected", "didn't accept this key"],
  [404, "wrong_project", "belongs to a different project"],
])("a %i answer keeps the dialog open and says why", async (status, kind, text) => {
  renderDialog();
  const dialog = await open();
  fetchMock.mockResolvedValueOnce(answer(status));
  fireEvent.change(screen.getByLabelText("Project id"), { target: { value: PROJECT } });
  fireEvent.change(screen.getByLabelText("Secret key"), { target: { value: KEY } });
  await act(async () => {
    fireEvent.submit(submitButton(dialog).form as HTMLFormElement);
  });
  const alert = await screen.findByRole("alert");
  expect(alert.getAttribute("data-error")).toBe(kind);
  expect(alert.textContent).toContain(text);
  expect(localStorage.length).toBe(0);
  expect(submitButton(dialog).disabled).toBe(false);
});

test("a server that cannot be reached is reported, and nothing is kept", async () => {
  renderDialog();
  const dialog = await open();
  fetchMock.mockRejectedValueOnce(new TypeError("failed to fetch"));
  fireEvent.change(screen.getByLabelText("Project id"), { target: { value: PROJECT } });
  fireEvent.change(screen.getByLabelText("Secret key"), { target: { value: KEY } });
  await act(async () => {
    fireEvent.submit(submitButton(dialog).form as HTMLFormElement);
  });
  expect((await screen.findByRole("alert")).textContent).toContain("Couldn't reach Crucible");
  expect(localStorage.length).toBe(0);
});

test("disconnect forgets the key and the project", async () => {
  localStorage.setItem("crucible.connection.key", KEY);
  localStorage.setItem("crucible.connection.project", PROJECT);
  window.history.replaceState(null, "", `/traces/?project=${PROJECT}`);
  renderDialog();
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Connected" }));
  });
  expect((screen.getByLabelText("Project id") as HTMLInputElement).value).toBe(PROJECT);
  expect((screen.getByLabelText("Secret key") as HTMLInputElement).value).toBe("");
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
  });
  expect(localStorage.length).toBe(0);
  expect(window.location.search).toBe("");
  await waitFor(() => expect(screen.getByRole("button", { name: "Connect" })).toBeTruthy());
});
