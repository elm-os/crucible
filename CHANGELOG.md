# Changelog

Notable changes to Crucible, newest first. Versions follow [semantic versioning](https://semver.org/).

## [Unreleased]

- With `clickhouse.cluster` set, a start refuses tables created for a single server on any replica that answers,
  before any statement runs on the cluster, and names the replica, its tables and the fix. In 0.1.3 the check read
  only the replica that `clickhouse.url` names, and the `ON CLUSTER` statements left another replica's
  single-server tables as they were.
- When the migrations ledger in Postgres records the step that creates Crucible's ClickHouse service users and
  ClickHouse holds none of them, as with a new cluster given a single-server deployment's Postgres, `crucible
  migrate` exits with 1 and a start stays not ready, both saying so and naming the fix: a Postgres database of its
  own for the new ClickHouse, or an empty ledger. `crucible migrate` blamed the master key in 0.1.3, or reported the
  migrations applied when no step had changed; a wrong master key keeps its own message.
- On a cluster with a replica down, each read of every replica (the existence probe, the checks of a deletion and of
  a partition drop, and the check before each migration step) tries the stopped replica once instead of three times,
  so it waits for one connection attempt instead of three.
- The docs at a release's tag install that release: the release pull request writes the new version into every
  install command in the README, the docs and the SDK READMEs, and the SDK release workflow fails while one names
  another version. At the tags v0.1.1 to v0.1.3 the install commands name the previous release.
- The TypeScript SDK's MCP wrapper records the tools that a `tools/list` result names (each tool's name and input and
  output schemas, as `crucible.mcp.tools`), so for servers on the TypeScript SDK MCP > Tools shows each tool's schema
  hash and the Unused tab lists the listed tools that nobody called. In 0.1.3 the tab stayed empty for them.
- The TypeScript SDK classifies calls that the MCP SDK refuses before the tool's handler runs: arguments that fail the
  tool's input schema are `validation` errors and "Tool <name> not found" is `not_found`, both caller faults with
  JSON-RPC code -32602, so they leave tool error rates unless caller faults are included. In 0.1.3 refused arguments
  counted as tool failures (`tool_execution`) and an unknown tool as invalid arguments.
- The Python SDK classifies the same refusals from mcp 2.3, which answers them with `isError` results: "Unknown tool:
  <name>" is `not_found` and arguments that fail the tool's input model are `validation`, both caller faults with
  JSON-RPC code -32602.
- MCP tool errors recorded by the TypeScript SDK carry a message: the text of the result's first text block, which
  holds a thrown error's message, scrubbed and cut at 2 KiB as the Go and Rust SDKs and the proxy send it. MCP >
  Errors groups them by it and the Operation page shows it, where 0.1.3 showed "No message".
- MCP tool errors recorded by the Python SDK carry a message too: the text of the result's first text block, scrubbed
  and cut at 2 KiB. mcp keeps a raised exception's own text on the server, so the error of a tool that raised reads
  "Error executing tool <name>".
- The Python SDK cuts the error messages of MCP spans at 2 KiB on a character boundary, as the TypeScript, Go and
  Rust SDKs and the proxy do. 0.1.3 cut them at 8,192 characters.
- The TypeScript SDK emits one process warning (code `CRUCIBLE_EXPORT`) when an export first fails, naming the traces
  URL, the HTTP status and Crucible's message, for example the 401 for a key the server does not accept, or the
  connection error; later failures count in `stats().failed` without another warning.
- The Go SDK logs one line through the standard `log` package when an export first fails, with the traces URL, the
  HTTP status and Crucible's message, or the connection error; later failures count in `CurrentStats().ExportFailed`.
- The Rust SDK logs one `tracing` warning when an export first fails, with the traces URL and the exporter's error,
  which names the HTTP status; later failures count in `stats().failed`. In 0.1.3 the Go and Rust SDKs said nothing
  when exports on the background interval failed.
- The Rust SDK's manifest declares `rust-version = "1.95"`, the minimum its README names, so cargo older than 1.95
  refuses the crate up front and names the version it needs. In 0.1.3 the build failed partway, with "use of
  unstable library feature `atomic_try_update`".
- The Set up card on Home marks each step from the project's state: Create an API key is done once the project has
  an active key, Send your first trace once spans arrive, Instrument an MCP server once MCP operations arrive on
  either side, and Invite your team once the organization has a second member. A step the viewer's role can't do,
  or that a server without a control plane doesn't offer, says who or what does it instead. In 0.1.3 every step
  stayed "To do".
- Each step to do on the Home Set up card links to where it is done: the project's API keys page, the Members page,
  and the guides to sending traces and to instrumenting an MCP server.
- The Home Set up card comes first on a project that has received no data, and it is hidden once every step is done
  or is one the viewer can't do.
- The web UI's charts are drawn at the size they are shown, so axis labels stay at the label size on a wide chart
  and on a phone. A day or a week is drawn in hours and a month in days; bars have rounded ends, are at most 24 px
  wide, and are added together into whole clock intervals (15 minutes, 30 minutes, an hour) when they would be too
  thin; an empty bucket of a count is zero; and lines break where there is no data instead of drawing across it.
- Hovering or focusing a chart shows each series' value for that bucket in a tooltip beside the pointer, and the arrow
  keys, Home and End step through the buckets; the slider under each chart is gone. The duration axis reads in round
  units ("20 s", "1 min").
- An MCP tool's latency percentiles are drawn as one range per bucket, from p50 to p99 with p90 marked, which stays
  readable when a bucket holds a few calls.
- MCP time series use at most 200 buckets: a day in quarter hours and a week in hours. A day was 288 five-minute
  buckets.
- Password and secret fields show and hide with an eye icon inside the field, level with the input, on the sign-in,
  setup, invitation and reset pages, in the Connect dialog and in the sign-in provider forms. The toggle keeps its
  name, such as "Show password", and reports its state with `aria-pressed`. In 0.1.3 the word "Show" ran past the
  field's edge.
- On the trace page, Add tag sits level with the New tag input, and the score form's fields line up at the top, with
  Add score level with their inputs and the comment box one line tall until it is resized.
- On Home, the sparklines of the five headline tiles sit level at the bottom of the tiles, also when a caption takes
  two lines.
- The trace timeline gives span names three fifths of its width from 640 px up, so a name such as
  `tools/call create_pull_request` shows in full at 1440 px; a name that is still cut, in the timeline or on an agent
  graph node, shows in full on hover.
- Dialogs keep their title and their actions in view: when a dialog is taller than the window, the fields between
  them scroll. In 0.1.3 Cancel and Create rule of New alert rule sat below the dialog's edge at 1440 by 900. On a
  phone, New alert rule and New dashboard keep a margin at both sides.
- The Traces list pages: Load more traces reads the next traces, as the Spans, Sessions and Users lists do. 0.1.3
  showed the newest 50 only.
- An account menu holds who is signed in and Sign out, in place of the name and the Sign out button, so on a phone the
  header takes two rows: the menu, the project and the account menu, then the time range and Feedback.
- The SQL page's schema browser keeps column names on one line, shows an enum column as `Enum8` with its values on
  the line below, and gives the unit of the cost, duration and token columns. Result headers show the short type,
  with the full type on hover.
- The web UI shows a request's code only beside an unexpected failure (a server fault, an unknown error, or an
  answer that is not from Crucible), as "If this keeps happening, give your administrator this code" with a Copy
  code button; a wrong password, a field to correct, a conflict, a missing permission or a rate limit shows none.
  In 0.1.3 every error printed a bare "Reference" id.
- The sign-in, setup, invitation and reset pages have a new layout: two rounded panels fill the window, the form on
  the left, with icons in the email and password fields, and on wide screens a picture of Crucible on the right. On
  the sign-in page, sign-in providers follow the password form under "Or continue with", and a line says how a new
  person gets an invitation. Crucible shows the brand mark it shares with rightmodeler beside its name on these pages
  and in the sidebar, and uses it as the browser tab icon.
- Exports of 129 spans or more complete, over REST and MCP, in both formats. In 0.1.3 their payload lookups named a
  whole page of spans in one request, which ClickHouse refused under its default `http_max_fields` of 1,000, so they
  stayed `queued` and failed with `attempts_exhausted` after about 2.4 hours.
- Every statement Crucible sends stays within ClickHouse's default `http_max_fields`: a read above 950 fields is
  refused before it is sent with `query_too_broad`, and lists longer than 512 values are read 512 at a time.
- An export of up to 10,000 `trace_ids` reads them 512 at a time. In 0.1.3 more than 512 ids failed the page read.
- The trace list filter takes at most 256 values across its members; above that it answers 422
  `validation_failed` with `too_many_values`.
- ClickHouse's refusal of a request with too many fields (`Poco::Exception`, code 1000) is `query_too_broad` and is
  not retried: an export that meets it, or another guardrail refusal, fails at once with its error code.
- `get_export`, over REST and MCP, reports `attempts`, `waited_secs` while the export is queued or running, and
  `last_error`: what went wrong in the latest failed attempt, or why a failed export ended, and what to do.
- Failed attempts of exports, alert evaluations and notification deliveries are logged at warn with their reason.
  In 0.1.3 they were logged at debug, which the server never showed.
- `RUST_LOG` filters the server's log, for example `RUST_LOG=info,crucible_jobs=debug`; unset, the log holds info
  and above. The Compose files pass it from the environment or `.env`.
- `GET /api/v1/projects/{project_id}/traces/{trace_id}` returns `summary`: the trace name, the head span's name,
  status, level, environment and release, the trace's start, duration, span and error counts, user and session, and
  its cost and token totals counted the way the trace list counts them, so a parent span that repeats its children's
  usage is counted once. It covers the whole trace, also when the response is paged, and is null when the response
  holds no span.
- The spans of a trace, the children of a span and search hits carry `input_truncated` and `output_truncated`:
  whether the span's input or output was cut at the size limit, null for a span that has not reached ClickHouse.
- A trace list row's `error_count` counts a span with an error status and an explicit level other than `error` as
  soon as the span is received. In 0.1.3 such a span counted only once ClickHouse stored it.
- The trace page's header shows the same cost, tokens, duration and error count as the trace list: an agent span
  that reports its run's total usage is no longer added on top of its model calls, and a large trace's header covers
  the whole trace. In 0.1.3 the header added up the spans it had loaded.
- The trace page is titled with the trace name when one is set, as the trace list names the trace, with the root
  span's name below it, and its header shows the trace's user and session, each linked to its page, its environment
  and its release. In 0.1.3 the page showed only the root span's name.
- The span panel's Attributes and Events tabs say "No attributes were recorded.", "No events were recorded.",
  "This span's attributes aren't stored." and "This span's events aren't stored.", in place of "was recorded" and
  "isn't stored".
- The span panel's Input and Output tabs say "The input was cut at the size limit, so it shows as text." only for a
  field the span reports as cut, and label other text "Text". In 0.1.3 every text value carried a notice that it
  might be JSON cut at the size limit.
- The trace page's timeline fits its card beside the span panel: its columns follow the timeline's own width instead
  of the window's. In 0.1.3 the Duration column was cut off in a window 1024 px wide.

## [0.1.3] - 2026-11-26

Crucible stores telemetry on a self-hosted replicated ClickHouse cluster as well as on a single server, and the SDKs
have the server's version and ship as packages with each release.

### Clustered ClickHouse

- Crucible runs on a self-hosted replicated ClickHouse cluster of one shard with several replicas and ClickHouse
  Keeper: `clickhouse.cluster` (`CRUCIBLE_CLICKHOUSE_CLUSTER`, `--clickhouse-cluster`) names the cluster, every
  table, user and row policy is created on every replica, each insert is stored on a majority of the replicas, and
  deletions count as applied and verified only once every replica has run them. ClickHouse Cloud is still refused at
  startup ([clustered ClickHouse](https://github.com/elm-os/crucible/blob/v0.1.3/docs/self-hosting/clustered.md)).
- `deploy/compose/compose.cluster.yaml` runs Crucible with two ClickHouse replicas, ClickHouse Keeper and Postgres,
  and `compose.cloud.yaml` passes `CRUCIBLE_CLICKHOUSE_CLUSTER` to the server.
- With `clickhouse.cluster` set, a start refuses a cluster that cannot hold the tables (an unknown cluster, several
  shards, missing macros, no Keeper, `distributed_ddl_use_initial_user_and_roles` off, no cluster secret, or tables
  created for a single server on the replica that `clickhouse.url` names) with the fix, and a migration step while a
  replica is down fails without being recorded and names the replica.

### SDKs

- The four SDKs have the server's version (the TypeScript SDK said 0.0.0 and the others 0.1.0), so the version in an
  SDK's User-Agent names the Crucible release it shipped with.
- Each release carries the SDK packages on its GitHub release: `elm-os-crucible-X.Y.Z.tgz` for TypeScript,
  `crucible_sdk-X.Y.Z-py3-none-any.whl` and `crucible_sdk-X.Y.Z.tar.gz` for Python and `crucible-sdk-X.Y.Z.crate`
  for Rust, and tags the Go module as `sdk/go/vX.Y.Z`.
- The SDK guides and READMEs install from the release: the package downloads, the Go module tag and, for Rust, a git
  dependency pinned to the release tag.
- The SDK packages and the Go module carry the MIT license text.

### Compatibility

- On ClickHouse 26.11 and newer, which refuse to kill a query that has already ended, cancelling a finished SQL
  workspace query succeeds again, as on earlier versions, and a read abandoned after its query ended no longer logs
  a failed kill.

### Upgrading

- A deployment of 0.1.2 on a single ClickHouse server upgrades with the new image and a restart, as
  [operations](https://github.com/elm-os/crucible/blob/v0.1.3/docs/self-hosting/operations.md#upgrades) describes:
  0.1.3 adds no Postgres migration and no ClickHouse migration step, changes none of the steps that 0.1.2 recorded,
  and needs no new setting or grant, so its first start applies nothing. From an older version the Upgrading notes
  of each version in between apply as well: from 0.1.1, the first migration runs the copy of migration step 0005
  that the notes of [0.1.2](https://github.com/elm-os/crucible/blob/v0.1.3/CHANGELOG.md#012---2026-11-25) describe.
  The `compose.cloud.yaml` of 0.1.3 passes `CRUCIBLE_CLICKHOUSE_CLUSTER` from `.env`; left out or empty, it keeps
  Crucible on one server.
- A single-server deployment goes back to 0.1.2 with the 0.1.2 image and a restart, since 0.1.3 migrates neither
  Postgres nor ClickHouse.
- An existing single-server deployment cannot be switched to a cluster in place. Once its ClickHouse server is a
  replica in a cluster, a start with `clickhouse.cluster` refuses the tables it holds, which were created for a
  single server: it names them and says to unset `clickhouse.cluster` or to point `clickhouse.url` at a cluster
  whose database `crucible` holds no tables. Crucible does not convert such tables or copy their rows.
- To run on a cluster, start a new deployment, with a Postgres database of its own, on a cluster whose replicas hold
  no tables in database `crucible` yet; Crucible looks for such tables only on the replica that `clickhouse.url`
  names. With the single-server deployment's Postgres, which records the migration steps applied to the single
  server, a start does not create the tables on the cluster, and `crucible migrate` exits with 1 saying that the
  ClickHouse service users refuse the master key. The new deployment starts without the single-server deployment's
  telemetry, projects, users and API keys, which stay with that deployment.
- Give a cluster all its replicas before Crucible's first start on it, as
  [adding a replica](https://github.com/elm-os/crucible/blob/v0.1.3/docs/self-hosting/clustered.md#adding-a-replica)
  says: Crucible creates its tables, users and row policies on the replicas in the cluster when each migration step
  applies, and not on a replica added later or rebuilt from an empty disk. A migration step also needs every replica
  up: a start with one down stays not ready, names it, and applies the step once it answers.
- The SDK packages now carry the server's version. The SDKs of this release, from its package downloads, the Go
  module tag `sdk/go/v0.1.3` or the git tag `v0.1.3` for Rust, are version 0.1.3 and send it in their User-Agent,
  which Crucible records as the SDK version of the spans they export. SDKs of earlier releases keep working with
  0.1.3 and report the version they were built with.

## [0.1.2] - 2026-11-25

MCP analytics over long windows read an hourly rollup, the web UI manages the organization and its sign-in
providers, the MCP proxy records the tool arguments and results you opt in to, export files can go to object storage,
and dashboard widgets and search take filters. The upgrade copies the MCP rollup once and goes back only through a
backup: read Upgrading below before you upgrade.

### MCP analytics

- MCP analytics over 12 hours or more (the MCP pages, their API routes and MCP tools, `mcp_ops` metrics and alert
  rules) read whole hours from a new hourly rollup, `mcp_tool_rollup_hourly`, which the shipper writes beside the
  minute rollup, and only the minutes at the window's edges from the minute rollup; the answers are the same.
- Migration step `0005_mcp_tool_rollup_hourly` adds the hourly rollup and copies the existing minute rollup into it
  once, the first time this version migrates (a start with `--migrate`, or `crucible migrate`).

### Sign-in and organization

- Settings has an Organization page for the organization's name, project access (every project, or only the projects
  someone is added to) and the allowed email domains, whose people join as viewers without an invitation once their
  sign-in provider verifies their address. Members and viewers read it; editing needs a screen at least 768 pixels
  wide.
- Settings has a Sign-in page that adds, edits, turns off and deletes GitHub, Google and OpenID Connect sign-in
  providers, replaces a client secret without ever showing the stored one, and copies the callback URL to register
  with the provider.
- Crucible refuses a change that would leave nobody able to sign in: turning off password sign-in while the sign-in
  page offers no provider, and turning off or deleting the last provider while password sign-in is off. The refusal
  (409 `conflict`) says what to turn on first.
- The Organization page has a password sign-in switch, and shows the refusal when password sign-in is the only way in.
- `crucible admin enable-password-login` turns password sign-in back on from the shell, for a deployment whose sign-in
  provider no longer signs anyone in; the server logs at start when nobody can sign in.
- Sign-in providers carry a version: `PATCH` and `DELETE org/auth-providers/{provider_id}` need it in `If-Match`, and
  the Sign-in page sends it and reloads a provider that changed meanwhile. Postgres migration
  `V11__auth_provider_version` adds the column.
- A sign-in provider's client ID and scopes, and an OpenID Connect provider's issuer, can be changed in place. An
  empty client secret is refused and never replaces the stored one, and a new issuer unlinks the provider's
  accounts, which link again through their verified email.
- A provider's allowed email domains apply at every sign-in, also to accounts linked before the list changed, and the
  sign-in page names the domains the provider allows. An account outside its provider's allowed domains that signed
  in on 0.1.1 is refused after the upgrade: add its domain to the provider, or have the person sign in another way.
- Sign-in providers can be declared in the configuration (`[auth_github]`, `[auth_google]`, `[auth_oidc]`), with
  their client secrets in files or environment variables. They are read-only on the Sign-in page and in the API, and
  removing one from the configuration turns it off, except while it is the last way to sign in: then its row stays on
  and the server logs at start what to do. A new `auth_oidc.issuer` unlinks the provider's accounts, as an edit on the
  Sign-in page does.

### MCP proxy and SDKs

- `crucible proxy` records the arguments and results of the tool calls a `--capture` rule (or
  `CRUCIBLE_PROXY_CAPTURE`) names, with the SDKs' rules: off by default, sampled per call, scrubbed with the shared
  rules and cut at 16 KiB per field unless the rule sets `max_bytes`, keeping no more than the cap plus 1 KiB of a
  value, and never on the forwarding path.
- The Python SDK encodes captured content without a step per JSON token: a result of 500 records took 0.4 ms
  instead of 5.2 ms per call on a laptop, with the same text as before.
- The TypeScript SDK writes runs of plain records with one serializer call and skips a scrub rule on text that lacks
  a pair of characters every match of it holds (a hexadecimal digit before a `:` for the ipv6 rule), so capturing
  500 records or 64 KiB of hexadecimal text costs about half as much, measured on a laptop.
- The Rust SDK's `flush` waits at most the export timeout in all, for the spans and the queued scores together; it
  waited as long again for the scores.
- The SDK conformance driver checks that Crucible keeps each span's captured content as sent, and a capability
  manifest can name the proxy as the sender (`"source": "proxy"`); the proxy passes the content-capture scenario in
  all four variants.
- The Python, Go and Rust SDKs skip a scrub rule on text that lacks a pair of characters every match of it holds, as
  the TypeScript SDK does (a digit before a `.` for the ipv4 rule), with the same scrubbed text as before.
- The Python SDK also skips a scrub rule on text without a run of the characters its matches are made of as long as
  its shortest match (14 digits, spaces and dashes for the card rule), so each shape of the capture-overhead scenario
  adds less than 1 ms per call on a laptop (64 KiB of hexadecimal text 0.6 ms instead of 1.3 ms), with the same
  scrubbed text.
- `crucible proxy` refuses a `--capture` rule whose `max_bytes` is above 8,323,072 (8 MiB less 64 KiB) and says so:
  a field captured at a larger cap might not fit the proxy's 8 MiB export buffer with its operation, which would then
  be dropped.

### Media and exports

- With object storage, export files go to the bucket under the project's prefix, and their download answers 303 with
  a presigned link that any server hands out; expiry and deletion requests remove the objects.
- The span panel has a Media tab, in place of the media section under each payload tab, that lists the media a span's
  input, output and metadata reference, with where each appears, and the media uploaded for the span; without object
  storage it says so and links the media guide.
- `GET projects/{project_id}/spans/{trace_id}/{span_id}/media` lists the media a span references.
- With `egress.mode = strict`, alert channels and webhooks cannot target the object store's endpoints.
- SeaweedFS and Garage join the Versity S3 gateway as tested object stores, and the media guide says what each one
  needs.
- With object storage, one server at a time sweeps the export objects, the one that runs the media collector, so a
  deployment lists every project's export keys once every 10 minutes, however many servers it has.

### Dashboards and search

- Each project runs at most 4 metrics queries at once on a server (`metrics.queries_per_project`), so a dashboard of
  many widgets takes turns: the other queries wait within their deadline instead of reaching ClickHouse together.
- A read made with an API key or through the MCP server that ClickHouse refuses because several servers together run
  as many such reads as it allows (`TOO_MANY_SIMULTANEOUS_QUERIES`) waits and is sent again within its deadline,
  instead of answering 503.
- A time series widget can draw as areas, beside lines and bars: a light fill under each series' line, with its
  dash, marker and table view.
- The widget sheet edits a widget's filters: a field of the metrics schema, is or is not, and a value, with the
  values the time range holds offered as suggestions.
- Search takes the filters of the span list, name included, as `filters` in its request, and the search page has the
  lists' filter bar. The lists, search and the MCP server's `list_traces` and `list_spans` read one filter object
  and refuse an invalid one alike: the same member, with the same code.

### Deletion

- Deleting a project deletes its dashboards with their widgets and its media records, and then every object under
  the project's key prefix in object storage.

### Ingest and storage

- The metric `crucible_shipper_task_microseconds_total{phase}` gives the shipper's time by what it was doing:
  reading the log, normalizing, building rows, encoding blocks, writing its own log records, waiting for inserts and
  idle.
- The shipper finds the first span of each new trace in one pass over a request. A request of 50,000 traces of one
  span each, whose parents it does not carry, as a service that exports only child spans sends, took 3.1 times as
  long to turn into rows as a request of one trace of 50,000 spans, and takes about 1.2 times as long, measured on a
  laptop.
- The shipper reads a request as soon as it is durable instead of looking for new requests every 5 ms, so at low load
  trace reads see a span up to 5 ms sooner after its acknowledgement.

### Migrations and startup

- The server that applies a migration step renews the migrations lease every 5 seconds while the step runs, so a
  server that starts meanwhile waits for it, also during a step that takes longer than the lease's 15 seconds, such
  as the copy of step 0005.
- A migration step whose content changed, such as steps 0001 to 0003 on an upgrade from 0.1.1, applies once and the
  ledger records its new content; it no longer applies again, with a warning, at every start.
- A start whose master key the ClickHouse service users refuse leaves a changed migration step for a start with the
  right key, so it no longer gives the SQL workspace's probe users that key's passwords.
- With Postgres, a server started without `--migrate` stays not ready and refuses ingest until the migrations ledger
  records every step of its version, and says to run `crucible migrate` or to start one server with `--migrate`.
- `crucible migrate` exits with 1 when a changed migration step waits for the master key the deployment was migrated
  with, and names the steps, why the key is refused and the key to run it with, instead of reporting the migrations
  as applied.
- A listener whose address is taken names the setting that moves it, for example `server.metrics_listen`
  (`CRUCIBLE_SERVER_METRICS_LISTEN`).

### Dependencies

- The `url` crate is 2.5.9, which fixes RUSTSEC-2026-0592: in 2.5.8, `Url::origin()` could overflow the stack on a
  nested `blob:` URL. Crucible calls it only on the object store endpoint from its own configuration, never on input
  from a request.

### Help and feedback

- The web UI's header has a Feedback menu whose two links open on GitHub in a new tab: "Ask a question or suggest an
  idea" (Discussions) and "Report a bug" (the issue templates). They send no referrer and add nothing to the address.
- `SUPPORT.md` and the docs page `docs/getting-help.md`, linked from the README, the docs index and `CONTRIBUTING.md`,
  say where to ask a question (Discussions Q&A), suggest an idea (Discussions Ideas) or report a bug (an issue), and
  what a bug report needs.
- The new issue page links to Discussions Q&A and Ideas and offers a blank issue to maintainers only, and the feature
  request form sends broad ideas to Discussions Ideas first.

### Upgrading

- Migrations only move forward: 0.1.1 started with `--migrate`, as the Compose files start it, stays not ready on a
  database that 0.1.2 has upgraded. Take a
  [backup](https://github.com/elm-os/crucible/blob/v0.1.2/docs/self-hosting/operations.md#backups) before the upgrade;
  going back means restoring it.
- The first migration of 0.1.2, at a start with `--migrate` or with `crucible migrate`, runs ClickHouse migration step
  0005, which copies the minute MCP rollup into the new hourly rollup once; a server that runs it stays not ready and
  refuses ingest until the copy has finished. The copy takes longer the more rows the minute rollup holds, and
  ClickHouse ends it after an hour:
  [operations](https://github.com/elm-os/crucible/blob/v0.1.2/docs/self-hosting/operations.md#upgrades) gives a rate
  to plan with, and says what to do when a copy does not finish.
- Leave the server running until the copy has finished: a copy cut off by a stop or a restart leaves step 0005
  unfinished, and the next start stays not ready until you end the copy and empty the table, as operations says.
  With the Compose files the `ready` service reports unhealthy about three minutes into a longer copy, while the
  copy goes on; follow `docker compose logs crucible` instead of restarting.
- Stop every 0.1.1 server first: an MCP call that 0.1.1 stores after the copy reaches only the minute rollup, so
  reads over 12 hours or more leave it out. Then start one server with `--migrate`, as the Compose files do, or run
  `crucible migrate`, before the other servers. With Postgres, a server started without `--migrate` stays not ready
  and refuses ingest with 503 until the migrations ledger records step 0005.
- With Postgres, the first migration also applies ClickHouse migration steps 0001 to 0003 once more, because they
  now cover the new table, and warns for each that the ledger records it with other content; later starts apply
  nothing.
- Without Postgres there is no migrations lease and no ledger to wait for: start the other servers only once the
  first one answers ready, so that none ships or migrates while it copies.
- A sign-in provider's allowed email domains now apply at every sign-in, so an account outside them that signed in
  on 0.1.1 is refused. Add the domains such accounts use to the provider, on the Sign-in page of Settings or in
  `allowed_domains` of its configuration section, or have those people sign in another way.
- `PATCH` and `DELETE org/auth-providers/{provider_id}` now need `If-Match` with the provider's `version`, and
  answer 428 `precondition_required` without it: a script that changes or deletes sign-in providers through the API
  has to read the provider first and send its version.
- With object storage, new export files go to the bucket from the first start of 0.1.2, and their download answers 303
  with a presigned link, so a script that downloads them has to follow redirects (`curl -L`). The store has to accept
  multipart uploads, and a lifecycle rule for incomplete multipart uploads removes what a stopped server leaves
  ([media](https://github.com/elm-os/crucible/blob/v0.1.2/docs/self-hosting/media.md#export-files)). Export files
  written before stay on the server's disk until they expire.
- Each project now runs at most 4 metrics queries at once on a server (`metrics.queries_per_project`). A widget that
  gets no turn within its deadline, 12 seconds for most widgets, answers 504 `query_timeout`: on dashboards of many
  slow widgets, raise the setting.

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
