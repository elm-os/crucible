import {
  announce,
  type ChangeDetails,
  createChange,
  createResize,
  definePrimitive,
  live,
  prepareAnnouncements,
  type ResizeBox,
  type ResizeDraft,
  type ResizeHandle,
  type ResizeReason,
} from "@ui-sdk/core";

// The layout editor of a dashboard: boxes on a grid of 12 columns, in cells, that move and resize by pointer
// or keyboard and never overlap. A move handle takes the arrows to move its box one cell; a corner handle
// takes them to resize it. A change that would overlap another box or leave the grid is refused and said
// out loud. The grid's pitch, the width of a column with its gap, is the scale of a pointer drag.

export const COLUMNS = 12;
export const ROWS = 600;
export const GAP_PX = 12;

export type GridBoxes = Record<string, ResizeBox>;

type DashboardGridOptions = {
  value?: GridBoxes;
  defaultValue?: GridBoxes;
  onValueChange?: (value: GridBoxes, details: ChangeDetails<ResizeReason>) => void;
  /** The smallest width and height of each box, in cells. */
  limits?: Record<string, { width: number; height: number }>;
  /** The name screen readers hear for each box. */
  labels?: Record<string, string>;
};

const meet = (a0: number, aLen: number, b0: number, bLen: number) =>
  Math.max(a0, b0) < Math.min(a0 + aLen, b0 + bLen);

/** Whether no two boxes share a cell. */
export function disjoint(boxes: GridBoxes): boolean {
  const all = Object.values(boxes);
  return all.every((a, i) =>
    all
      .slice(i + 1)
      .every((b) => !(meet(a.x, a.width, b.x, b.width) && meet(a.y, a.height, b.y, b.height))),
  );
}

const ARROWS: Record<string, [number, number]> = {
  ArrowLeft: [-1, 0],
  ArrowRight: [1, 0],
  ArrowUp: [0, -1],
  ArrowDown: [0, 1],
};

/** The width of one column and its gap, in pixels: what one cell of a pointer drag is worth. */
function pitch(root: HTMLElement | null | undefined): number {
  const width = root?.clientWidth ?? 0;
  return width > 0 ? (width + GAP_PX) / COLUMNS : 80;
}

const dashboardGrid = definePrimitive({
  name: "dashboard-grid",
  options: {
    value: "value",
    defaultValue: "value",
    onValueChange: "callback",
    limits: "value",
    labels: "value",
  },
  controlled: ["value"],
  state: (options: DashboardGridOptions) => ({
    value: options.value ?? options.defaultValue ?? {},
    draft: null as ResizeDraft | null,
    limits: options.limits ?? {},
    labels: options.labels ?? {},
  }),
  live: ["draft"],
  sync: (options) => ({
    ...(options.value === undefined ? {} : { value: options.value }),
    limits: options.limits ?? {},
    labels: options.labels ?? {},
  }),
  api: (context) => {
    const change = createChange(context);
    const label = (key: string) => context.store.get().labels[key] ?? key;
    // Whether the last change asked for went through; a key that asks for none leaves it null.
    let applied: boolean | null = null;
    const resize = createResize(context, {
      boxes: (state) => state.value,
      draft: "draft",
      set: (value, details) => {
        applied = disjoint(value) && change("value", value, details);
        return applied;
      },
      min: (key) => context.store.get().limits[key] ?? { width: 1, height: 1 },
      max: () => ({ width: COLUMNS, height: COLUMNS }),
      bounds: () => ({ left: 0, top: 0, right: COLUMNS, bottom: ROWS }),
      step: () => 1,
      scale: () => pitch(context.element("root")),
      label,
    });
    const refused = (key: string, move: boolean) =>
      announce(
        move
          ? `${label(key)} can't move there: another widget or the edge of the grid is in the way.`
          : `${label(key)} can't take that size: another widget, the edge of the grid or its smallest size is in the way.`,
      );

    return {
      resize,
      keydown(event: KeyboardEvent, key: string, handle: ResizeHandle): boolean {
        const arrow = ARROWS[event.key];
        if (!arrow) return false;
        event.preventDefault();
        applied = null;
        if (handle === "move") {
          resize.resizeBy(key, "move", arrow[0], arrow[1], { reason: "resize-key", event });
        } else {
          resize.keydown(event, key, handle);
        }
        if (applied !== true) refused(key, handle === "move");
        return true;
      },
      /** Moves a box to a column and row, when the place is free. */
      moveWidget(key: string, x: number, y: number): boolean {
        const box = context.store.get().value[key];
        if (!box) return false;
        return resize.resizeTo(key, { ...box, x, y }, { reason: "imperative" }, "move");
      },
      /** Gives a box a width and height in cells, when the room is free. */
      resizeWidget(key: string, width: number, height: number): boolean {
        const box = context.store.get().value[key];
        const min = context.store.get().limits[key] ?? { width: 1, height: 1 };
        if (!box || width < min.width || height < min.height) return false;
        if (width > COLUMNS || height > COLUMNS || box.x + width > COLUMNS) return false;
        return resize.resizeTo(key, { ...box, width, height }, { reason: "imperative" }, "se");
      },
    };
  },
  slots: (slot) => ({
    // A refused move is announced at once, so the live regions exist before the first one.
    root: slot({ bind: (el) => prepareAnnouncements(el.ownerDocument) }),
    box: slot<{ value: string }>({
      props: { value: "string" },
      attrs: (state, { key }) => {
        const box = state.value[key];
        return {
          style: box
            ? `--box-x: ${box.x}; --box-y: ${box.y}; --box-w: ${box.width}; --box-h: ${box.height}`
            : undefined,
        };
      },
      live: live((state, { key, api }) => {
        const dragged = api.resize.dragging(state, key) ? api.resize.box(state, key) : null;
        return {
          "data-dragging": dragged ? "" : undefined,
          style: dragged
            ? `--drag-x: ${dragged.x}; --drag-y: ${dragged.y}; --drag-w: ${dragged.width}; --drag-h: ${dragged.height}`
            : undefined,
        };
      }),
    }),
    handle: slot<{ id: string; box: string; handle: ResizeHandle }>({
      props: { id: "string", box: "string", handle: "string" },
      attrs: (_state, { props, api }) => api.resize.handleAttrs(props.handle),
      on: {
        keydown: (event, { api, props }) => {
          api.keydown(event as KeyboardEvent, props.box, props.handle);
        },
        pointerdown: (event, { el, api, props }) => {
          if (el) api.resize.pointerdown(event as PointerEvent, props.box, props.handle, el);
        },
      },
    }),
  }),
  agent: { actions: ["moveWidget", "resizeWidget"] },
});

export { dashboardGrid, type DashboardGridOptions };
