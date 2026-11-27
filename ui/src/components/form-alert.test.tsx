import { act, render, screen } from "@testing-library/react";
import { Field, FieldError, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import { afterEach, describe, expect, test, vi } from "vitest";
import { fromHttpFailure, networkError, type UiError } from "@/api/errors";
import type { ProblemBody } from "@/api/types";
import { FormAlert, failureCopy } from "./form-alert";

const ID = "01a216d5-f11d-794a-9801-5490863d6314";

function failure(status: number, problem: ProblemBody | null, retryAfter: string | null = null) {
  return fromHttpFailure({
    status,
    problem,
    requestIdHeader: problem === null ? ID : null,
    retryAfterHeader: retryAfter,
  });
}

const problem = (status: number, code: string, fields: Partial<ProblemBody> = {}) =>
  failure(status, {
    code,
    status,
    title: "raw title",
    detail: "raw detail",
    request_id: ID,
    ...fields,
  });

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("what a failed form says", () => {
  test.each([
    ["a wrong password", problem(401, "invalid_credentials"), false],
    [
      "a field to correct",
      problem(422, "validation_failed", { errors: [{ pointer: "/name", code: "x" }] }),
      false,
    ],
    ["a conflict", problem(409, "conflict"), false],
    ["a role", problem(403, "forbidden"), false],
    ["a gone item", problem(404, "not_found"), false],
    ["an expired link", problem(422, "token_invalid"), false],
    ["a rate limit", problem(429, "rate_limited"), false],
    ["a guardrail", problem(422, "query_too_broad"), false],
    ["a change made elsewhere", problem(412, "precondition_failed"), false],
    ["a server fault", problem(500, "internal_error"), true],
    ["a database outage", problem(503, "control_plane_unavailable"), true],
    ["a code this page doesn't know", problem(400, "brand_new"), true],
    ["an answer that isn't a Crucible problem", failure(502, null), true],
  ])("%s gives the administrator's code: %s", (_case, error, unexpected) => {
    const copy = failureCopy(error, "Couldn't save the rule.");
    expect(copy.reference ?? null).toBe(unexpected ? ID : null);
    expect(`${copy.title} ${copy.detail}`).not.toContain("raw");
  });

  test("a form's own words win, and still carry the code of an unexpected failure", () => {
    const own = { control_plane_unavailable: { title: "Own title.", detail: "Own detail." } };
    expect(failureCopy(problem(503, "control_plane_unavailable"), "x", own)).toEqual({
      title: "Own title.",
      detail: "Own detail.",
      reference: ID,
    });
  });

  test.each([
    [networkError(), "Couldn't reach Crucible.", "Check your connection, then try again."],
    [
      problem(429, "rate_limited", { retry_after_s: 30 }),
      "Too many attempts.",
      "Wait 30 seconds, then try again.",
    ],
    [
      problem(409, "conflict"),
      "Couldn't save the rule.",
      "Reload the page to see the latest version, then try again.",
    ],
    [
      problem(422, "validation_failed"),
      "Some fields need attention.",
      "Check what you entered, then try again.",
    ],
    [
      problem(404, "not_found"),
      "This isn't there anymore.",
      "Someone may have deleted it. Reload the page to see what's there now.",
    ],
    [
      problem(422, "query_too_broad"),
      "The range is longer than this project allows.",
      "Choose a shorter time range.",
    ],
    [
      problem(412, "precondition_failed"),
      "Someone changed this since the page loaded.",
      "Reload the page to see the change, then try again.",
    ],
    [problem(500, "internal_error"), "Couldn't save the rule.", "Try again in a moment."],
  ])("%#: the kind of failure picks the words", (error: UiError, title, detail) => {
    const copy = failureCopy(error, "Couldn't save the rule.");
    expect([copy.title, copy.detail]).toEqual([title, detail]);
  });
});

describe("the alert", () => {
  test("an expected failure shows no code", () => {
    render(<FormAlert copy={failureCopy(problem(403, "forbidden"), "Couldn't create the key.")} />);
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toBe("Your role can't do this.Ask an organization owner or admin.");
    expect(screen.queryByRole("button", { name: "Copy code" })).toBeNull();
  });

  test("an unexpected failure says how to pass the code on, and copies it", async () => {
    const writeText = vi.fn(async () => {});
    vi.stubGlobal("navigator", { clipboard: { writeText } });
    render(
      <FormAlert copy={failureCopy(problem(500, "internal_error"), "Couldn't create the key.")} />,
    );
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain(
      "If this keeps happening, give your administrator this code:",
    );
    expect(screen.getByText(ID).tagName).toBe("CODE");
    expect(alert.textContent).not.toContain("Reference");
    await act(async () => screen.getByRole("button", { name: "Copy code" }).click());
    expect(writeText).toHaveBeenCalledWith(ID);
    expect(screen.getByText("Copied.")).toBeTruthy();
  });

  test("a browser that can't copy says to select the code instead", async () => {
    vi.stubGlobal("navigator", {});
    render(<FormAlert copy={failureCopy(problem(500, "internal_error"), "x")} />);
    await act(async () => screen.getByRole("button", { name: "Copy code" }).click());
    expect(
      screen.getByText("Couldn't copy here. Select the code and copy it yourself."),
    ).toBeTruthy();
  });
});

function NameForm({ rejected, alert }: { rejected: boolean; alert: boolean }) {
  return (
    <form>
      <Field name="note">
        <FieldLabel>Note</FieldLabel>
        <Input />
      </Field>
      <Field name="name" invalid={rejected || undefined}>
        <FieldLabel>Name</FieldLabel>
        <Input />
        <FieldError match={rejected}>Use 1 to 200 characters.</FieldError>
      </Field>
      {alert ? (
        <FormAlert copy={{ title: "Some fields need attention.", detail: "Check." }} />
      ) : null}
      <button type="submit">Save</button>
    </form>
  );
}

describe("focus after a refused submit", () => {
  test("moves to the first field the server rejected, which its message describes", async () => {
    const { rerender } = render(<NameForm rejected={false} alert={false} />);
    screen.getByRole("button", { name: "Save" }).focus();
    rerender(<NameForm rejected alert />);
    const name = screen.getByRole("textbox", { name: "Name" });
    expect(document.activeElement).toBe(name);
    expect(name.getAttribute("aria-invalid")).toBe("true");
    // The field links its message once it has rendered.
    await act(async () => {});
    const described = name.getAttribute("aria-describedby") ?? "";
    expect(described.split(" ").map((id) => document.getElementById(id)?.textContent)).toContain(
      "Use 1 to 200 characters.",
    );
  });

  test("stays put when no field was rejected", () => {
    const { rerender } = render(<NameForm rejected={false} alert={false} />);
    const save = screen.getByRole("button", { name: "Save" });
    save.focus();
    rerender(<NameForm rejected={false} alert />);
    expect(document.activeElement).toBe(save);
  });
});
