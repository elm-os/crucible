"use client";

import { useInfiniteQuery } from "@tanstack/react-query";
import { Button } from "@ui-sdk/react/button";
import { Checkbox, CheckboxGroup, CheckboxIndicator } from "@ui-sdk/react/checkbox";
import { Field, FieldDescription, FieldLabel } from "@ui-sdk/react/field";
import { Input } from "@ui-sdk/react/input";
import {
  SegmentedControl,
  SegmentedControlIndicator,
  SegmentedControlItem,
} from "@ui-sdk/react/segmented-control";
import type { Route } from "next";
import Link from "next/link";
import { type FormEvent, useEffect, useId, useState } from "react";
import { searchSpans } from "@/api/client";
import { asUiError } from "@/api/errors";
import type { SearchField } from "@/api/generated/SearchField";
import type { SearchHit } from "@/api/generated/SearchHit";
import type { SearchMatch } from "@/api/generated/SearchMatch";
import type { SearchMode } from "@/api/generated/SearchMode";
import type { SearchPage } from "@/api/generated/SearchPage";
import { EmptyState } from "@/components/empty-state";
import { TickIcon } from "@/components/icons";
import { PageHeader } from "@/components/page-header";
import { ErrorCard, Section, SectionFrame } from "@/components/section";
import { isWhole, MetaNotice } from "@/components/status";
import { TYPE_LABEL } from "@/features/trace/span-waterfall";
import { formatTime } from "@/lib/format";
import { useProject } from "@/lib/project";
import { RANGE_PHRASE, type Range, rangeWindow, useTimeRange } from "@/lib/time-range";
import { useUrlParam } from "@/lib/url-state";

const MODES: { value: SearchMode; label: string }[] = [
  { value: "all", label: "All words" },
  { value: "any", label: "Any word" },
  { value: "phrase", label: "Exact phrase" },
];

const FIELDS: { value: SearchField; label: string }[] = [
  { value: "input", label: "Input" },
  { value: "output", label: "Output" },
  { value: "metadata", label: "Metadata" },
];

const DEFAULT_FIELDS: SearchField[] = ["input", "output"];

// A page whose phrase candidates were all left out moves on by itself, this many times in a row.
const AUTO_PAGES = 4;

const SKELETON_ROWS = Array.from({ length: 6 }, (_, row) => `skeleton-row-${row}`);

function parseMode(value: string | null): SearchMode {
  return MODES.find((mode) => mode.value === value)?.value ?? "all";
}

function parseFields(value: string | null): SearchField[] {
  if (value === null) return DEFAULT_FIELDS;
  const chosen = FIELDS.map((f) => f.value).filter((field) => value.split(",").includes(field));
  return chosen.length > 0 ? chosen : DEFAULT_FIELDS;
}

export function SearchView() {
  const project = useProject();
  const [range, setRange] = useTimeRange(project);
  const [text, setText] = useUrlParam("q");
  const [modeParam, setMode] = useUrlParam("mode");
  const [fieldsParam, setFields] = useUrlParam("fields");
  const mode = parseMode(modeParam);
  const fields = parseFields(fieldsParam);
  const [draft, setDraft] = useState(text ?? "");
  const queryId = useId();

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const next = draft.trim();
    setText(next === "" ? null : next);
  };

  const query = text?.trim() ?? "";
  return (
    <>
      <PageHeader
        title="Search"
        description="Find spans by the words in their input and output, or by metadata entries."
      />
      <search aria-label="Spans" className="mb-4 block">
        <form
          onSubmit={submit}
          className="flex flex-col gap-3 rounded-xl border border-ash-border bg-warm-sand px-4 py-3"
        >
          <Field className="flex flex-col gap-1">
            <FieldLabel className="text-label font-medium text-ink-soft">Find</FieldLabel>
            <div className="flex flex-wrap gap-2">
              <Input
                id={queryId}
                type="search"
                value={draft}
                onValueChange={(value) => setDraft(value)}
                placeholder="refund policy"
                autoComplete="off"
                className="min-w-0 flex-1 rounded-md border border-control-edge bg-parchment-white px-3 py-1.5 text-body"
              />
              <Button type="submit">Search</Button>
            </div>
            <FieldDescription className="text-label text-ink-soft">
              Input and output match whole words, without regard to case. Metadata matches whole
              key=value entries, such as tenant=acme.
            </FieldDescription>
          </Field>
          <div className="flex flex-wrap items-center gap-x-6 gap-y-3">
            <SegmentedControl
              aria-label="Match"
              value={mode}
              onValueChange={(next) => setMode(next === "all" || next === null ? null : next)}
            >
              <SegmentedControlIndicator />
              {MODES.map((option) => (
                <SegmentedControlItem key={option.value} value={option.value} className="text-data">
                  {option.label}
                </SegmentedControlItem>
              ))}
            </SegmentedControl>
            <CheckboxGroup
              aria-label="Fields"
              value={fields}
              onValueChange={(next) => {
                const chosen = FIELDS.map((f) => f.value).filter((field) => next.includes(field));
                const isDefault = chosen.join() === DEFAULT_FIELDS.join();
                setFields(chosen.length === 0 || isDefault ? null : chosen.join(","));
              }}
              className="flex flex-wrap gap-4 text-data"
            >
              {FIELDS.map((field) => (
                <label key={field.value} className="flex items-center gap-1.5">
                  <Checkbox value={field.value}>
                    <CheckboxIndicator>
                      <TickIcon aria-hidden="true" />
                    </CheckboxIndicator>
                  </Checkbox>
                  {field.label}
                </label>
              ))}
            </CheckboxGroup>
          </div>
        </form>
      </search>
      {query === "" ? (
        <SectionFrame title="Results">
          <EmptyState kind="filtered" title="Type what to find.">
            Search covers {RANGE_PHRASE[range]}, the time range above. A search for words finds
            spans whose input or output holds each of them.
          </EmptyState>
        </SectionFrame>
      ) : (
        <SearchResults
          key={`${project}|${query}|${mode}|${fields.join()}|${range}`}
          project={project}
          query={query}
          mode={mode}
          fields={fields}
          range={range}
          onWiden={() => setRange("30d")}
        />
      )}
    </>
  );
}

function SearchResults({
  project,
  query: text,
  mode,
  fields,
  range,
  onWiden,
}: {
  project: string;
  query: string;
  mode: SearchMode;
  fields: SearchField[];
  range: Range;
  onWiden: () => void;
}) {
  // The window is fixed for a search's pages: their cursors are bound to it.
  const [timeWindow] = useState(() => rangeWindow(range, Date.now()));
  const query = useInfiniteQuery({
    queryKey: [project, "search", { text, mode, fields }, timeWindow.from, timeWindow.to],
    queryFn: ({ pageParam, signal }) =>
      searchSpans(
        project,
        { query: text, mode, fields, ...timeWindow, cursor: pageParam ?? undefined },
        signal,
      ),
    initialPageParam: null as string | null,
    getNextPageParam: (last: SearchPage) => (last.has_more ? last.next_cursor : null),
  });
  const pages = query.data?.pages ?? [];
  const hits = pages.flatMap((page) => page.data);
  const last = pages.at(-1);

  // Pages that came back without hits but with more to read are read on, a few in a row.
  const emptyRun = pages.length - 1 - pages.findLastIndex((page) => page.data.length > 0);
  const autoMore =
    query.hasNextPage &&
    !query.isFetching &&
    !query.isFetchNextPageError &&
    last !== undefined &&
    last.data.length === 0 &&
    emptyRun < AUTO_PAGES;
  const { fetchNextPage } = query;
  useEffect(() => {
    if (autoMore) void fetchNextPage();
  }, [autoMore, fetchNextPage]);

  return (
    <Section
      title="Results"
      query={query}
      skeleton={<ResultsSkeleton />}
      isEmpty={() =>
        hits.length === 0 && !query.hasNextPage && last !== undefined && isWhole(last.meta)
      }
      empty={
        range === "30d" ? (
          <EmptyState kind="filtered" title={`No spans match "${text}".`}>
            Try other words, another match, or more fields.
          </EmptyState>
        ) : (
          <EmptyState
            kind="filtered"
            title={`No spans match "${text}" in ${RANGE_PHRASE[range]}.`}
            action={<Button onClick={onWiden}>Search the last 30 days</Button>}
          >
            Older spans may match. Other words, another match or more fields may too.
          </EmptyState>
        )
      }
    >
      {() => (
        <>
          {last ? <MetaNotice meta={last.meta} noun="spans" /> : null}
          {hits.length > 0 ? (
            <ol aria-label="Matching spans" className="flex flex-col">
              {hits.map((hit) => (
                <Hit key={`${hit.trace_id}/${hit.span.span_id}`} project={project} hit={hit} />
              ))}
            </ol>
          ) : (
            <EmptyState
              kind="incomplete"
              title={query.hasNextPage ? "No matches yet." : "No matches to show."}
            >
              {query.hasNextPage
                ? "Spans that hold every word but not the exact phrase were left out. Older spans may still match."
                : "Spans may still be arriving, or part of the data couldn't be read."}
            </EmptyState>
          )}
          {query.isFetchNextPageError ? (
            <ErrorCard
              error={asUiError(query.error)}
              label="more results"
              onRetry={() => query.fetchNextPage()}
            />
          ) : query.hasNextPage ? (
            <div className="mt-3">
              <Button
                onClick={() => query.fetchNextPage()}
                disabled={query.isFetchingNextPage}
                aria-busy={query.isFetchingNextPage || undefined}
              >
                {query.isFetchingNextPage ? "Loading more results" : "Load more results"}
              </Button>
            </div>
          ) : null}
        </>
      )}
    </Section>
  );
}

const FIELD_LABEL: Record<SearchField, string> = {
  input: "Input",
  output: "Output",
  metadata: "Metadata",
};

function traceHref(project: string, hit: SearchHit): Route {
  const params = new URLSearchParams({ project, trace: hit.trace_id, span: hit.span.span_id });
  return `/trace/?${params}` as Route;
}

function Hit({ project, hit }: { project: string; hit: SearchHit }) {
  const { span } = hit;
  return (
    <li className="flex flex-col gap-1 border-t border-ash-border py-3 first:border-t-0 first:pt-0">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <Link href={traceHref(project, hit)} className="text-body font-medium break-all">
          {span.name || "Unnamed span"}
        </Link>
        <span className="text-label text-ink-soft">{TYPE_LABEL[span.observation_type]}</span>
        <span className="text-label text-ink-soft">{formatTime(span.start_time)}</span>
      </div>
      {hit.matches.map((found) => (
        <MatchLine key={found.field} found={found} />
      ))}
      {hit.matches.length === 0 ? (
        <p className="text-label text-ink-soft">Matched further into the span&apos;s fields.</p>
      ) : null}
    </li>
  );
}

function MatchLine({ found }: { found: SearchMatch }) {
  if (found.fragments.length === 0) {
    return (
      <p className="text-data">
        <span className="me-2 text-label text-ink-soft">{FIELD_LABEL[found.field]}</span>
        Has the entries you searched for.
      </p>
    );
  }
  // Fragments never move, so each is keyed by where it starts in the snippet.
  const pieces: { text: string; highlighted: boolean; start: number }[] = [];
  let offset = 0;
  for (const fragment of found.fragments) {
    pieces.push({ ...fragment, start: offset });
    offset += fragment.text.length;
  }
  return (
    <p className="text-data break-words whitespace-pre-wrap">
      <span className="me-2 text-label text-ink-soft">{FIELD_LABEL[found.field]}</span>
      {found.cut_before ? "…" : null}
      {pieces.map((piece) =>
        piece.highlighted ? (
          <mark
            key={piece.start}
            className="rounded-sm bg-silver-mist/60 px-0.5 font-medium text-midnight-ink"
          >
            {piece.text}
          </mark>
        ) : (
          <span key={piece.start}>{piece.text}</span>
        ),
      )}
      {found.cut_after ? "…" : null}
    </p>
  );
}

function ResultsSkeleton() {
  return (
    <div className="flex flex-col">
      {SKELETON_ROWS.map((id) => (
        <div
          key={id}
          className="flex flex-col gap-2 border-t border-ash-border py-3 first:border-t-0"
        >
          <div className="skeleton h-4 w-1/3" />
          <div className="skeleton h-3 w-4/5" />
        </div>
      ))}
    </div>
  );
}
