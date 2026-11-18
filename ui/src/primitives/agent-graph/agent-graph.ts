import {
  type ChangeDetails,
  createChange,
  createRovingFocus,
  createTypeahead,
  createViewport,
  definePrimitive,
  elements,
  isTypeaheadKey,
  labelOf,
  live,
} from "@ui-sdk/core";

// An APG listbox of graph nodes on a canvas you pan and zoom. Options follow render order, so arrows and
// typeahead keep the listbox contract; choosing the chosen node again clears the choice.

type AgentGraphOptions = {
  value?: string | null;
  defaultValue?: string | null;
  onValueChange?: (
    value: string | null,
    details: ChangeDetails<"item-press" | "imperative">,
  ) => void;
  // The laid-out graph's size in canvas units, which the view fits when it changes.
  width?: number;
  height?: number;
};

const agentGraph = definePrimitive({
  name: "agent-graph",
  options: {
    value: "value",
    defaultValue: "value",
    onValueChange: "callback",
    width: "number",
    height: "number",
  },
  controlled: ["value"],
  state: (options: AgentGraphOptions) => ({
    value: options.value ?? options.defaultValue ?? null,
    focused: null as string | null,
    viewport: { x: 0, y: 0, zoom: 1 },
  }),
  live: ["viewport"],
  sync: (options) => (options.value === undefined ? {} : { value: options.value }),
  api: (context) => {
    const change = createChange(context);
    const viewport = createViewport(context, {
      key: "viewport",
      surface: () => context.element("root"),
      maxZoom: () => 2,
    });
    const roving = createRovingFocus(context, {
      slot: "node",
      key: "focused",
      orientation: () => "both",
    });
    const typeahead = createTypeahead();

    const fit = () => {
      const surface = context.element("root");
      const { width = 0, height = 0 } = context.options();
      if (!surface || surface.clientWidth === 0 || width === 0) return;
      viewport.fit({ left: 0, top: 0, right: width, bottom: height }, { padding: 16, maxZoom: 1 });
    };
    let size = `${context.options().width}x${context.options().height}`;
    context.onOptions((options) => {
      const next = `${options.width}x${options.height}`;
      if (next === size) return;
      size = next;
      fit();
    });

    const choose = (key: string, event?: Event) =>
      change("value", context.store.get().value === key ? null : key, {
        reason: "item-press",
        event,
      });

    return {
      roving,
      viewport,
      fit,
      choose,
      // The nodes an agent can choose from, in page order, by their label or else their text.
      items: () =>
        elements<{ label?: string }>(context, "node").map(({ key, el, props }) => ({
          value: key,
          label: props.label ?? labelOf(el),
        })),
      setValue: (value: string | null) => change("value", value, { reason: "imperative" }),
      keydown(event: KeyboardEvent, el: HTMLElement, key: string) {
        if (roving.keydown(event, el)) return;
        // Space continues a name being typed; otherwise it chooses, like Enter.
        if (isTypeaheadKey(event) && (event.key !== " " || typeahead.active())) {
          const items = roving.items();
          const index = typeahead(
            event.key,
            items.map((item) => labelOf(item.el)),
            items.findIndex((item) => item.el === el),
          );
          const found = items[index];
          if (found) {
            event.preventDefault();
            roving.focus(found.key);
          }
          return;
        }
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          choose(key, event);
        }
      },
    };
  },
  slots: (slot) => ({
    root: slot({
      role: "listbox",
      keys: { none: "Each node handles the arrow keys and typing." },
      on: {
        wheel: (event, { api }) => api.viewport.wheel(event as WheelEvent),
        pointerdown: (event, { api }) => {
          const target = event.target as Element | null;
          if (!target?.closest('[data-slot="agent-graph-node"]')) {
            api.viewport.pointerdown(event as PointerEvent);
          }
        },
        keydown: (event, { api }) => {
          if (api.viewport.keydown(event as KeyboardEvent)) event.preventDefault();
        },
      },
      bind: (_el, { api }) => api.fit(),
    }),
    world: slot({
      live: live((state, { api }) => ({ style: api.viewport.style(state) })),
    }),
    node: slot<{ value: string; label?: string }>({
      role: "option",
      focus: "roving",
      props: { value: "string", label: "string" },
      press: ({ api, key }, event) => api.choose(key, event),
      attrs: (state, part) => ({
        "aria-selected": String(state.value === part.key),
        tabindex: part.api.roving.tabIndex(state, part),
        "data-selected": state.value === part.key ? "" : undefined,
      }),
      on: {
        keydown: (event, { el, key, api }) => {
          if (el) api.keydown(event as KeyboardEvent, el, key);
        },
        focus: (_event, { el, api }) => {
          if (el) api.viewport.reveal(api.viewport.rectOf(el), { padding: 24 });
        },
      },
      bind: (el, part) => part.api.roving.bind(el, part),
    }),
  }),
  agent: { actions: ["setValue"], closed: true, items: (api) => api.items() },
});

export { agentGraph, type AgentGraphOptions };
