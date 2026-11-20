import { render, screen } from "@testing-library/react";
import { describe, expect, test } from "vitest";
import { blocks, Markdown, safeHref } from "./markdown";

describe("markdown", () => {
  test("headings, lists and paragraphs become blocks", () => {
    expect(blocks("# Title\n\nFirst line\nsecond line\n- one\n- two\n\n### Small")).toEqual([
      { kind: "heading", level: 1, text: "Title" },
      { kind: "paragraph", text: "First line second line" },
      { kind: "list", items: ["one", "two"] },
      { kind: "heading", level: 3, text: "Small" },
    ]);
  });

  test("inline marks render as elements and HTML stays text", () => {
    render(
      <Markdown
        source={"Read **this** and *that*, run `pnpm build`.\n\n<img src=x onerror=alert(1)>"}
      />,
    );
    expect(screen.getByText("this").tagName).toBe("STRONG");
    expect(screen.getByText("that").tagName).toBe("EM");
    expect(screen.getByText("pnpm build").tagName).toBe("CODE");
    expect(screen.getByText("<img src=x onerror=alert(1)>").tagName).toBe("P");
    expect(document.querySelector("img")).toBeNull();
  });

  test("only http and https links become links", () => {
    render(<Markdown source={"[docs](https://example.com/a) and [bad](javascript:alert(1))"} />);
    const link = screen.getByRole("link", { name: "docs" });
    expect(link.getAttribute("href")).toBe("https://example.com/a");
    expect(link.getAttribute("rel")).toBe("noreferrer noopener");
    expect(screen.queryByRole("link", { name: "bad" })).toBeNull();
    expect(screen.getByText(/\[bad\]\(javascript:alert\(1\)\)/)).toBeTruthy();
    expect(safeHref("mailto:a@b.c")).toBeNull();
    expect(safeHref("not a url")).toBeNull();
  });

  test("headings sit below the widget's own title", () => {
    render(<Markdown source={"# One\n## Two\n### Three"} />);
    expect(screen.getByRole("heading", { name: "One" }).tagName).toBe("H3");
    expect(screen.getByRole("heading", { name: "Three" }).tagName).toBe("H5");
  });
});
