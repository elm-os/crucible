import { act, render, screen } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import { PageError } from "./page-error";

test("a page that failed to render says so in plain words and tries again", async () => {
  const retry = vi.fn();
  render(<PageError retry={retry} />);
  expect(screen.getByRole("alert").textContent).toBe(
    "This page couldn't be shown.Try again. If this keeps happening, reload the page.Try again",
  );
  await act(async () => screen.getByRole("button", { name: "Try again" }).click());
  expect(retry).toHaveBeenCalledOnce();
});
