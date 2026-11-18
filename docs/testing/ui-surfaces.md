# UI surfaces by state

Every section of the UI that fetches its own data designs four states: loading, success, error and empty. This
page lists each surface and section, what renders in each state, and the test that pins it. The Playwright suite
in `ui/e2e/` checks every row in a real browser; the Vitest suites beside the code check the same wiring in
jsdom.

## How the suite runs

```sh
pnpm --filter crucible-ui build              # the static export in ui/out
pnpm --filter crucible-ui exec playwright test
```

- `ui/e2e/serve.mjs` serves `ui/out` the way the binary does (contract 12.1): exact files, `/x/` as
  `x/index.html`, slashless routes as the page, and `404.html` for anything else. It has no dependencies.
- Each test answers every `/api/v1/**` request itself through route interception. A request no test answers,
  or an uncaught error in the page, fails the test.
- Loading states hold the response until the test has checked the skeleton; the loading ladder shows nothing
  for 300 ms, then the skeleton, announced through a `status` region.
- Error states keep the route failing across the client's own retries (one for a 5xx), then check the mapped
  copy, the reference id, that no server text reaches the page, and that the retry refetches only its section.
- Every state runs axe (`@axe-core/playwright`) and fails on any serious or critical violation.
- Two projects run: `desktop` (1280 by 900) and `phone` (390 by 844, touch). CI runs both in
  `.github/workflows/ui-e2e.yml` on pull requests that touch `ui/`.

## Surface-by-state table

| Surface: section | Loading | Success | Error | Empty |
|---|---|---|---|---|
| Home: set up | Not applicable: static steps until data bounds land | Four steps, each marked to do | Not applicable: makes no request | Not applicable |
| Home: tiles (traces, error rate, latency, cost, tokens) | Each tile holds its size with a skeleton after 300 ms, busy and announced | The headline for the range with a sparkline: the trace count, errors over spans, p50 and p95 of the newest bucket with spans, cost, tokens | Per tile: the mapped copy, reference id and a retry that refetches only that tile; the other tiles stay | Whole and no data: "No data in the last 24 hours." with "Show last 30 days"; first use at 30 days; partial or lagging: "No data to show yet." with "Check again" |
| Home: charts (traces over time, spans and errors, latency percentiles) | A chart-sized skeleton after 300 ms | SVG series with a dash and a marker each (percentiles in one ink family), a legend for two or more series, a crosshair driven by the pointer or the bucket slider's arrow keys, and "View as table" | As the tiles: the chart shares its tile's query and retry | As the tiles |
| Traces: recent traces | Twelve skeleton rows after 300 ms, announced | One row per trace with started, name (a link to the trace), duration, spans, cost and status as icon and label | In-section card with the mapped copy, reference id and a retry that refetches only the list; a 401 asks to connect and offers the dialog | Filtered: "No traces in the last 24 hours." with "Show last 30 days"; first use at 30 days; incomplete (partial or lagging): "No traces to show yet." with "Check again" |
| Trace: summary | Skeleton lines after 300 ms | Name, status as icon and label, start, duration, cost, tokens, span count; the partial or lagging notice from `meta`; on a paged trace, that totals cover the loaded spans | The page title and trace id stay; the spans section carries the one error card for the trace | Not applicable: a trace with no spans shows the spans section's empty state |
| Trace: spans (timeline) | Eight skeleton rows after 300 ms, announced | The span tree as a virtualized UI SDK waterfall: type, name, error marker, bar on the shared axis and duration; unpaged traces open fully | In-pane card with a retry that refetches only the trace; a missing trace uses the not-found copy, without a retry | "This trace has no spans to show yet." with "Check again", distinct from not found |
| Trace: span panel | Skeleton while the trace loads | Status, level, start and offset, duration, model, tokens, cost, child count, ids; a planned note for input, output, attributes and events | Not shown while the trace has failed | "Select a span."; an invalid span link; a span not in the trace; on a paged trace, a span not loaded yet |
| Trace: child spans (paged traces) | A "Loading child spans" row, busy, when a span opens | The page's spans under their parent; a "Show more child spans" row while the cursor has more | A "Couldn't load child spans" row whose activation refetches only that page (or restarts the span's pages after `cursor_invalid`) | A span whose count ran ahead shows no row once its page comes back empty |
| Trace: agent graph | The layout library loads only for the graph, behind a skeleton after 300 ms | Agents, tools and models as nodes with spans and errors; each node describes its calls; choosing a node filters the timeline | The section card with "Retry agent graph" when the layout library can't load, and the retry works | "This trace has no agents, tools or model calls." with "Show the timeline"; on a paged trace, none among the loaded spans |
| Trace: links | Not applicable | Not applicable | Not applicable | No trace in the URL, or an id that isn't 32 hex characters, points back to the list and requests nothing |
| Trace: tags, flags and feedback | The frame holds its place, busy, while the trace loads | Bookmark and Public toggles; the effective tags with a remove button each and an add form; scores with value, comment and source, and an add form. Each write carries an `Idempotency-Key`, shows its pending state, and the acknowledged value stays until a read shows it | A refused write says so beside its control with the reference id, and nothing else changes; the trace's own failure shows in the spans section | "No tags yet." and "No scores or feedback yet.", with the forms still offered |

## Tests per row

| Row | Playwright (`ui/e2e/`) | Vitest |
|---|---|---|
| Home: set up | `home.spec.ts` "static" (1); `phone.spec.ts`: home reads without sideways scrolling | `app-shell.test.tsx` |
| Home: tiles and charts | `home.spec.ts`: loading, success with the table view and the bucket slider, error with retry, empty (4) | `home-overview.test.tsx`, `chart-model.test.ts` |
| Traces: recent traces | `traces.spec.ts`: loading, success, error, unauthenticated, empty (filtered, first use), empty (incomplete), keyboard link (7) | `traces-section.test.tsx` |
| Trace: summary | `trace.spec.ts` "summary": loading, success, partial and lagging, error (4) | `trace-detail.test.tsx` |
| Trace: spans | `trace.spec.ts` "spans": loading, success, error with retry, not found, empty, keyboard (6) | `trace-detail.test.tsx`, `trace-tree.test.ts` |
| Trace: span panel | `trace.spec.ts` "span panel": loading, empty, deep link, unknown and invalid span, error (5) | `trace-detail.test.tsx` |
| Trace: child spans | `trace.spec.ts` "child spans": loading then success, more, error with retry, empty, unloaded deep link (5) | `trace-tree.test.ts`, `child-pages.test.ts`, `trace-detail.test.tsx` |
| Trace: agent graph | `trace.spec.ts` "agent graph": loading, success, keyboard and filter, error with retry, empty, paged (6) | `agent-graph.test.ts`, `trace-detail.test.tsx` |
| Trace: links | `trace.spec.ts` "links" (2) | `trace-detail.test.tsx` |
| Trace: tags, flags and feedback | `trace.spec.ts` "facts": loading, success with a tag written, empty, error (4) | `trace-facts.test.tsx` |
| Shell: Connect dialog | `traces.spec.ts` "shell" (1) | `connect-dialog.test.tsx` |
| Phone | `phone.spec.ts` (4) | |

## Keyboard paths

- **Span tree.** The waterfall is one tab stop. Down and Up move between rows, Left collapses and Right expands,
  Enter selects; the selected span goes to `span=` and fills the span panel. A "Show more child spans" row loads
  the next page on Enter, and when it leaves, focus moves to the first span it loaded.
- **Span panel on a phone.** Enter on a row opens the span sheet; Escape closes it and focus returns to the row.
  "Show span details" opens it again.
- **Agent graph.** The graph is one tab stop (an APG listbox). Arrows step through every node in flow order, Home
  and End go to the ends, typing jumps by name, Enter or Space chooses, and + and - zoom the canvas.
- **Connect dialog.** Enter opens it with focus in the project id field; Escape closes it and focus returns to
  its trigger.

## Phone width

At 390 px each page reads without the page scrolling sideways: the traces table scrolls inside its frame, the
waterfall drops to name, bar and duration, and the span panel becomes a bottom sheet.

## Not covered yet

- Full input and output payloads, attributes and events need the span detail and payload endpoints (contract
  12.6), which do not exist yet; the span panel shows the core fields and says the rest is planned.
- A deep link to a span below the loaded levels of a paged trace cannot be revealed until the spans route serves
  `ancestors_of` (contract 8.8); the panel says how to find it.
- The set-up steps will read data bounds and API keys once those screens land.
- Brush-to-zoom on charts is planned; the crosshair, the bucket slider and the table view cover reading values.
