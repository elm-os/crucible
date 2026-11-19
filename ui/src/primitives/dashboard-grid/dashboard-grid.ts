import {
  announce,
  bindDrag,
  type ChangeDetails,
  createChange,
  createResize,
  definePrimitive,
  dragThreshold,
  isRTL,
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
// Boxes are kept from the inline start, as the dashboard lays them out. The resize behavior works on the screen
// from the left, so right to left the boxes and the handles' edges are mirrored on the way in and out. It does
// not move a box from a pointer at this version, so the move handle's drag follows the pointer itself and
// commits through the same checks.

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

/** Whether every box lies inside the grid. */
function inside(boxes: GridBoxes): boolean {
  return Object.values(boxes).every(
    (b) => b.x >= 0 && b.y >= 0 && b.x + b.width <= COLUMNS && b.y + b.height <= ROWS,
  );
}

/** Whether no two boxes share a cell. */
export function disjoint(boxes: GridBoxes): boolean {
  const all = Object.values(boxes);
  return all.every((a, i) =>
    all
      .slice(i + 1)
      .every((b) => !(meet(a.x, a.width, b.x, b.width) && meet(a.y, a.height, b.y, b.height))),
  );
}

const FLIP: Record<ResizeHandle, ResizeHandle> = {
  n: "n",
  s: "s",
  e: "w",
  w: "e",
  ne: "nw",
  nw: "ne",
  se: "sw",
  sw: "se",
  move: "move",
};

const same = (a?: ResizeBox, b?: ResizeBox) =>
  a?.x === b?.x && a?.y === b?.y && a?.width === b?.width && a?.height === b?.height;

/** A box seen from the other side; mirroring twice gives the box back. */
const mirror = (box: ResizeBox): ResizeBox => ({ ...box, x: COLUMNS - box.x - box.width });

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
    /** A box being moved by the pointer: how far it has gone, in cells along the inline axis and down. */
    moving: null as { key: string; dx: number; dy: number } | null,
    limits: options.limits ?? {},
    labels: options.labels ?? {},
  }),
  live: ["draft", "moving"],
  sync: (options) => ({
    ...(options.value === undefined ? {} : { value: options.value }),
    limits: options.limits ?? {},
    labels: options.labels ?? {},
  }),
  api: (context) => {
    const change = createChange(context);
    const label = (key: string) => context.store.get().labels[key] ?? key;
    const rtl = () => isRTL(context.element("root"));
    const screen = (boxes: GridBoxes): GridBoxes =>
      rtl() ? Object.fromEntries(Object.entries(boxes).map(([k, b]) => [k, mirror(b)])) : boxes;
    const edge = (handle: ResizeHandle) => (rtl() ? FLIP[handle] : handle);
    const refused = (key: string, move: boolean) =>
      announce(
        move
          ? `${label(key)} can't move there: another widget or the edge of the grid is in the way.`
          : `${label(key)} can't take that size: another widget, the edge of the grid or its smallest size is in the way.`,
      );

    // Whether the last change asked for went through; a key that asks for none leaves it null.
    let applied: boolean | null = null;
    const resize = createResize(context, {
      boxes: (state) => screen(state.value),
      draft: "draft",
      set: (value, details) => {
        const boxes = screen(value);
        applied = inside(boxes) && disjoint(boxes) && change("value", boxes, details);
        // A key's refusal is said by the key handler; a refused drop is said here, as the box goes back.
        if (!applied && details.reason === "drag") {
          const now = context.store.get().value;
          const key = Object.keys(boxes).find((k) => !same(boxes[k], now[k]));
          const was = key ? now[key] : undefined;
          const sized = key ? boxes[key] : undefined;
          if (key) refused(key, was?.width === sized?.width && was?.height === sized?.height);
        }
        return applied;
      },
      min: (key) => context.store.get().limits[key] ?? { width: 1, height: 1 },
      max: () => ({ width: COLUMNS, height: COLUMNS }),
      bounds: () => ({ left: 0, top: 0, right: COLUMNS, bottom: ROWS }),
      step: () => 1,
      scale: () => pitch(context.element("root")),
      label,
    });
    let stop: (() => void) | undefined;
    context.onDestroy(() => stop?.());
    const moveDrag = (event: PointerEvent, key: string, el: HTMLElement) => {
      if (event.button !== 0) return;
      stop?.();
      const scale = pitch(context.element("root"));
      const sign = rtl() ? -1 : 1;
      const from = { x: event.clientX, y: event.clientY };
      stop = bindDrag(el, event, {
        threshold: dragThreshold,
        cancelOnEscape: true,
        onMove: ({ x, y }) =>
          context.store.set({
            moving: { key, dx: (sign * (x - from.x)) / scale, dy: (y - from.y) / scale },
          }),
        onEnd: (end, canceled) => {
          const moved = context.store.get().moving;
          context.store.set({ moving: null });
          const box = context.store.get().value[key];
          if (canceled || !moved || !box) return;
          const to = { ...box, x: box.x + Math.round(moved.dx), y: box.y + Math.round(moved.dy) };
          if (to.x === box.x && to.y === box.y) return;
          const [placed] = Object.values(screen({ [key]: to }));
          if (placed) resize.resizeTo(key, placed, { reason: "drag", event: end }, "move");
        },
      });
    };

    return {
      resize,
      rtl,
      edge,
      moveDrag,
      keydown(event: KeyboardEvent, key: string, handle: ResizeHandle): boolean {
        const arrow = ARROWS[event.key];
        if (!arrow) return false;
        event.preventDefault();
        applied = null;
        if (handle === "move") {
          resize.resizeBy(key, "move", arrow[0], arrow[1], { reason: "resize-key", event });
        } else {
          resize.keydown(event, key, edge(handle));
        }
        if (applied !== true) refused(key, handle === "move");
        return true;
      },
      /** Moves a box to a column and row, when the place is free. */
      moveWidget(key: string, x: number, y: number): boolean {
        const box = context.store.get().value[key];
        if (!box) return false;
        const [moved] = Object.values(screen({ [key]: { ...box, x, y } }));
        return moved ? resize.resizeTo(key, moved, { reason: "imperative" }, "move") : false;
      },
      /** Gives a box a width and height in cells, when the room is free. */
      resizeWidget(key: string, width: number, height: number): boolean {
        const box = context.store.get().value[key];
        const min = context.store.get().limits[key] ?? { width: 1, height: 1 };
        if (!box || width < min.width || height < min.height) return false;
        if (width > COLUMNS || height > COLUMNS || box.x + width > COLUMNS) return false;
        const [sized] = Object.values(screen({ [key]: { ...box, width, height } }));
        return sized ? resize.resizeTo(key, sized, { reason: "imperative" }, edge("se")) : false;
      },
    };
  },
  slots: (slot) => ({
    // A refused move is announced at once, so the live regions exist before the first one.
    root: slot({ role: "group", bind: (el) => prepareAnnouncements(el.ownerDocument) }),
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
        const seen = api.resize.dragging(state, key) ? api.resize.box(state, key) : null;
        const box = state.value[key];
        const moved =
          state.moving?.key === key && box
            ? { ...box, x: box.x + state.moving.dx, y: box.y + state.moving.dy }
            : null;
        const dragged = moved ?? (seen && api.rtl() ? mirror(seen) : seen);
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
      attrs: (_state, { props, api }) => api.resize.handleAttrs(api.edge(props.handle)),
      on: {
        keydown: (event, { api, props }) => {
          api.keydown(event as KeyboardEvent, props.box, props.handle);
        },
        pointerdown: (event, { el, api, props }) => {
          if (!el) return;
          if (props.handle === "move") api.moveDrag(event as PointerEvent, props.box, el);
          else api.resize.pointerdown(event as PointerEvent, props.box, api.edge(props.handle), el);
        },
      },
    }),
  }),
  agent: { actions: ["moveWidget", "resizeWidget"] },
});

export { dashboardGrid, type DashboardGridOptions };
