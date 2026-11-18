"use client";

import {
  Tree,
  TreeItem,
  type TreeItemData,
  TreeItemIndicator,
  TreeItemLabel,
  visibleTreeRows,
} from "@ui-sdk/react/tree";
import { useMemo, useRef, useState } from "react";
import { ChevronDownIcon } from "@/components/icons";
import type { JsonKind, JsonNode } from "./payload-model";

const ROW = 28;
const VIEW = 448;
const OVERSCAN = 8;

const KIND_CLASS: Record<JsonKind, string> = {
  object: "text-ink-soft",
  array: "text-ink-soft",
  string: "",
  number: "",
  boolean: "",
  null: "text-ink-soft",
};

function itemsOf(pointer: string, loaded: ReadonlyMap<string, JsonNode[]>): TreeItemData[] {
  return (loaded.get(pointer) ?? []).map((node) => ({
    value: node.id,
    label: `${node.key} ${node.preview}`,
    // An object or array whose children are not loaded yet is a branch with none.
    children: node.size > 0 ? itemsOf(node.id, loaded) : undefined,
  }));
}

// A JSON document as a tree whose branches load their children from the worker when they open. Only the
// rows in view are mounted.
export function JsonTree({
  label,
  top,
  load,
}: {
  label: string;
  top: JsonNode[];
  load: (pointer: string) => Promise<JsonNode[]>;
}) {
  const [loaded, setLoaded] = useState<ReadonlyMap<string, JsonNode[]>>(() => new Map([["", top]]));
  const [expanded, setExpanded] = useState<string[]>([]);
  const [failed, setFailed] = useState(false);
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);

  const nodes = useMemo(() => new Map([...loaded.values()].flat().map((n) => [n.id, n])), [loaded]);
  const items = useMemo(() => itemsOf("", loaded), [loaded]);
  const rows = useMemo(() => visibleTreeRows(items, expanded), [items, expanded]);
  const first = Math.max(0, Math.floor(scrollTop / ROW) - OVERSCAN);
  const last = Math.min(rows.length, Math.ceil((scrollTop + VIEW) / ROW) + OVERSCAN);

  const open = (next: string[]) => {
    setExpanded(next);
    for (const pointer of next) {
      if (loaded.has(pointer)) continue;
      load(pointer).then(
        (children) => setLoaded((map) => new Map(map).set(pointer, children)),
        () => setFailed(true),
      );
    }
  };

  return (
    <div className="flex flex-col gap-2">
      {failed ? (
        <p role="alert" className="text-label text-ink-soft">
          Part of this document couldn&apos;t be opened. Reload the page to try again.
        </p>
      ) : null}
      <div
        ref={scroller}
        onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}
        className="overflow-auto rounded-md border border-ash-border bg-parchment-white"
        style={{ height: Math.min(Math.max(rows.length, 1) * ROW, VIEW) + 2 }}
      >
        <Tree
          aria-label={label}
          items={items}
          expanded={expanded}
          onExpandedChange={open}
          onHighlight={(_value, { index }) => {
            const el = scroller.current;
            if (!el) return;
            const y = index * ROW;
            if (y < el.scrollTop) el.scrollTop = y;
            else if (y + ROW > el.scrollTop + el.clientHeight) {
              el.scrollTop = y + ROW - el.clientHeight;
            }
            setScrollTop(el.scrollTop);
          }}
          className="relative !max-w-none"
          style={{ height: rows.length * ROW }}
        >
          {rows.slice(first, last).map(({ item, index }) => {
            const node = nodes.get(item.value);
            if (!node) return null;
            return (
              <TreeItem
                key={item.value}
                value={item.value}
                className="absolute inset-x-0"
                style={{ top: index * ROW, height: ROW }}
              >
                <TreeItemLabel className="font-mono text-data">
                  {node.size > 0 ? (
                    <TreeItemIndicator>
                      <ChevronDownIcon />
                    </TreeItemIndicator>
                  ) : null}
                  <span className="shrink-0">{node.key}:</span>
                  <span className={KIND_CLASS[node.kind]}>{node.preview}</span>
                </TreeItemLabel>
              </TreeItem>
            );
          })}
        </Tree>
      </div>
    </div>
  );
}
