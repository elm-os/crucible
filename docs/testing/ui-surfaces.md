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
| Home: tiles | Not applicable: static until the metrics API lands | Five tiles marked planned | Not applicable: makes no request | Not applicable |
| Traces: recent traces | Twelve skeleton rows after 300 ms, announced | One row per trace with started, name (a link to the trace), duration, spans, cost and status as icon and label | In-section card with the mapped copy, reference id and a retry that refetches only the list; a 401 asks to connect and offers the dialog | Filtered: "No traces in the last 24 hours." with "Show last 30 days"; first use at 30 days; incomplete (partial or lagging): "No traces to show yet." with "Check again"; under filters: "No traces match these filters" with "Clear filters" |
| Traces and spans: filter bar | Not applicable: the controls render at once and make no request | A field (name, environment, level, type, user, session, tag, metadata, release, version), a value (a list for level and type, text for the rest) and "Add filter"; each condition a chip that removes it; "Clear filters"; every condition in the URL with the API's parameter names | Not applicable: the list's own section shows a refused filter | No condition: no chips. An empty value, metadata without `key=value`, or type with metadata on the trace list says why inline |
| Traces: live tail | Not applicable: asks in the background every 5 s with the first page's window, filters and head cursor | "Show N new traces" (100+ when saturated); pressing it reads the list again | Silent: a refused cursor reads the list again; other failures wait for the next poll | Nothing new shows nothing; an empty list counts its first traces too; when the page's window ends with nothing new, the list reads the next window |
| Spans: spans | Twelve skeleton rows after 300 ms, announced | One row per span with started, name (a link to the span in its trace), type, duration, model, cost and status as icon and label; "Load more spans" pages with the first page's window | In-section card with the mapped copy and a retry; a refused filter is mapped, never shown raw; a failed or refused page under the rows says so with its own retry or a restart | Filtered window: "Show last 30 days"; first use at 30 days; under filters: "Clear filters"; incomplete: "Check again" |
| Sessions: sessions | Twelve skeleton rows after 300 ms, announced | One row per session with last seen, session (a link), traces, spans, cost, tokens and errors as icon and label; "Load more sessions" | In-section card with the mapped copy, reference id and a retry of the list only; a refused page cursor offers "Show the list from the top" | Filtered: "No sessions in the last 24 hours." with "Show last 30 days"; first use at 30 days; incomplete: "No sessions to show yet." with "Check again" |
| Session: summary | Fact skeletons after 300 ms | Traces, spans, status, cost, tokens, first and last seen, and the users as links | The session's own card: a missing session uses the not-found copy; the traces section still answers | Incomplete (the server could not see it whole): "This session has no data to show yet." with "Check again" |
| Session and user: traces | Twelve skeleton rows after 300 ms | The trace list filtered by the session or the user, with "Load more traces" | In-section card with a retry of that section only | "No traces of this session in the last 24 hours." with "Show last 30 days"; incomplete: "Check again" |
| Users: users | Twelve skeleton rows after 300 ms, announced | One row per user with last seen, user (a link), traces, sessions, spans, cost, tokens and errors; "Load more users" | In-section card with the mapped copy, reference id and a retry of the list only | Filtered window: "Show last 30 days"; first use at 30 days; incomplete: "Check again" |
| User: summary | Fact skeletons after 300 ms | Traces, sessions, spans, status, cost, tokens, first and last seen | The user's own card: a missing user uses the not-found copy | Incomplete: "This user has no data to show yet." with "Check again" |
| User: sessions | Twelve skeleton rows after 300 ms | One row per session the user's traces name: latest trace, session (a link), traces, first trace; "Load more sessions" | In-section card with a retry of this section only; the summary and the traces stay | "No sessions of this user in the last 24 hours." with "Show last 30 days"; incomplete: "Check again" |
| Session and user: links | Not applicable | Not applicable | Not applicable | No session or user in the URL points back to its list and requests nothing |
| Trace: summary | Skeleton lines after 300 ms | Name, status as icon and label, start, duration, cost, tokens, span count; the partial or lagging notice from `meta`; on a paged trace, that totals cover the loaded spans | The page title and trace id stay; the spans section carries the one error card for the trace | Not applicable: a trace with no spans shows the spans section's empty state |
| Trace: spans (timeline) | Eight skeleton rows after 300 ms, announced | The span tree as a virtualized UI SDK waterfall: type, name, error marker, bar on the shared axis and duration; unpaged traces open fully | In-pane card with a retry that refetches only the trace; a missing trace uses the not-found copy, without a retry | "This trace has no spans to show yet." with "Check again", distinct from not found |
| Trace: span panel | Skeleton while the trace loads | Tabs: Overview (status, level, start and offset, duration, model, tokens, cost, child count, ids), then Input, Output, Attributes and Events, each the payload viewer; the tab is kept in `tab=` | Not shown while the trace has failed | "Select a span."; an invalid span link; a span not in the trace; on a paged trace, a span not loaded yet |
| Trace: payload (a tab of the span panel) | The open tab asks for the field's first kibibyte only, busy and announced ("Loading the input") | The first kibibyte at once, with the field's size; up to 256 KiB the rest loads by itself, larger fields on "Load 4.2 MB", by byte ranges with a progress bar and Cancel; parsed in a Web Worker into a virtualized tree whose branches open in place, or for `genai_messages` and `indexed_messages` spans a message list first with the JSON a choice away; text, and JSON cut at the size limit, as text | The section card with "Retry input"; when the rest fails, "Couldn't load the full input. The preview above is intact." with its own retry | "No input was recorded." for an empty field; "This span's input isn't stored." when the payload expired or was never stored |
| Search: results | The results section busy and announced, with skeleton rows after 300 ms | Each hit's span name (a link to the trace with the span selected), type and start, then each matching field's snippet with the matches marked; "Load more results" while there are more; a phrase page without hits reads on by itself | In-section card with the mapped copy, reference id and "Retry results"; a failed further page keeps the hits and offers its own retry | Before a query: "Type what to find." and nothing is asked; no match: "No spans match ..." with "Search the last 30 days"; incomplete when `meta` is partial or lagging |
| Trace: child spans (paged traces) | A "Loading child spans" row, busy, when a span opens | The page's spans under their parent; a "Show more child spans" row while the cursor has more | A "Couldn't load child spans" row whose activation refetches only that page (or restarts the span's pages after `cursor_invalid`) | A span whose count ran ahead shows no row once its page comes back empty |
| Trace: agent graph | The layout library loads only for the graph, behind a skeleton after 300 ms | Agents, tools and models as nodes with spans and errors; each node describes its calls; choosing a node filters the timeline | The section card with "Retry agent graph" when the layout library can't load, and the retry works | "This trace has no agents, tools or model calls." with "Show the timeline"; on a paged trace, none among the loaded spans |
| Trace: links | Not applicable | Not applicable | Not applicable | No trace in the URL, or an id that isn't 32 hex characters, points back to the list and requests nothing |

## Tests per row

| Row | Playwright (`ui/e2e/`) | Vitest |
|---|---|---|
| Home | `home.spec.ts` (1); `phone.spec.ts`: home reads without sideways scrolling | `app-shell.test.tsx` |
| Traces: recent traces | `traces.spec.ts`: loading, success, error, unauthenticated, empty (filtered, first use), empty (incomplete), keyboard link (7) | `traces-section.test.tsx` |
| Traces and spans: filter bar | `lists.spec.ts` "traces: filters": add a condition from the field list, the URL, the filtered empty state and "Clear filters"; "spans": add and remove conditions (2) | `spans-section.test.tsx` |
| Traces: live tail | `lists.spec.ts` "live tail": the count and the refresh, and an empty list (2) | `live-tail.test.tsx`: the request's window, filters and cursor; an empty list; nothing new; the window's end with and without a count |
| Spans: spans | `lists.spec.ts` "spans": loading, success, filters in the URL, error, empty under filters (2) | `spans-section.test.tsx` |
| Sessions: sessions | `lists.spec.ts` "sessions: list": loading, success with Load more, error, empty (filtered, first use, incomplete) (4) | `sessions-section.test.tsx`, including a refused page cursor |
| Session: summary and traces | `lists.spec.ts` "session": success, missing, incomplete (2) | `session-detail.test.tsx`, including the link without a session |
| Users: users | `lists.spec.ts` "users": loading, success, error, empty (1) | `users.test.tsx` |
| User: summary, sessions and traces | `lists.spec.ts` "users": one section down and its retry (1) | `users.test.tsx`, including the link without a user |
| Trace: summary | `trace.spec.ts` "summary": loading, success, partial and lagging, error (4) | `trace-detail.test.tsx` |
| Trace: spans | `trace.spec.ts` "spans": loading, success, error with retry, not found, empty, keyboard (6) | `trace-detail.test.tsx`, `trace-tree.test.ts` |
| Trace: span panel | `trace.spec.ts` "span panel": loading, empty, deep link, unknown and invalid span, error (5) | `trace-detail.test.tsx` |
| Trace: child spans | `trace.spec.ts` "child spans": loading then success, more, error with retry, empty, unloaded deep link (5) | `trace-tree.test.ts`, `child-pages.test.ts`, `trace-detail.test.tsx` |
| Trace: agent graph | `trace.spec.ts` "agent graph": loading, success, keyboard and filter, error with retry, empty, paged (6) | `agent-graph.test.ts`, `trace-detail.test.tsx` |
| Trace: links | `trace.spec.ts` "links" (2) | `trace-detail.test.tsx` |
| Trace: payload | `trace.spec.ts` "payload": loading, success (tree), success (messages), large field by ranges, error with retry, empty and not stored (6) | `payload-viewer.test.tsx`, `payload-model.test.ts` |
| Search: results | `search.spec.ts`: idle, loading, success with more pages and the link, error with retry, empty with a wider range (5) | `search-view.test.tsx` |
| Shell: Connect dialog | `traces.spec.ts` "shell" (1) | `connect-dialog.test.tsx` |
| Phone | `phone.spec.ts` (5) | |

## Keyboard paths

- **Span tree.** The waterfall is one tab stop. Down and Up move between rows, Left collapses and Right expands,
  Enter selects; the selected span goes to `span=` and fills the span panel. A "Show more child spans" row loads
  the next page on Enter, and when it leaves, focus moves to the first span it loaded.
- **Span panel on a phone.** Enter on a row opens the span sheet; Escape closes it and focus returns to the row.
  "Show span details" opens it again.
- **Agent graph.** The graph is one tab stop (an APG listbox). Arrows step through every node in flow order, Home
  and End go to the ends, typing jumps by name, Enter or Space chooses, and + and - zoom the canvas.
- **Payload tree.** The tree is one tab stop (UI SDK Tree). Down and Up move between rows, Right opens a branch
  and loads its children from the worker, Left closes it; only the rows in view are mounted.
- **Connect dialog.** Enter opens it with focus in the project id field; Escape closes it and focus returns to
  its trigger.

## Phone width

At 390 px each page reads without the page scrolling sideways: the traces table scrolls inside its frame, the
waterfall drops to name, bar and duration, the span panel becomes a bottom sheet, and search snippets wrap.

## Not covered yet

- The span panel's metadata and MCP tabs wait for the payload route's `metadata`, `mcp_args` and `mcp_result`
  selectors (contract 12.6), which it does not serve yet.
- A deep link to a span below the loaded levels of a paged trace cannot be revealed until the spans route serves
  `ancestors_of` (contract 8.8); the panel says how to find it.
- Home's sections will fetch once the data-bounds and metrics screens land; their four states will join this
  table then.
