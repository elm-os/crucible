import { checkServer, mount } from "@ui-sdk/core/testing";
import { expect, test } from "vitest";
import { dashboardGrid, disjoint, type GridBoxes } from "./dashboard-grid";

const BOXES: GridBoxes = {
  a: { x: 0, y: 0, width: 4, height: 2 },
  b: { x: 6, y: 0, width: 4, height: 2 },
};

const MARKUP = `<root>
  <box value="a"><handle id="a-move" box="a" handle="move" tabindex="0">Move A</handle><handle id="a-se" box="a" handle="se" tabindex="0">Resize A</handle></box>
  <box value="b"><handle id="b-move" box="b" handle="move" tabindex="0">Move B</handle></box>
</root>`;

const options = {
  defaultValue: BOXES,
  limits: { a: { width: 3, height: 2 }, b: { width: 3, height: 2 } },
  labels: { a: "Spans", b: "Cost" },
};

test("arrows on the move handle move the box a cell, and a move onto another box is refused aloud", async () => {
  const view = mount(dashboardGrid, options, MARKUP);
  try {
    await view.keys("Tab", "ArrowRight");
    expect(view.state.value.a).toEqual({ x: 1, y: 0, width: 4, height: 2 });
    expect(view.changes.at(-1)).toMatchObject({
      key: "value",
      reason: "resize-key",
      source: "keyboard",
    });
    await view.keys("ArrowRight");
    expect(view.state.value.a?.x).toBe(2);
    // From column 2 a width of 4 reaches column 6, where the other box starts.
    await view.keys("ArrowRight");
    expect(view.state.value.a?.x).toBe(2);
    expect(view.announced.at(-1)).toMatch(/Spans can't move there/);
    // Two rows down clears the other box, which is two rows tall.
    await view.keys("ArrowDown", "ArrowDown", "ArrowRight");
    expect(view.state.value.a).toEqual({ x: 3, y: 2, width: 4, height: 2 });
    await view.keys("ArrowUp");
    expect(view.state.value.a?.y).toBe(2);
    expect(view.announced.at(-1)).toMatch(/can't move there/);
  } finally {
    view.unmount();
  }
});

test("arrows on the corner handle resize the box, never below its smallest size", async () => {
  const view = mount(dashboardGrid, options, MARKUP);
  try {
    await view.keys("Tab", "Tab", "ArrowDown");
    expect(view.state.value.a).toEqual({ x: 0, y: 0, width: 4, height: 3 });
    expect(view.announced.at(-1)).toMatch(/Spans resized to 4 by 3/);
    await view.keys("ArrowLeft", "ArrowLeft", "ArrowLeft");
    expect(view.state.value.a?.width).toBe(3);
    await view.keys("ArrowRight", "ArrowRight", "ArrowRight", "ArrowRight");
    expect(view.state.value.a?.width).toBe(6);
    expect(view.announced.at(-1)).toMatch(/can't take that size/);
    await view.keys("ArrowUp", "ArrowUp");
    expect(view.state.value.a?.height).toBe(2);
    expect(view.announced.at(-1)).toMatch(/smallest size/);
  } finally {
    view.unmount();
  }
});

test("an agent moves and resizes only into free room", () => {
  const view = mount(dashboardGrid, options, MARKUP);
  try {
    const api = view.api as unknown as {
      moveWidget: (key: string, x: number, y: number) => boolean;
      resizeWidget: (key: string, w: number, h: number) => boolean;
    };
    expect(api.moveWidget("a", 4, 0)).toBe(false);
    expect(api.moveWidget("a", 0, 3)).toBe(true);
    expect(view.state.value.a).toEqual({ x: 0, y: 3, width: 4, height: 2 });
    expect(api.resizeWidget("a", 2, 2)).toBe(false);
    expect(api.resizeWidget("a", 12, 4)).toBe(true);
    expect(api.resizeWidget("b", 13, 2)).toBe(false);
    expect(disjoint(view.state.value)).toBe(true);
  } finally {
    view.unmount();
  }
});

test("renders on the server", () => {
  checkServer(dashboardGrid, options, MARKUP);
});
