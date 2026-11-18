import { checkDirection, checkPattern, checkServer, mount } from "@ui-sdk/core/testing";
import { expect, test } from "vitest";
import { agentGraph } from "./agent-graph";

// The nodes in flow order: a planner, the tool and model it calls, and a reviewer.
const GRAPH = `<root aria-label="Agent graph"><world>
  <node value="planner">Agent planner</node>
  <node value="search">Tool search</node>
  <node value="model">Model gpt-x</node>
  <node value="review">Agent reviewer</node>
</world></root>`;

const focused = () => document.activeElement?.textContent;

test("one tab stop; arrows step through the nodes in flow order, Home and End to the ends", async () => {
  const view = mount(agentGraph, {}, GRAPH);
  try {
    const nodes = [...view.root.querySelectorAll('[data-slot="agent-graph-node"]')];
    expect(nodes.filter((node) => node.getAttribute("tabindex") === "0")).toHaveLength(1);
    await view.keys("Tab");
    expect(focused()).toBe("Agent planner");
    await view.keys("ArrowDown");
    expect(focused()).toBe("Tool search");
    await view.keys("ArrowRight");
    expect(focused()).toBe("Model gpt-x");
    await view.keys("ArrowUp");
    expect(focused()).toBe("Tool search");
    await view.keys("ArrowLeft");
    expect(focused()).toBe("Agent planner");
    await view.keys("End");
    expect(focused()).toBe("Agent reviewer");
    await view.keys("Home");
    expect(focused()).toBe("Agent planner");
    expect(nodes.filter((node) => node.getAttribute("tabindex") === "0")).toHaveLength(1);
    expect(view.state.value).toBeNull();
  } finally {
    view.unmount();
  }
});

test("typing a name moves to the node it starts", async () => {
  const view = mount(agentGraph, {}, GRAPH);
  try {
    await view.keys("Tab", "t");
    expect(focused()).toBe("Tool search");
    expect(view.state.value).toBeNull();
  } finally {
    view.unmount();
  }
});

test("Enter and Space choose the focused node; choosing it again clears the choice", async () => {
  const view = mount(agentGraph, {}, GRAPH);
  try {
    await view.keys("Tab", "ArrowRight", "Enter");
    expect(view.state.value).toBe("search");
    expect(view.part("node", "search")?.getAttribute("aria-selected")).toBe("true");
    expect(view.part("node", "search")?.hasAttribute("data-selected")).toBe(true);
    expect(view.part("node", "planner")?.getAttribute("aria-selected")).toBe("false");
    expect(view.changes.at(-1)).toMatchObject({
      key: "value",
      reason: "item-press",
      source: "keyboard",
    });
    await view.keys("Space");
    expect(view.state.value).toBeNull();
    await view.keys("ArrowRight", "Space");
    expect(view.state.value).toBe("model");
  } finally {
    view.unmount();
  }
});

test("a press chooses a node, and the controlled value follows the owner", async () => {
  const chosen: (string | null)[] = [];
  const view = mount(agentGraph, { onValueChange: (value) => chosen.push(value) }, GRAPH);
  try {
    const review = view.part("node", "review");
    if (!review) throw new Error("no reviewer node");
    await view.click(review);
    expect(view.state.value).toBe("review");
    expect(chosen).toEqual(["review"]);
    expect(view.changes.at(-1)).toMatchObject({ reason: "item-press", source: "mouse" });
  } finally {
    view.unmount();
  }
});

test("+ and - zoom the canvas from any node, and 0 returns it to its size", async () => {
  const view = mount(agentGraph, {}, GRAPH);
  try {
    // `keys` reads "+" as the join of a combination, so the zoom keys are dispatched as typed.
    const press = async (key: string) => {
      document.activeElement?.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
      await view.settle();
    };
    await view.keys("Tab");
    await press("+");
    expect(view.state.viewport.zoom).toBeGreaterThan(1);
    await press("0");
    expect(view.state.viewport.zoom).toBe(1);
    await press("-");
    expect(view.state.viewport.zoom).toBeLessThan(1);
  } finally {
    view.unmount();
  }
});

test("an agent's items are the nodes in page order, by label or else by text", () => {
  const view = mount(
    agentGraph,
    {},
    `<root aria-label="Agent graph"><world>
      <node value="planner" label="Agent planner">Agent planner 2 spans</node>
      <node value="search">Tool search</node>
    </world></root>`,
  );
  try {
    expect(view.api.items()).toEqual([
      { value: "planner", label: "Agent planner" },
      { value: "search", label: "Tool search" },
    ]);
  } finally {
    view.unmount();
  }
});

test("the agent's setValue chooses or clears a node", () => {
  const view = mount(agentGraph, {}, GRAPH);
  try {
    view.api.setValue("model");
    expect(view.state.value).toBe("model");
    view.api.setValue(null);
    expect(view.state.value).toBeNull();
  } finally {
    view.unmount();
  }
});

test("it follows the listbox pattern", async () => {
  const view = mount(agentGraph, {}, GRAPH);
  try {
    await checkPattern(view.root, "listbox");
  } finally {
    view.unmount();
  }
});

test("it renders on the server, and its arrows mirror in right-to-left text", async () => {
  expect(checkServer(agentGraph, {}, GRAPH)).toContain('role="listbox"');
  await checkDirection(agentGraph, {}, GRAPH);
});
