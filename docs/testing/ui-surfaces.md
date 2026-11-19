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
- Unless a test signs in, the fixture answers the shell's sign-in check (`GET auth/providers`) with 404
  `feature_disabled`: the server has no control plane and the Connect dialog applies. `signedIn(api)` gives the
  server a control plane and the browser a session (`GET auth/session`).
- Loading states hold the response until the test has checked the skeleton; the loading ladder shows nothing
  for 300 ms, then the skeleton, announced through a `status` region.
- Error states keep the route failing across the client's own retries (one for a 5xx), then check the mapped
  copy, the reference id, that no server text reaches the page, and that the retry refetches only its section.
- Every state runs axe (`@axe-core/playwright`) and fails on any serious or critical violation.
- Two projects run: `desktop` (1280 by 900) and `phone` (390 by 844, touch). CI runs both in
  `.github/workflows/ui-e2e.yml` on pull requests that touch `ui/`.
- The SQL workspace is the one surface that shows a server sentence: a workspace refusal writes how to fix the
  request in `detail` (contract 12.3), and the page shows it under a title it owns, with ClickHouse's message
  beside it. Its tests use the `sqlProblem` fixture for those refusals and still check that `title` never reaches
  the page.
- The `phone` project matches only `phone.spec.ts`, so `sql.spec.ts` sets the 390 by 844 viewport itself for its
  phone test.

## Surface-by-state table

| Surface: section | Loading | Success | Error | Empty |
|---|---|---|---|---|
| Home: set up | Not applicable: static steps until data bounds land | Four steps, each marked to do | Not applicable: makes no request | Not applicable |
| Home: tiles (traces, error rate, latency, cost, tokens) | Each tile holds its size with a skeleton after 300 ms, busy and announced | The headline for the range with a sparkline: the trace count, errors over spans, p50 and p95 of the newest bucket with spans, cost, tokens | Per tile: the mapped copy, reference id and a retry that refetches only that tile; the other tiles stay | Whole and no data: "No data in the last 24 hours." with "Show last 30 days"; first use at 30 days; partial or lagging: "No data to show yet." with "Check again" |
| Home: charts (traces over time, spans and errors, latency percentiles) | A chart-sized skeleton after 300 ms | SVG series with a dash and a marker each (percentiles in one ink family), a legend for two or more series, a crosshair driven by the pointer or the bucket slider's arrow keys, and "View as table" | As the tiles: the chart shares its tile's query and retry | As the tiles |
| Traces: recent traces | Twelve skeleton rows after 300 ms, announced | One row per trace with started, name (a link to the trace), duration, spans, cost and status as icon and label | In-section card with the mapped copy, reference id and a retry that refetches only the list; a 401 asks to connect and offers the dialog | Filtered: "No traces in the last 24 hours." with "Show last 30 days"; first use at 30 days; incomplete (partial or lagging): "No traces to show yet." with "Check again" |
| Trace: summary | Skeleton lines after 300 ms | Name, status as icon and label, start, duration, cost, tokens, span count; the partial or lagging notice from `meta`; on a paged trace, that totals cover the loaded spans | The page title and trace id stay; the spans section carries the one error card for the trace | Not applicable: a trace with no spans shows the spans section's empty state |
| Trace: spans (timeline) | Eight skeleton rows after 300 ms, announced | The span tree as a virtualized UI SDK waterfall: type, name, error marker, bar on the shared axis and duration; unpaged traces open fully | In-pane card with a retry that refetches only the trace; a missing trace uses the not-found copy, without a retry | "This trace has no spans to show yet." with "Check again", distinct from not found |
| Trace: span panel | Skeleton while the trace loads | Tabs: Overview (status, level, start and offset, duration, model, tokens, cost, child count, ids), then Input, Output, Attributes and Events, each the payload viewer; the tab is kept in `tab=` | Not shown while the trace has failed | "Select a span."; an invalid span link; a span not in the trace; on a paged trace, a span not loaded yet |
| Trace: payload (a tab of the span panel) | The open tab asks for the field's first kibibyte only, busy and announced ("Loading the input") | The first kibibyte at once, with the field's size; up to 256 KiB the rest loads by itself, larger fields on "Load 4.2 MB", by byte ranges with a progress bar and Cancel; parsed in a Web Worker into a virtualized tree whose branches open in place, or for `genai_messages` and `indexed_messages` spans a message list first with the JSON a choice away; text, and JSON cut at the size limit, as text | The section card with "Retry input"; when the rest fails, "Couldn't load the full input. The preview above is intact." with its own retry | "No input was recorded." for an empty field; "This span's input isn't stored." when the payload expired or was never stored |
| Search: results | The results section busy and announced, with skeleton rows after 300 ms | Each hit's span name (a link to the trace with the span selected), type and start, then each matching field's snippet with the matches marked; "Load more results" while there are more; a phrase page without hits reads on by itself | In-section card with the mapped copy, reference id and "Retry results"; a failed further page keeps the hits and offers its own retry | Before a query: "Type what to find." and nothing is asked; no match: "No spans match ..." with "Search the last 30 days"; incomplete when `meta` is partial or lagging |
| Trace: child spans (paged traces) | A "Loading child spans" row, busy, when a span opens | The page's spans under their parent; a "Show more child spans" row while the cursor has more | A "Couldn't load child spans" row whose activation refetches only that page (or restarts the span's pages after `cursor_invalid`) | A span whose count ran ahead shows no row once its page comes back empty |
| Trace: agent graph | The layout library loads only for the graph, behind a skeleton after 300 ms | Agents, tools and models as nodes with spans and errors; each node describes its calls; choosing a node filters the timeline | The section card with "Retry agent graph" when the layout library can't load, and the retry works | "This trace has no agents, tools or model calls." with "Show the timeline"; on a paged trace, none among the loaded spans |
| Trace: links | Not applicable | Not applicable | Not applicable | No trace in the URL, or an id that isn't 32 hex characters, points back to the list and requests nothing |
| SQL: schema | Four skeleton lines after 300 ms, announced; saved queries and history wait for its answer | One collapsible item per relation: description, time column and whether the window is required, columns with types, and "Insert example"; the first relation's example starts the editor | In-section card with the mapped copy, reference id and a retry that refetches only the schema | "No relations to query."; the editor starts empty and Run is off |
| SQL: workspace unavailable | Not applicable: it is the schema's answer | Not applicable | A page state by reason (`isolation_probe_failed`, `not_configured`, `probe_pending`, `probe_unavailable`, or the server's sentence for any other) with operator guidance, the failed checks and "Check again"; Run, export and saving are off, and saved queries and history are not requested | Not applicable |
| SQL: query and result | Run is off and Cancel is on while a run is in flight; the result shows an indeterminate progress bar named for the run | A typed grid: column name and ClickHouse type, numbers right-aligned by type, null marked, nested values as compact JSON, 100 rows per page; notices for a truncated result and cut values; rows returned, rows and bytes read, elapsed | An alert with the title for the code, the server's fix sentence, "ClickHouse said" in a monospace block, "Line L, column C" with the caret moved there and "Go to line", the seconds to wait for `sql_quota_exceeded`, and the reference id; a cancelled run says "The run was cancelled." and is not an error | "No result yet." before the first run; "The query returned no rows." for a result without rows |
| SQL: export | A progress bar named "Exporting CSV" (or JSON lines, Parquet); the export buttons are off | The file downloads and the section says "Downloaded crucible-sql.csv." | The same alert as a run, inside the query section | Not applicable |
| SQL: saved queries | Three skeleton rows after 300 ms, announced | One item per query: name, description, visibility, declared parameters; Open, Run when `can_run`, Delete when `can_edit`; Run opens a form built from the declared parameters with defaults prefilled; "Save as new" and "Save changes" open a form with the detected parameters and a type select | In-section card with the fix, reference id and a retry that refetches only the list; a stale save says the query changed and offers "Reload saved query"; a refused delete stays in the section | "No saved queries yet." |
| SQL: history | Five skeleton rows after 300 ms, announced | One row per run: started, outcome as icon and label, kind, rows, elapsed, first line of the query, "Load into editor"; refetched after every run, cancel and export | In-section card with the mapped copy, reference id and a retry that refetches only the history | "No runs yet." |
| Shell: sign-in check | Nothing for 300 ms, then "Connecting to Crucible", announced; the page waits for the check | Without a control plane, the page and the Connect dialog. Signed in, the page, the user's name, Sign out, and the session's projects in the project menu (switching keeps the page and range and drops entity ids) | A "Connection" card with the mapped copy and "Retry connection". No session, or a session that ends during any read, sends the browser to `/login/` with the page as `next` | Not applicable |
| Sign in | Field skeletons after 300 ms, announced | Provider buttons (full-page links to the server, carrying `next`), the password form and "Forgot your password?"; signing in opens `next`, or `/` when `next` leaves the server or points under `/api/`. A failed sign-in, or a provider failure in `?error=`, is an alert in the page's own words | Options that fail to load show the mapped copy, the reference id and "Try again" | A server with no owner points to setup; one without a control plane has no sign-in; one with no sign-in method turned on says so |
| Setup | Not applicable: requests nothing until submitted | Opens Crucible signed in as the owner | Checked before sending (password 12 to 128 characters, checked off live). `token_invalid` marks the token field; `conflict` links to sign in | Not applicable |
| Invitation | Not applicable: requests nothing until submitted | Opens Crucible signed in | `token_invalid` says to ask for a new link; `conflict` links to sign in | A link without its token says it is incomplete and requests nothing |
| Password reset | Not applicable: requests nothing until submitted | Requesting a link focuses "Request received" with the administrator's command (this version sends no email); a new password with the link's token focuses "Password changed" with a sign-in link | `token_invalid` says how to get a new link | Not applicable: the page always names `crucible admin reset-password <email>` |
| Settings: members | Three skeleton rows after 300 ms, announced; the invitations section loads on its own | Name, email, role and status. Owners and admins get a role menu (never owner) and Remove, which asks first, on other non-owner rows; nobody edits their own row | In-section card with the mapped copy, reference id and a retry that refetches only the members | "No members yet." Without a control plane, says which setting turns members on and requests nothing |
| Settings: invitations | Three skeleton rows after 300 ms, announced | Email, role, status and expiry; Revoke, which asks first, on pending ones. "Invite member" creates one and shows its link once, until the dialog closes | In-section card with a retry that refetches only the invitations | First use: "No invitations yet." |
| Settings: API keys | Three skeleton rows after 300 ms, announced | Public key, note, scopes, last use and status; Revoke on active keys asks first and sends `If-Match`. "Create key" shows the secret and public keys once, until the dialog closes | In-section card with a retry that refetches only the keys; a stale version (`precondition_failed`) refreshes the list | First use: "No API keys yet." Without the `manage` permission, for a project outside the session, with no projects, or without a control plane, the page explains and requests no keys |
| Settings: new project | Not applicable: requests nothing until submitted | For organization owners and admins, "New project" beside the settings tabs opens a dialog; the slug follows the name until edited. Creating refreshes the session, selects the project and opens its API keys page | Checked before sending (slug of lowercase letters, digits and hyphens). `conflict` marks the slug as taken; `validation_failed` marks the field it points at; `forbidden` says to ask an owner or admin | Not applicable: members and viewers, and servers without a control plane, get no control |
| Trace: tags, flags and feedback | The frame holds its place, busy, while the trace loads | Bookmark and Public toggles; the effective tags with a remove button each and an add form; scores with value, comment and source, and an add form. Each write carries an `Idempotency-Key`, shows its pending state, and the acknowledged value stays until a read shows it | A refused write says so beside its control with the reference id, and nothing else changes; the trace's own failure shows in the spans section | "No tags yet." and "No scores or feedback yet.", with the forms still offered |
| MCP: scope bar (every MCP page) | Not applicable: reads the URL only | Links to the nine MCP pages, the current one marked; Server side or Client side, the capture source (Automatic, SDK, Proxy, OTLP) and "Include caller faults", kept in `side=`, `source=` and `faults=` and carried by every MCP link; each section names the source it read for each server, and any server it could not read with the sources it has | Not applicable | Not applicable |
| MCP: servers, tools, deploys | Eight skeleton rows after 300 ms, announced | Servers: version, calls, error rate, caller faults, p50, p90, p99, clients and last seen, the name linking to its tools. Tools: Called and Unused tabs; called tools link to their detail, unused ones show their schema hash. Deploys: newest first, each with "Compare with" the deploy before it | In-section card with the mapped copy, reference id and a retry that refetches only the section | Filtered: "No MCP servers in the last 24 hours." with "Show last 30 days"; first use at 30 days says how to wrap or proxy a server; incomplete (partial or lagging): "No MCP servers to show yet." with "Check again" |
| MCP: tool detail | Each section (summary, calls over time, latency percentiles, error classes, top clients) holds its place with a skeleton after 300 ms | Six headline numbers; calls and errors as bars; p50, p90 and p99 in one ink family; both charts mark the deploys of the tool's servers with a dashed rule and list them, and both have "View as table"; error classes as bars with counts; top clients, labelled self-reported, linking to their operations | Per section, as the servers | Without a name, points to the tools list and requests nothing; no calls: "No MCP calls of this tool in the last 24 hours.", and at 30 days that it may be listed and never called |
| MCP: errors, clients, users, sessions | As the servers | Errors: Summary (counts, classes as bars, failed calls by target linking to their operations) or By message (server, target, class, JSON-RPC code, message, calls). Clients: by name, or By version, labelled self-reported. Users: id or subject, how they were identified, calls, errors, sessions. Sessions: group key and kind, client, calls, errors. Capped lists say which rows they kept | As the servers | As the servers; errors and users explain their own emptiness at 30 days |
| MCP: operations and operation | The list as the servers; the detail with skeleton rows | The list: started (a link to the operation), method, target, server, client, duration and status as icon and label, a caller fault named so; "Failed calls only" and the filters from the URL, each with Clear; "Load more operations" by cursor. The detail: every field, the captured arguments and result (their first 16 KiB, with the stored size), "Open in the trace", the other side's row of the call, and its multi-round exchange | As the servers; a failed further page keeps the rows and offers its own retry | List as the servers. Detail: outside the window, "This operation isn't in the selected time range."; when the answer is not whole, "This operation isn't available yet." |
| MCP: compare | Skeleton rows after 300 ms | Tools added, removed and with a changed schema; per tool, calls, error rate, p50 and p99 as baseline to candidate | As the servers | Without a server and two deploys, says to pick them from the deploys and requests nothing; no calls as the servers |

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
| Trace: payload | `trace.spec.ts` "payload": loading, success (tree), success (messages), large field by ranges, error with retry, empty and not stored (6) | `payload-viewer.test.tsx`, `payload-model.test.ts` |
| Search: results | `search.spec.ts`: idle, loading, success with more pages and the link, error with retry, empty with a wider range (5) | `search-view.test.tsx` |
| Trace: tags, flags and feedback | `trace.spec.ts` "facts": loading, success with a tag written, empty, error (4) | `trace-facts.test.tsx` |
| Shell: Connect dialog | `traces.spec.ts` "shell" (1) | `connect-dialog.test.tsx` |
| SQL: schema | `sql.spec.ts` "schema browser": loading, success with insert example, error with retry, empty (4) | `errors.test.ts` (in `features/sql`) |
| SQL: workspace unavailable | `sql.spec.ts` "workspace unavailable": `isolation_probe_failed` with "Check again", `not_configured` and `probe_pending` from a 503 (2); "editor and result": a run refused because the workspace went away (1) | `errors.test.ts` |
| SQL: query and result | `sql.spec.ts` "editor and result": loading, success grid, truncated with paging, empty, `sql_invalid` with caret, `sql_time_window_required`, `sql_forbidden`, `query_too_broad`, `forbidden`, `sql_quota_exceeded`, unexplained failure, cancel, Ctrl+Enter, parameters (14); "desktop layout" (1) | `sql-workspace.test.tsx`, `params.test.ts`, `cells.test.ts`, `errors.test.ts` |
| SQL: export | `sql.spec.ts` "export": download, feedback and failure (2) | `errors.test.ts` |
| SQL: saved queries | `sql.spec.ts` "saved queries": loading, success with open, error with retry, empty, run with parameters, run without parameters and Escape, save as new, update with `If-Match` and reload, delete, refused delete (10) | `params.test.ts`, `errors.test.ts` |
| SQL: history | `sql.spec.ts` "history": loading, success with load into editor, error with retry, empty then refreshed after a run (4) | `cells.test.ts` |
| Shell: sign-in check | `sign-in.spec.ts` "sessions": loading then error with retry, no session, a session that ends while reading, signed-in header and sign-out (4) | `auth-gate.test.tsx`, `client.test.ts` |
| Sign in | `sign-in.spec.ts` "sign in": loading then success, unsafe `next`, sign-in errors, options error, provider errors, setup and no control plane (6); `phone.spec.ts` | `login-form.test.tsx`, `next-path.test.ts`, `client.test.ts` |
| Setup | `onboarding.spec.ts` "first-run setup": success, checked before sending, token and owner errors (3); `phone.spec.ts` | `onboarding-forms.test.tsx`, `client.test.ts` |
| Invitation | `onboarding.spec.ts` "invitations": success, error, no token (3); `phone.spec.ts` | `onboarding-forms.test.tsx`, `client.test.ts` |
| Password reset | `onboarding.spec.ts` "password reset": request, confirm, error (3) | `onboarding-forms.test.tsx`, `client.test.ts` |
| Settings: members and invitations | `settings.spec.ts` "members": loading then success, role change, remove, invite link once, revoke, error with retry, read-only roles, no control plane (8); `phone.spec.ts` | `settings.test.tsx` |
| Settings: API keys | `settings.spec.ts` "API keys": loading then success, secret once, revoke with `If-Match`, error with retry, no permission (5); `phone.spec.ts` | `settings.test.tsx`, `client.test.ts` |
| Settings: new project | `projects.spec.ts`: create opens the API keys page, a taken slug, no control for a member (3); `phone.spec.ts` | `new-project.test.tsx`, `slug.test.ts` |
| MCP: all pages | `mcp.spec.ts` "servers": loading, success, scope controls, error with retry, empty (5); "tools": unused tab and detail link, tool detail chart with deploy markers and table view, tool without calls (3); "errors, clients, users and sessions" (4); "operations, deploys and compare": load more and detail, deploy compare, compare without deploys (3); each list page (tools, errors, clients, users, sessions, operations, deploys): loading then error with retry, and empty (14); `phone.spec.ts` | `mcp.test.tsx` |
| Phone | `phone.spec.ts` (11); `sql.spec.ts` "phone at 390 px" (1) | |

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
- **SQL editor.** Ctrl+Enter or Cmd+Enter in the query text runs the query and focus stays in the editor; Enter
  in a parameter field runs it too. "Go to line" in a query error moves focus and the caret to the reported
  position.
- **SQL saved queries.** Enter on "Run" opens the parameter form; Escape closes it without running and focus
  returns to the button. The save form opens with focus in the name field.
- **Settings confirmations.** A row's Remove or Revoke opens one confirmation shared by the table, starting on
  Cancel; Cancel or Escape returns focus to that row's button. Each row button's name carries its row, like
  "Remove Grace Hopper".
- **Forms replaced by their result.** On the password reset pages, focus moves to the result's heading.
- **New project.** The dialog opens with focus in Name; after a project is created, the dialog closes and a
  status message says so.

## Phone width

At 390 px each page reads without the page scrolling sideways: the traces table scrolls inside its frame, the
waterfall drops to name, bar and duration, the span panel becomes a bottom sheet, and search snippets wrap. The
sign-in, setup and invitation pages fit, the MCP scope controls wrap and its tables scroll inside their frames, the
settings tables scroll inside their frames with the signed-in header wrapping, and a new key's secret wraps inside
its dialog.

On the SQL page the schema, query, result, saved queries and history stack in that order; the result grid and the
history table scroll inside their frames.

## Not covered yet

- The span panel's metadata and MCP tabs wait for the payload route's `metadata`, `mcp_args` and `mcp_result`
  selectors (contract 12.6), which it does not serve yet. Until then the MCP operation page shows the first 16 KiB
  of captured arguments and results, not the whole field.
- A deep link to a span below the loaded levels of a paged trace cannot be revealed until the spans route serves
  `ancestors_of` (contract 8.8); the panel says how to find it.
- The account page, password change, organization settings, ownership transfer, project members and the
  sign-in provider settings have API routes (contract 12.6) but no pages yet.
- The set-up steps will read data bounds and API keys once those screens land.
- Brush-to-zoom on charts is planned; the crosshair, the bucket slider and the table view cover reading values.
