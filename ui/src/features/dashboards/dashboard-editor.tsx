"use client";

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import {
  Drawer,
  DrawerBackdrop,
  DrawerContent,
  DrawerDescription,
  DrawerTitle,
} from "@ui-sdk/react/drawer";
import { Field, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { type CSSProperties, useId, useRef, useState } from "react";
import { getDashboard, patchDashboard } from "@/api/client";
import { asUiError, type UiError } from "@/api/errors";
import type { Dashboard } from "@/api/generated/Dashboard";
import type { DashboardPatch } from "@/api/generated/DashboardPatch";
import type { SchemaMeasure } from "@/api/generated/SchemaMeasure";
import type { WidgetInput } from "@/api/generated/WidgetInput";
import type { WidgetLayout } from "@/api/generated/WidgetLayout";
import { EmptyState } from "@/components/empty-state";
import { FormAlert, failureCopy } from "@/components/form-alert";
import { Section, SectionFrame } from "@/components/section";
import { useProject } from "@/lib/project";
import { useTimeRange } from "@/lib/time-range";
import type { GridBoxes } from "@/primitives/dashboard-grid/dashboard-grid";
import {
  DashboardGrid,
  DashboardGridBox,
  DashboardGridHandle,
} from "@/primitives/dashboard-grid/dashboard-grid-parts";
import { DuplicateButton } from "./dashboard-list";
import {
  byReadingOrder,
  firstFree,
  gridHeight,
  KIND_LABEL,
  MAX_DESCRIPTION,
  MAX_NAME,
  MAX_WIDGETS,
  minSize,
  overlaps,
  startSize,
} from "./rules";
import {
  dashboardHref,
  dashboardKeys,
  useDashboardAccess,
  useMetricsSchema,
  useWide,
} from "./shared";
import { catalogOf, measureLabel } from "./widget-data";
import { WidgetForm, type WidgetDraft } from "./widget-form";

interface DraftWidget extends WidgetDraft {
  /** The stored widget's id, or a key for a widget not saved yet. */
  key: string;
  stored: boolean;
  layout: WidgetLayout;
}

interface Draft {
  name: string;
  description: string;
  widgets: DraftWidget[];
}

function draftOf(dashboard: Dashboard): Draft {
  return {
    name: dashboard.name,
    description: dashboard.description,
    widgets: dashboard.widgets.map((w) => ({
      key: w.id,
      stored: true,
      query: w.query,
      display: w.display,
      layout: w.layout,
    })),
  };
}

/** The draft as the server stores it: the widgets of `kept` keep their ids, the others are new. */
function body(draft: Draft, kept: (id: string) => boolean): DashboardPatch {
  const widgets: WidgetInput[] = draft.widgets
    .map((w) => ({ id: w.key, layout: w.layout, widget: w }))
    .toSorted(byReadingOrder)
    .map(({ widget: w }) => ({
      id: w.stored && kept(w.key) ? w.key : undefined,
      query: w.query,
      display: w.display,
      layout: w.layout,
    }));
  return { name: draft.name.trim(), description: draft.description, widgets };
}

const sameDraft = (a: Draft, b: Draft) => JSON.stringify(a) === JSON.stringify(b);

const placed = (widgets: DraftWidget[]) =>
  widgets.map((x) => ({ id: x.key, kind: x.display.kind, layout: x.layout }));

/** What a box shows of its widget: the kind, the measures by their labels, and the grouping. */
function summaryOf(
  w: DraftWidget,
  catalogFor: (source: string) => readonly SchemaMeasure[],
): string {
  if (!w.query) return KIND_LABEL[w.display.kind];
  const catalog = catalogFor(w.query.source);
  const measures = w.query.measures
    .map(
      (m) =>
        `${measureLabel(catalog, m.measure)}${m.aggregation === "sum" ? "" : ` ${m.aggregation}`}`,
    )
    .join(", ");
  const grouping = w.query.dimensions?.length ? ` by ${w.query.dimensions.join(" and ")}` : "";
  return `${KIND_LABEL[w.display.kind]} of ${measures}${grouping}`;
}

export function DashboardEditor() {
  const search = useSearchParams();
  const id = search.get("id");
  const project = useProject();
  const { signedIn, canEdit } = useDashboardAccess(project);
  const wide = useWide();
  let refusal: { title: string; body: string } | null = null;
  if (!id)
    refusal = { title: "This link doesn't name a dashboard.", body: "Open one from the list." };
  else if (!signedIn) {
    refusal = {
      title: "Editing dashboards needs a control plane and a sign-in.",
      body: "Dashboards are stored in Postgres. Set postgres.url (CRUCIBLE_POSTGRES_URL), then sign in.",
    };
  } else if (!canEdit) {
    refusal = {
      title: "Your role can't edit dashboards.",
      body: "Members and admins of the project edit dashboards. Ask a project admin.",
    };
  }
  if (refusal || !id) {
    return (
      <SectionFrame title="Dashboard">
        <EmptyState kind="unavailable" title={refusal?.title ?? ""}>
          {refusal?.body}
        </EmptyState>
      </SectionFrame>
    );
  }
  if (!wide) {
    return (
      <SectionFrame title="Dashboard">
        <EmptyState kind="unavailable" title="Editing needs a wider screen.">
          Open this page on a screen at least 768 pixels wide, or{" "}
          <Link href={dashboardHref(id, search)}>view the dashboard</Link>.
        </EmptyState>
      </SectionFrame>
    );
  }
  return <Loader project={project} id={id} />;
}

function Loader({ project, id }: { project: string; id: string }) {
  const query = useQuery({
    queryKey: dashboardKeys.one(project, id),
    queryFn: ({ signal }) => getDashboard(project, id, signal),
  });
  return (
    <Section
      title="Layout"
      label="the dashboard"
      query={query}
      skeleton={<div className="skeleton h-64 w-full" />}
      isEmpty={() => false}
      empty={null}
    >
      {(dashboard) =>
        dashboard.builtin_key !== undefined ? (
          <EmptyState
            kind="unavailable"
            title="Built-in dashboards are read-only."
            action={<DuplicateButton project={project} dashboard={dashboard} />}
          >
            Duplicate it, then change the copy.
          </EmptyState>
        ) : (
          <Editor key={dashboard.id} project={project} loaded={dashboard} />
        )
      }
    </Section>
  );
}

type Outcome =
  | { kind: "saved" }
  | { kind: "conflict" }
  | { kind: "deleted" }
  | { kind: "failed"; error: UiError };

function Editor({ project, loaded }: { project: string; loaded: Dashboard }) {
  const queryClient = useQueryClient();
  const search = useSearchParams();
  const [range] = useTimeRange(project);
  const schema = useMetricsSchema(project);
  const [base, setBase] = useState(loaded);
  const [draft, setDraft] = useState(() => draftOf(loaded));
  const [editing, setEditing] = useState<string | null>(() => {
    const widget = search.get("widget");
    return widget && loaded.widgets.some((w) => w.id === widget) ? widget : null;
  });
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const added = useRef(0);
  const dirty = !sameDraft(draft, draftOf(base));

  const adopt = (saved: Dashboard) => {
    queryClient.setQueryData(dashboardKeys.one(project, saved.id), saved);
    void queryClient.invalidateQueries({ queryKey: dashboardKeys.list(project) });
    setBase(saved);
    setDraft(draftOf(saved));
  };
  const save = useMutation({
    mutationFn: async (over: boolean) => {
      // Saving over someone else's change starts from their version; widgets they deleted come back as new.
      const target = over ? await getDashboard(project, base.id) : base;
      const ids = new Set(target.widgets.map((w) => w.id));
      return patchDashboard(
        project,
        target,
        body(draft, (id) => ids.has(id)),
      );
    },
    onSuccess: (saved) => {
      adopt(saved);
      setOutcome({ kind: "saved" });
    },
    onError: (error) => {
      const failed = asUiError(error);
      if (failed.code === "precondition_failed") setOutcome({ kind: "conflict" });
      else if (failed.kind === "not_found") setOutcome({ kind: "deleted" });
      else setOutcome({ kind: "failed", error: failed });
    },
  });
  const reload = useMutation({
    mutationFn: () => getDashboard(project, base.id),
    onSuccess: (latest) => {
      adopt(latest);
      setOutcome(null);
    },
  });

  const change = (next: (d: Draft) => Draft) => {
    setDraft(next);
    if (outcome?.kind === "saved") setOutcome(null);
  };
  const editingWidget = draft.widgets.find((w) => w.key === editing);
  const apply = (result: WidgetDraft) => {
    const key = editing === "new" ? `new-${(added.current += 1)}` : editing;
    change((d) => {
      if (editing === "new") {
        const layout = firstFree(placed(d.widgets), startSize(result.display.kind));
        if (!layout) return d;
        return {
          ...d,
          widgets: [...d.widgets, { key: key ?? "", stored: false, ...result, layout }],
        };
      }
      return {
        ...d,
        widgets: d.widgets.map((w) => {
          if (w.key !== key) return w;
          // A kind whose smallest size is larger grows the widget in place, or moves it where it fits.
          const min = minSize(result.display.kind);
          const size = { w: Math.max(w.layout.w, min.w), h: Math.max(w.layout.h, min.h) };
          const grown = { ...w.layout, ...size };
          const others = placed(d.widgets.filter((o) => o.key !== w.key));
          const fits = grown.x + grown.w <= 12 && others.every((o) => !overlaps(o.layout, grown));
          return { ...w, ...result, layout: fits ? grown : (firstFree(others, size) ?? w.layout) };
        }),
      };
    });
    setEditing(null);
  };

  const boxes: GridBoxes = Object.fromEntries(
    draft.widgets.map((w) => [
      w.key,
      { x: w.layout.x, y: w.layout.y, width: w.layout.w, height: w.layout.h },
    ]),
  );
  const limits = Object.fromEntries(
    draft.widgets.map((w) => {
      const min = minSize(w.display.kind);
      return [w.key, { width: min.w, height: min.h }];
    }),
  );
  const labels = Object.fromEntries(draft.widgets.map((w) => [w.key, w.display.title]));
  const rows = Math.max(gridHeight(placed(draft.widgets)) + 3, 6);
  const hint = useId();
  const catalogFor = (source: string) => catalogOf(schema.data, source);
  const full = draft.widgets.length >= MAX_WIDGETS;
  const nameBlank = draft.name.trim() === "";

  return (
    <div className="flex flex-col gap-4">
      <div className="grid gap-3 lg:grid-cols-2">
        <Field invalid={nameBlank || undefined}>
          <FieldLabel>Name</FieldLabel>
          <Input
            value={draft.name}
            onValueChange={(name) => change((d) => ({ ...d, name }))}
            maxLength={MAX_NAME}
            autoComplete="off"
          />
        </Field>
        <Field>
          <FieldLabel>Description</FieldLabel>
          <Input
            value={draft.description}
            onValueChange={(description) => change((d) => ({ ...d, description }))}
            maxLength={MAX_DESCRIPTION}
            autoComplete="off"
          />
        </Field>
      </div>
      <div className="flex flex-wrap items-center gap-3">
        <Button onClick={() => setEditing("new")} aria-disabled={full || undefined} disabled={full}>
          Add widget
        </Button>
        <Button
          data-variant="solid"
          onClick={() => save.mutate(false)}
          disabled={!dirty || nameBlank || save.isPending}
        >
          {save.isPending ? "Saving" : "Save"}
        </Button>
        <Button onClick={() => setDraft(draftOf(base))} disabled={!dirty || save.isPending}>
          Discard changes
        </Button>
        <Link href={dashboardHref(base.id, search)} className="text-data">
          View dashboard
        </Link>
        <p className="text-label text-ink-soft" aria-live="polite">
          {full
            ? "This dashboard has 50 widgets, the most it can hold."
            : nameBlank
              ? "Give the dashboard a name to save it."
              : dirty
                ? "Unsaved changes."
                : outcome?.kind === "saved"
                  ? "Saved."
                  : "No changes."}
        </p>
      </div>
      {outcome?.kind === "conflict" ? (
        <FormAlert
          copy={{
            title: "Someone changed this dashboard since you opened it.",
            detail: "Load their version, which drops your changes, or save yours over it.",
          }}
          action={
            <div className="flex gap-2">
              <Button onClick={() => reload.mutate()} disabled={reload.isPending}>
                {reload.isPending ? "Loading" : "Load their version"}
              </Button>
              <Button onClick={() => save.mutate(true)} disabled={save.isPending}>
                Save mine over theirs
              </Button>
            </div>
          }
        />
      ) : null}
      {outcome?.kind === "deleted" ? (
        <FormAlert
          copy={{
            title: "This dashboard was deleted.",
            detail: "Someone deleted it while you edited it. Your changes can't be saved to it.",
          }}
        />
      ) : null}
      {outcome?.kind === "failed" ? (
        <FormAlert
          copy={failureCopy(outcome.error, "The dashboard couldn't be saved.")}
          requestId={outcome.error.requestId}
        />
      ) : null}
      <p id={hint} className="text-label text-ink-soft">
        Drag a widget by its handle, or focus the handle and use the arrow keys to move it a cell.
        The corner handle resizes it the same way. Widgets never overlap.
      </p>
      {draft.widgets.length === 0 ? (
        <EmptyState kind="first-use" title="No widgets yet.">
          Add a chart, a number, a table, a breakdown or a note with Add widget.
        </EmptyState>
      ) : (
        <div data-dashboard>
          <DashboardGrid
            aria-label={`Layout of ${draft.name || "the dashboard"}`}
            value={boxes}
            limits={limits}
            labels={labels}
            onValueChange={(next) =>
              change((d) => ({
                ...d,
                widgets: d.widgets.map((w) => {
                  const box = next[w.key];
                  return box
                    ? { ...w, layout: { x: box.x, y: box.y, w: box.width, h: box.height } }
                    : w;
                }),
              }))
            }
            style={{ "--grid-rows": rows } as CSSProperties}
          >
            {draft.widgets.map((w) => (
              <DashboardGridBox
                key={w.key}
                value={w.key}
                data-widget={w.key}
                className="flex flex-col gap-1 overflow-hidden rounded-xl border border-ash-border bg-warm-sand p-2"
              >
                <div className="flex items-center gap-2">
                  <DashboardGridHandle
                    id={`${w.key}-move`}
                    box={w.key}
                    handle="move"
                    aria-label={`Move ${w.display.title}`}
                    aria-describedby={hint}
                    className="shrink-0 cursor-move rounded px-1 text-ink-soft"
                  >
                    <svg width="12" height="16" viewBox="0 0 12 16" aria-hidden="true">
                      <path
                        d="M3 3h1M8 3h1M3 8h1M8 8h1M3 13h1M8 13h1"
                        stroke="currentColor"
                        strokeWidth="2"
                        strokeLinecap="round"
                      />
                    </svg>
                  </DashboardGridHandle>
                  <p
                    className="min-w-0 grow truncate text-data font-medium"
                    title={w.display.title}
                  >
                    {w.display.title}
                  </p>
                </div>
                <p className="truncate ps-6 text-label text-ink-soft">{summaryOf(w, catalogFor)}</p>
                <div className="mt-auto flex flex-wrap gap-2 ps-6 pe-5">
                  <Button
                    aria-label={`Configure ${w.display.title}`}
                    onClick={() => setEditing(w.key)}
                  >
                    Configure
                  </Button>
                  <Button
                    aria-label={`Delete ${w.display.title}`}
                    onClick={() =>
                      change((d) => ({ ...d, widgets: d.widgets.filter((x) => x.key !== w.key) }))
                    }
                  >
                    Delete
                  </Button>
                </div>
                <DashboardGridHandle
                  id={`${w.key}-se`}
                  box={w.key}
                  handle="se"
                  aria-label={`Resize ${w.display.title}`}
                  aria-describedby={hint}
                  className="absolute end-1 bottom-1 size-4 cursor-se-resize rounded text-ink-soft"
                >
                  <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
                    <path
                      d="M11 3L3 11M11 7L7 11"
                      stroke="currentColor"
                      strokeWidth="1.5"
                      strokeLinecap="round"
                    />
                  </svg>
                </DashboardGridHandle>
              </DashboardGridBox>
            ))}
          </DashboardGrid>
        </div>
      )}
      <Drawer
        side="right"
        preventEdgeSwipe
        open={editing !== null}
        onOpenChange={(open) => {
          if (!open) setEditing(null);
        }}
      >
        <DrawerBackdrop className="bg-midnight-ink/20" />
        <DrawerContent className="flex h-full w-full max-w-xl flex-col gap-4 overflow-y-auto border-s border-ash-border bg-parchment-white p-5">
          <div className="flex flex-col gap-1">
            <DrawerTitle>{editing === "new" ? "Add a widget" : "Configure the widget"}</DrawerTitle>
            <DrawerDescription>
              The preview reads the time range at the top of the page.
            </DrawerDescription>
          </div>
          {editing !== null ? (
            <WidgetForm
              key={editing}
              project={project}
              schema={schema.data}
              range={range}
              initial={
                editingWidget ?? {
                  query: { source: "spans", measures: [{ measure: "count", aggregation: "sum" }] },
                  display: { kind: "timeseries", title: "", chart: "line" },
                }
              }
              onApply={apply}
              onCancel={() => setEditing(null)}
            />
          ) : null}
        </DrawerContent>
      </Drawer>
    </div>
  );
}
