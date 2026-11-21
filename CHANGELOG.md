# Changelog

Notable changes to Crucible, newest first. Versions follow [semantic versioning](https://semver.org/).

## [Unreleased]

- MCP analytics over 12 hours or more (the MCP pages, their API routes and MCP tools, `mcp_ops` metrics and alert
  rules) read whole hours from a new hourly rollup, `mcp_tool_rollup_hourly`, which the shipper writes beside the
  minute rollup, and only the minutes at the window's edges from the minute rollup; the answers are the same.
- Migration step `0005_mcp_tool_rollup_hourly` adds the hourly rollup and copies the existing minute rollup into it
  once, on the first start of this version.
- The server that applies a migration step renews the migrations lease every 5 seconds while the step runs, so a
  server that starts meanwhile waits for it, also during a step that takes longer than the lease's 15 seconds, such
  as the copy of step 0005.
- A migration step whose content changed, such as steps 0001 to 0003 on an upgrade from 0.1.1, applies once and the
  ledger records its new content; it no longer applies again, with a warning, at every start.
- A start whose master key the ClickHouse service users refuse leaves a changed migration step for a start with the
  right key, so it no longer gives the SQL workspace's probe users that key's passwords.
- With Postgres, a server started without `--migrate` stays not ready and refuses ingest until the migrations ledger
  records every step of its version, and says to run `crucible migrate` or to start one server with `--migrate`.
- Settings has an Organization page for the organization's name, project access (every project, or only the projects
  someone is added to) and the allowed email domains, whose people join as viewers without an invitation once their
  sign-in provider verifies their address. Members and viewers read it; editing needs a screen at least 768 pixels
  wide.
- Settings has a Sign-in page that adds, edits, turns off and deletes GitHub, Google and OpenID Connect sign-in
  providers, replaces a client secret without ever showing the stored one, and copies the callback URL to register
  with the provider.
- `crucible proxy` records the arguments and results of the tool calls a `--capture` rule (or
  `CRUCIBLE_PROXY_CAPTURE`) names, with the SDKs' rules: off by default, sampled per call, scrubbed with the shared
  rules and cut at 16 KiB per field, keeping no more than the cap plus 1 KiB of a value, and never on the forwarding
  path.
- The Python SDK encodes captured content without a step per JSON token: a result of 500 records took 0.4 ms
  instead of 5.2 ms per call on a laptop, with the same text as before.
- The TypeScript SDK writes runs of plain records with one serializer call and skips a scrub rule on text that lacks
  a pair of characters every match of it holds (a hexadecimal digit before a `:` for the ipv6 rule), so capturing
  500 records or 64 KiB of hexadecimal text costs about half as much.
- The Rust SDK's `flush` waits at most the export timeout in all, for the spans and the queued scores together; it
  waited as long again for the scores.
- The SDK conformance driver checks that the normalizer keeps each span's captured content as sent, and a capability
  manifest can name the proxy as the sender (`"source": "proxy"`); the proxy passes the content-capture scenario in
  all four variants.
- With object storage, export files go to the bucket under the project's prefix, and their download answers 303 with
  a presigned link that any server hands out; expiry and deletion requests remove the objects.
- The span panel has a Media tab, in place of the media section under each payload tab, that lists the media a span's
  input, output and metadata reference, with where each appears, and the media uploaded for the span; without object
  storage it says so and links the media guide.
- `GET projects/{project_id}/spans/{trace_id}/{span_id}/media` lists the media a span references.
- With `egress.mode = strict`, alert channels and webhooks cannot target the object store's endpoints.
- SeaweedFS and Garage join the Versity S3 gateway as tested object stores, and the media guide says what each one
  needs.
- Each project runs at most 4 metrics queries at once on a server (`metrics.queries_per_project`), so a dashboard of
  many widgets takes turns: the other queries wait within their deadline instead of reaching ClickHouse together.
- Deleting a project deletes its dashboards with their widgets and its media records, and then every object under
  the project's key prefix in object storage.
- A time series widget draws as areas: a light fill under each series' line, with its dash, marker and table view.
- The widget sheet edits a widget's filters: a field of the metrics schema, is or is not, and a value, with the
  values the time range holds offered as suggestions.
- Search takes the filters of the span list, name included, as `filters` in its request, and the search page has the
  lists' filter bar. The lists, search and the MCP server's `list_traces` and `list_spans` read one filter object
  and refuse an invalid one alike: the same member, with the same code.
- A listener whose address is taken names the setting that moves it, for example `server.metrics_listen`
  (`CRUCIBLE_SERVER_METRICS_LISTEN`).
- Crucible refuses a change that would leave nobody able to sign in: turning off password sign-in while the sign-in
  page offers no provider, and turning off or deleting the last provider while password sign-in is off. The refusal
  (409 `conflict`) says what to turn on first.
- The Organization page has a password sign-in switch, and shows the refusal when password sign-in is the only way in.
- `crucible admin enable-password-login` turns password sign-in back on from the shell, for a deployment whose sign-in
  provider no longer signs anyone in; the server logs at start when nobody can sign in.
- Sign-in providers carry a version: `PATCH` and `DELETE org/auth-providers/{provider_id}` need it in `If-Match`, and
  the Sign-in page sends it and reloads a provider that changed meanwhile. Postgres migration
  `V11__auth_provider_version` adds the column.
- A sign-in provider's client ID, issuer and scopes can be changed in place. An empty client secret is refused and
  never replaces the stored one, and a new issuer unlinks the provider's accounts, which link again through their
  verified email.
- A provider's allowed email domains apply at every sign-in, also to accounts linked before the list changed, and the
  sign-in page names the domains the provider allows.
- Sign-in providers can be declared in the configuration (`[auth_github]`, `[auth_google]`, `[auth_oidc]`), with
  their client secrets in files or environment variables. They are read-only on the Sign-in page and in the API, and
  removing one from the configuration turns it off.
- `crucible migrate` exits with 1 when a changed migration step waits for the master key the deployment was migrated
  with, and names the steps, why the key is refused and the key to run it with, instead of reporting the migrations
  as applied.

## [0.1.1] - 2026-11-21

Trace reads stay fast while data is arriving, the MCP server lists spans, sessions and users, the Go SDK scrubs
captured content off the request path, and the web UI, compatible SDKs and deletion get fixes.

### Performance

- Telemetry reads leave ClickHouse's column statistics out of their planning (`use_statistics = 0`). On ClickHouse
  26.8, the version the Compose file ships, every insert or merge made reads that plan with statistics, such as
  trace by id's, load the statistics of every part of the table until the next refresh, five minutes later, so
  under live ingest they always paid for them: trace by id measured 124 ms p50 after an insert, and 35 ms without
  them, on a laptop.
- Trace by id reads a trace's spans and its facts together, and the trace list reads the facts of a page together,
  so each waits for fewer ClickHouse round trips.
- Reads by API keys and the MCP server wait for one of the 16 ClickHouse statements their identity may run at once,
  instead of answering 503 `telemetry_store_unavailable` when more are asked for at the same time.

### Web UI

- The dashboard editor sets the side, the capture source and whether caller faults count for a widget over MCP
  operations. The sheet's field for what a widget reads is called Data.
- The retention page keeps saying that the settings changed since the page loaded after it reloads the stored values,
  until you edit them, also when the change was a shortening you confirmed.
- The project menu shows a name changed in another session as soon as the project settings page reads the project
  list.
- The SQL page sends you to sign in when your session ends, and back to the page afterwards, as every other page does.

### Ingest and compatibility

- Without object storage, the media endpoint of compatible SDKs answers with both ways to keep their media, the SDK
  setting that keeps media inline or the server's object storage settings; the server names the SDK setting at
  startup and logs the warning at the first refusal.
- Function-call spans from the Go SDK of a supported evaluation product are recognized by their instrumentation
  scope name, and are mapped as that product's traffic.

### Retention and deletion

- A server that starts reads from Postgres only the deletion requests made after the oldest data in its write-ahead
  log arrived, not every request ever made, before it sends that data to ClickHouse.
- The metric `crucible_shipper_deletion_drops_total{kind}` counts the spans, MCP operations and facts that the
  shipper dropped before ClickHouse because a deletion request selects them.

### MCP and SDKs

- The MCP server's `list_spans`, `list_sessions`, `list_users` and `list_user_sessions` tools list a window's
  spans, sessions and users, and `list_traces` and `list_spans` take the filters of the REST trace and span lists.
- `crucible proxy` records the error message of a failed operation, scrubbed with the shared rules and cut at 2 KiB,
  and the keyed hashes of a multi-round exchange's `requestState`, as the SDKs do.
- The Go SDK scrubs captured arguments and results, error messages and handle ids off the request path, on its
  export goroutine, so capture no longer adds the scrubbing time to a call.
- The Rust SDK's `flush` and `shutdown` keep to the export timeout when an export to a stalled collector is already
  in flight.

## [0.1.0] - 2026-11-20

The first release of Crucible: open-source, self-hosted observability and analytics for AI agents and MCP
servers, built in Rust and engineered for speed and throughput. One binary ingests OpenTelemetry traces and serves
the web UI, the REST API and an MCP server. ClickHouse stores the telemetry and Postgres runs the control plane.

### Ingest and storage

- OTLP trace ingest over HTTP (protobuf or JSON, optionally gzip) on ports 4318 and 8080, and over gRPC on 4317.
- Every request is appended to a write-ahead log and acknowledged only after its commit group is synced to disk.
  Each batch reaches ClickHouse with a deduplication token derived from its log position, so a retry never stores
  a span twice. A batch ClickHouse rejects is quarantined on disk, not dropped.
- Fixed byte budgets for memory, with 429 or 503 and Retry-After under load instead of dropped data.
- Ingest's blocking work (decompression, scanning and frame compression) runs under one CPU budget of first-come
  permits, so a burst of large requests queues instead of oversubscribing the machine.
- Recent spans are served from memory, so a trace can be read as soon as it is acknowledged. During a ClickHouse
  outage, the trace list and trace detail answer from memory and say the answer is partial.
- Mappings for the OpenTelemetry GenAI and MCP semantic conventions and for popular instrumentation libraries,
  with compatible ingest paths for other platforms' OTLP exporters ([integrations](docs/integrations/)).
- Cost calculated from usage with a built-in price table, in exact integer arithmetic, when the client sends none.
- Spans older than a project's retention horizon, or than the maximum event age, are rejected through OTLP partial
  success.
- Single-node ClickHouse 26.3 or newer for telemetry, and Postgres 15 or newer for the control plane.

### Explore

- A web UI embedded in the binary: a home overview, traces with a timeline and an agent graph, spans, sessions,
  users, full-text search, and a payload viewer that loads large payloads by byte range.
- Tags, flags, feedback scores and corrections, written through the same durable path as spans. The trace list's tag
  filter matches a trace's current tags, edits included, and feedback also accepts a project's public key, for
  browser clients.
- A REST API under `/api/v1`, described by an OpenAPI 3.1 document (`crucible openapi`).
- A read-only SQL workspace, isolated per project by ClickHouse users, row policies and quotas, with saved queries
  and CSV, JSON lines and Parquet downloads.
- A metrics query API over spans and traces.
- Dashboards over the metrics query: built-in starters for an agent overview, cost and tokens, latency, errors and
  MCP servers, and your own, edited on a 12-column grid by pointer or keyboard, with stat, time series, bar, table
  and Markdown widgets.

### MCP

- Every MCP operation is stored with its method, target, error class, latency and capture source, and an agent's
  tool call links to the server-side span through trace context. Both MCP protocol eras are supported.
- MCP analytics for the server or the client side: servers, tools, errors, clients, users, sessions, operations,
  deploys and deploy comparison, with p50, p90 and p99 latency and caller faults counted apart.
- Alert rules on MCP calls, error rate (caller faults excluded), error count and latency percentiles.
- `crucible proxy stdio` and `crucible proxy http` record an MCP server's calls with no code changes, forward its
  traffic byte for byte, and keep the session working when Crucible is unreachable.
- Crucible's own MCP server at `/mcp`, with tools to list and search traces, read spans and payloads, create
  exports, and read MCP analytics (servers, tools, errors and deploy comparison).
- SDKs for TypeScript, Python, Go and Rust in [`sdk/`](sdk/), with agent helpers, feedback scores sent off the
  caller's path, and MCP server wrappers. They install from the repository.

### Platform

- Sign-in with email and password, and with GitHub, Google or any OpenID Connect provider added through the API.
  The first owner is created through a one-time setup link or `crucible admin init`.
- Organizations, projects, organization and project roles, invitations, and project API keys with scopes.
- A project settings page with the project's name, slug and id, and renaming for project admins.
- Alert rules on span and trace measures, delivered to Slack, email and signed webhooks under an egress policy.
- Exports in `crucible-jsonl` and `otlp-json` through the API, the UI, the MCP server and `crucible export`.
- Per-project retention settings and deletion requests by trace, user or session. A deletion also applies to data
  still in the write-ahead log, so a replay after a restart never inserts what it deleted.
- Optional media on S3-compatible object storage: presigned uploads and downloads, deduplication by content, the
  compatible media endpoint, and hourly collection of unused objects. Without a store, media stay inline in payloads.

### Distribution

- The container image `ghcr.io/elm-os/crucible` for linux/amd64 and linux/arm64, with SBOM and provenance
  attestations.
- Binaries for Linux and macOS, plus Windows for development, with shell and PowerShell installers. The
  write-ahead log's durability holds on Linux and macOS.
- Docker Compose files that run Crucible with a bundled ClickHouse and Postgres, or against your own
  ([self-hosting](docs/self-hosting/)).

### Known limits

- Crucible ingests OTLP traces; OTLP logs and metrics are answered with 404.
- ClickHouse Cloud and clustered ClickHouse are refused at startup.
- MCP analytics reads a minute-grain rollup, so windows of a week or more take seconds.
- Sign-in providers and organization settings are managed through the REST API, not the UI.
