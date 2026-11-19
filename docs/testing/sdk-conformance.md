# SDK conformance

The four milestone 3 SDKs (TypeScript, Python, Go, Rust) are tested against one language-neutral suite. The suite
checks what an SDK puts on the wire: the OTLP spans it exports, their attributes and resource, and the export
request itself, against the wire contract of contracts 9.1 to 9.10 and the source keys of 10.6.

| Piece | Where | What it is |
|---|---|---|
| Collector | `crates/crucible-fake-collector` (library) | An OTLP/HTTP receiver on a loopback port that records every request and span |
| Driver | `crucible-sdk-driver` (binary of the same crate) | Runs every case through an SDK's runner, diffs the spans against the goldens, writes the matrix report |
| Suite constants | `conformance/suite.json` | Keys, client and server identity, protocol versions, SDK options |
| Key registry | `conformance/registry.json` | Every wire key with its type, the derived fields, forbidden keys, hash rules and the rules that apply to every span |
| Scenarios | `conformance/scenarios/*.json` | What the runner does, one feature per file |
| Goldens | `conformance/goldens/*.json` | What the driver expects, one file per scenario, plus `common.json` |
| Scrub rules | `conformance/scrub-rules.json` | The shared content scrubbing rules with test vectors (contract 9.10) |
| Capability manifest | `conformance/capability-manifest.schema.json`, `conformance/sdks/*.json` | What one SDK declares about itself |
| Self-test | `crucible-sdk-driver selftest-runner` | A Rust runner that emits the expected spans directly, so the suite runs before any SDK exists |

This page is the runner contract, version 1. SDK runners code against it; a change to it bumps the `schema` strings
below.

## Running the suite

```sh
cargo build -p crucible-fake-collector
target/debug/crucible-sdk-driver run --manifest conformance/sdks/selftest.json
target/debug/crucible-sdk-driver run --manifest sdk/typescript/conformance/manifest.json --case mcp-echo
```

| Option | Default | Meaning |
|---|---|---|
| `--manifest <path>` | required | The SDK's capability manifest; its `runner` field is the command to run |
| `--conformance <dir>` | `conformance` | The suite directory |
| `--out <dir>` | `target/sdk-conformance` | Where `<sdk>.jsonl` and `<sdk>.md` are written |
| `--case <id>` | every case | Run only cases whose id equals this value or starts with `<value>@`; repeatable |
| `--timeout-secs <n>` | 60 | Per-case limit; the runner is killed when it passes |
| `-- <argv...>` | the manifest's `runner` | Replaces the runner command, for example to run a local build |

The exit code is 0 when no case fails (declared skips are allowed), 1 when a case fails or errors, and 2 when the
suite, the manifest or the command line is invalid. Every refusal names the file and field to fix.

## Cases

A scenario is one feature. MCP scenarios list the variants they run in; each variant fixes an era and a transport:

| Variant | Era | Transport | Protocol version |
|---|---|---|---|
| `legacy-stdio` | legacy | stdio | `2025-11-25` |
| `legacy-http` | legacy | streamable HTTP, session based | `2025-11-25` |
| `modern-stdio` | modern | stdio | `2026-07-28` |
| `modern-http` | modern | streamable HTTP, stateless | `2026-07-28` |

A case is one scenario in one variant, named `<scenario>@<variant>` (for example `mcp-fail@legacy-http`). Agent
scenarios have no variant, so their case id is the scenario id.

## The runner contract

### Invocation

For each case the driver starts the runner command (`runner` in the manifest, resolved against the driver's working
directory, which is the repository root in CI) with no extra arguments. The program name `crucible-sdk-driver` always
means the running driver executable. The driver:

1. writes one JSON document, the run spec, to the runner's stdin and closes stdin;
2. reads the runner's stdout as JSON lines (below) and keeps the last 4 KiB of stdout and stderr for the report;
3. waits for the runner to exit, killing it after the case timeout;
4. waits until the collector has received nothing for 200 ms (at most 2 s), then evaluates the case.

The runner exits 0 after it has played every step and the SDK has shut down: the agent program has returned from
`shutdown`, and for MCP cases the reference server process has exited after flushing. Any other exit, or a timeout,
makes the case an `error`; its findings are still reported.

### The run spec

```json
{
  "schema": "crucible.conformance.run/v1",
  "case": "mcp-echo@modern-http",
  "scenario": "mcp-echo",
  "kind": "mcp",
  "era": "modern",
  "transport": "streamable_http",
  "protocol_version": "2026-07-28",
  "otlp": { "endpoint": "http://127.0.0.1:41234", "traces_url": "http://127.0.0.1:41234/v1/traces" },
  "checkpoint_url": "http://127.0.0.1:41234/conformance/checkpoint",
  "keys": { "public_key": "cpk_conformance...", "secret_key": "csk_conformance..." },
  "sdk": {
    "service_name": "crucible-conformance-server",
    "deploy_id": "conformance-deploy-1",
    "flush_interval_ms": 60000,
    "capture": [{ "tool": "secret", "args": true, "result": true, "sample_rate": 1.0, "max_bytes": 16384 }],
    "handle_params": { "lookup_order": "order_id" }
  },
  "server": { "name": "crucible-conformance-server", "version": "1.2.0" },
  "client": { "name": "crucible-conformance-client", "version": "1.0.0" },
  "steps": []
}
```

- `era`, `transport`, `protocol_version` and `server` are present for MCP cases only. Agent cases have
  `sdk.environment` and `sdk.release` instead of `sdk.deploy_id`, `capture` and `handle_params`.
- `sdk` holds the options of the SDK's `init` (design surface: endpoint, keys, service name, environment, release,
  deploy id, flush interval) plus the MCP options `capture` (content capture rules) and `handle_params` (tool name to
  the argument whose value is the handle id, contract 9.8). The SDK exports to `otlp.endpoint` with the keys in
  `keys`. `flush_interval_ms` is long on purpose: spans must leave only through `flush`, `shutdown` or a full batch.
- `steps` are fully resolved: the driver has added the legacy handshake, merged the modern `_meta` fields and
  computed the HTTP headers. The runner adds nothing of its own to a message.
- `otlp` and `checkpoint_url` point at a collector the driver starts for this case alone, so spans that arrive
  after a case has ended never count toward the next one.

### MCP steps

The runner starts a fresh reference server for every case and plays the steps in order against it.

| Step | Meaning |
|---|---|
| `{"step": "s", "send": <message>, "headers": {...}, "wait": true}` | Send one JSON-RPC message. For a request with `wait` true (the default), wait for its response before the next step. With `wait` false, continue at once |
| `{"step": "s", "await": "<step>"}` | Wait for the response of an earlier request sent with `wait` false |
| `{"step": "s", "sleep_ms": 300}` | Pause |
| `{"step": "s", "close": "<step>"}` | HTTP only: abort the in-flight exchange of that step (close its response stream). This is modern-era cancellation |

Template values: any JSON object of the exact form `{"$from": "<step>/<pointer>"}` inside `send` is replaced, at
send time, by the value that the RFC 6901 pointer selects in the response message of that step. The MRTR retry uses
it to echo `requestState` byte for byte.

**stdio.** The runner launches the reference server as a child process, writes each message as one line of compact
JSON to its stdin, and reads its stdout as JSON lines: a line with an `id` and `result` or `error` is a response;
every other line is a server message for the request in flight (progress notifications). After the last step the
runner closes the server's stdin and waits for it to exit.

**Streamable HTTP.** The runner starts the reference server on `127.0.0.1` (any free port) with its MCP endpoint at
`/mcp`, and sends every message as its own `POST` with `Content-Type: application/json`,
`Accept: application/json, text/event-stream` and the step's `headers`. When the response to the `initialize` step
carries an `MCP-Session-Id` header, the runner sends that header on every later request of the case. A response is
either one JSON object or an event stream whose `data` lines each hold one JSON-RPC message; the message with the
request's id is the response. A notification is answered 202 with no body. After the last step the runner abandons
any exchange still open and stops the server in whatever way lets the SDK flush (for example a shutdown signal or an
in-process stop), then waits for it.

A request that a later step cancels (by `notifications/cancelled` or `close`) is never awaited.

### Runner events

The runner prints one JSON object per line on stdout. Other stdout lines are ignored and kept for the report.

```json
{"event": "response", "step": "echo", "http_status": 200, "session_id": null, "message": {"jsonrpc": "2.0", "id": 2, "result": {}}, "notifications": 0}
```

One `response` event is printed for every `send` step that completed: `message` is the JSON-RPC response (`null`
for a notification), `http_status` is the HTTP status (`null` on stdio), `session_id` is the `MCP-Session-Id`
response header (`null` when absent), and `notifications` counts the server messages that arrived for the request
before its response. The driver reads `session_id` and `result.requestState` from these events to compute the hashes
it expects, and it fails the case if either raw value appears anywhere in an export.

### Agent steps

For agent cases the runner is the agent program: it calls the SDK in process.

| Step | SDK call |
|---|---|
| `{"step": "s", "observe": <node>}` | Start a span for the node, apply its fields, run its children in order inside it, end it |
| `{"step": "s", "flush": {}}` | `flush()`, waiting until it returns |
| `{"step": "s", "checkpoint": "<name>"}` | `POST {"name": "<name>"}` to `checkpoint_url` and wait for its 204. The collector orders checkpoints against exports, so a golden can require that a span arrived before it |
| `{"step": "s", "shutdown": {}}` | `shutdown()`, waiting until it returns |

A node is `{"name", "type", "children"}` plus any of the fields below. Each field maps to the keys of contract 10.6;
where the contract offers alternatives, the goldens accept any of them.

| Node field | Wire keys |
|---|---|
| `name`, `type` | Span name; `crucible.observation.type` (10.4 names) |
| `trace_name` | `crucible.trace.name` |
| `context` | `session_id`, `user_id`, `tags` and `metadata`, set with the context setters before the span starts. Every span started inside carries `crucible.session.id` (or `session.id`), `crucible.user.id` (or `user.id`), `crucible.tags` (string array) and `crucible.metadata.<key>` (string, int, double or bool) |
| `input`, `output` | `crucible.input`, `crucible.output`: a string verbatim, any other JSON value as JSON text |
| `model`, `provider` | `gen_ai.request.model`, `gen_ai.provider.name` |
| `input_messages`, `output_messages` | `gen_ai.input.messages`, `gen_ai.output.messages` as JSON text |
| `finish_reasons` | `gen_ai.response.finish_reasons` (string array) |
| `usage` | `gen_ai.usage.<key>` (int), for example `input_tokens`, `cache_read.input_tokens` |
| `cost_usd` | `crucible.cost.<key>`, USD as a decimal string or a double |

`sdk.service_name` is the resource `service.name`; `sdk.environment` is `crucible.environment` or the resource
`deployment.environment.name`; `sdk.release` is `crucible.release` or the resource `service.version`. A span started
after `shutdown` must not raise and must not be exported.

## The reference server

Each SDK ships a small MCP server built on its language's official MCP server library (2.x lines, contract 9.10) and
instrumented by the SDK with the run spec's `sdk` options, identifying itself with `server.name` and
`server.version`. It is dual era: an `initialize` request opens a legacy session, a request carrying the modern
`_meta` fields is served statelessly. It configures no `identify` hook, and declares the tools below.

| Tool | Arguments | Behaviour |
|---|---|---|
| `echo` | `text` | Returns one text block holding `text` |
| `fail` | `reason` | Calls `annotate_error("upstream")`, then returns one text block holding `reason` with `isError: true` |
| `throw` | `message` | Raises an exception with `message` through the language's normal error path. The manifest declares the resulting outcome |
| `slow` | `ms`, `steps` | When the request carries `_meta.progressToken`, sends `steps` progress notifications spread over the wait; waits `ms` milliseconds in total (stopping early when cancelled); returns one text block `done` |
| `secret` | `note` | Returns one text block holding `note` unchanged. The suite plants secrets in `note`; content capture is on for this tool, so the SDK must scrub both the arguments and the result |
| `needs_input` | `item` | Without `requestState`: returns `resultType: "input_required"` with `requestState: "crucible-conformance-request-state-1"` and one form elicitation keyed `confirm` asking for a boolean `confirmed`. With `requestState` and an accepted `confirm` response: returns one text block `confirmed <item>`. Modern era only |
| `lookup_order` | `order_id` | Returns one text block `order <order_id>: shipped`. The SDK takes the handle id from `order_id` (`handle_params`) |

When the MCP library seals or rewrites `requestState`, the runner reports the value it observed and the driver uses
that value, so the hash expectation still holds.

## Scenario format

```json
{
  "schema": "crucible.conformance.scenario/v1",
  "id": "mcp-echo",
  "kind": "mcp",
  "title": "tools/list and echo calls",
  "variants": ["legacy-stdio", "legacy-http", "modern-stdio", "modern-http"],
  "steps": [
    { "step": "list", "send": { "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} } },
    { "step": "echo", "send": { "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "echo", "arguments": { "text": "hello" } } } }
  ]
}
```

Scenario steps are era neutral. Before the runner sees them the driver resolves each case:

- **Legacy.** The case starts with an `initialize` request (step `initialize`, JSON-RPC id 0, the suite's protocol
  version, client info and capabilities) and a `notifications/initialized` notification (step `initialized`).
- **Modern.** Every request gets `params._meta` entries `io.modelcontextprotocol/protocolVersion`,
  `io.modelcontextprotocol/clientInfo` and `io.modelcontextprotocol/clientCapabilities`; keys a scenario puts in
  `_meta` (`progressToken`, `traceparent`) are kept.
- **HTTP headers.** Modern requests carry `MCP-Protocol-Version`, `Mcp-Method` and, for `tools/call`,
  `resources/read` and `prompts/get`, `Mcp-Name` (from `params.name` or `params.uri`). Legacy requests after
  `initialize` carry `MCP-Protocol-Version`.
- **Cancellation.** A scenario step `{"step": "c", "cancel": "<step>", "reason": "..."}` becomes a
  `notifications/cancelled` notification on stdio and on legacy HTTP, and `close` on modern HTTP (2026-07-28: closing
  the response stream is the cancellation signal; 2025-11-25: a disconnect is not a cancellation).
- **Era filter.** A step with `"eras": ["legacy"]` is kept only in that era (`notifications/roots/list_changed` no
  longer exists in the modern era).

The loader refuses a suite whose resolved cases break these rules: duplicate step ids or JSON-RPC ids, id 0 outside
the handshake, a `wait`, `await`, `close` or `$from` that names a missing or later step, `close` on stdio, modern
`_meta` or headers that disagree with the body, or an MRTR retry that reuses an id.

## Goldens

A golden lists the spans a case must produce. The driver matches every expectation to exactly one span, then checks
it. In MCP cases every span that carries `mcp.method.name` must match exactly one expectation; in agent cases every
span must. An unmatched span is a failure (double emission shows up this way).

```json
{
  "schema": "crucible.conformance.golden/v1",
  "scenario": "mcp-fail",
  "spans": [
    {
      "id": "fail",
      "select": { "mcp.method.name": "tools/call", "jsonrpc.request.id": "2" },
      "kind": "server",
      "attributes": { "gen_ai.tool.name": "fail", "error.type": "tool_error", "crucible.mcp.error.subclass": "upstream" },
      "outcome": { "status": "tool_error", "error_class": "tool_execution", "error_subclass": "upstream" }
    }
  ]
}
```

| Expectation field | Meaning |
|---|---|
| `id` | Name used in reports, `parent` and `checkpoints` |
| `select` | Attribute values (or `name` for the span name) that pick the span |
| `when` | Applies the expectation only in some cases: an object whose conditions must all hold, or a list of such objects of which one must hold. Conditions test `era`, `transport` or a manifest capability (`throw_outcome`, `progress_count`) against a value or a list of values |
| `kind` | Span kind (`server`, `client`, `internal`, ...) |
| `parent` | `null` for a root span, or the `id` of the expectation whose span is the parent (same trace) |
| `trace_id`, `parent_span_id` | Literal hex ids, for propagated trace context |
| `min_duration_ms` | Lower bound on the span's duration (timing is never bounded above) |
| `attributes` | Key assertions, below |
| `fields` | Derived-field values: the record field the normalizer would compute from the registry's source list |
| `outcome` | MCP classification (`status`, `error_class`, `error_subclass`, `caller_fault`); defaults to `{"status": "ok"}` |

Key assertions: a JSON literal (equality for strings, booleans and string arrays, and by value for numbers, so an
integral double equals an integer; types are checked separately against the registry; a string starting with `$`
names a case variable such as `$era`, `$server_name` or `$release`), `{"absent": true}`,
`{"present": true}`, `{"matches": "<regex>"}`, `{"contains": [...]}`, `{"json": <value>}` (the string parses to an
equal JSON value), `{"usd": "0.0042"}` (equal in nano-USD after contract 3.4 rounding), `{"mrtr_hash": "<step>/<pointer>"}`
(the 9.6 hash of the observed value), `{"session_hash": "<step>"}` (the 9.4 hash of the session id observed on that
step) and `{"optional": <assertion>}` (checked only when the key is present).

A golden may also have `every_span` (assertions applied to every span of the case), `absent` (selectors that must
match no span), `checkpoints` (checkpoint name to the expectation ids whose spans must arrive before it) and
`forbidden_strings` (`{"label", "value"}` pairs that must not appear in any export). `common.json` holds the
expectations the driver adds for the legacy handshake.

### What the driver checks in every case

- **Registry rules.** `registry.json` types every known key (unknown keys are allowed), lists the forbidden keys and
  prefixes, and holds `mcp_rules` (applied to every MCP span) and `agent_rules` (applied to every agent span), for
  example: `capture_source` is `sdk`, era and transport match the case, `mcp.session.id` is absent in the modern era,
  `jsonrpc.request.id` is absent exactly on notifications, content keys are absent unless content was captured.
- **Classification.** The outcome comes from `crucible_records::classify_mcp_error` (contract 9.2) with these inputs
  read from the span: `rpc.response.status_code` (else a numeric `error.type`) as the JSON-RPC code, `error.type =
  tool_error` as `isError`, `http.response.status_code`, `crucible.mcp.cancel.source`, `crucible.mcp.timed_out`, a
  non-numeric `error.type` other than `tool_error` as an unhandled exception, and `crucible.mcp.error.class` and
  `.error.subclass` as the SDK's annotations.
- **Export.** Every request goes to `/v1/traces` as `application/x-protobuf` (gzip allowed) with
  `Authorization: Bearer <secret key>` or `Basic base64(public:secret)` (contracts 9.10 and 12.2), and decodes.
  Spans with all-zero ids are failures.
- **Taint.** No export body (decompressed, canonical protobuf) contains the secret key, the Basic credential, a
  planted secret, a raw session id or a raw `requestState`.
- **Every span.** No span, resource or event attribute is a forbidden key, and every key the registry knows has its
  registry type, value set, pattern and bounds. Keys the registry does not know are allowed.

## Scrub rules

`conformance/scrub-rules.json` is the one rule list the four SDKs and the proxy apply to captured arguments, results
and error messages (contract 9.10). Each rule has a `name`, a `pattern`, a `replacement` and optionally a
`validator` (`luhn`), plus `positive` inputs the rule must change and `negative` inputs it must leave alone; each
`example` is the output of the whole list. Rules run in file order, each replacing every non-overlapping match,
leftmost first; after a match the validator rejects, the search resumes after the match's first character that is
neither an ASCII letter nor a digit (after the match when it has none), so a rejected run of digit groups cannot hide
a card that starts at a later group.
Patterns stay in a subset that matches the same way in RE2, Go, Rust, ECMAScript and Python, and in linear time in the
first three (the backtracking engines of ECMAScript and Python stay polynomial on these patterns): explicit ASCII classes instead of `.`, `d`, `w`, `s`
and ``; no anchors, inline flags, lookaround, backreferences or capturing groups; no lazy or possessive
quantifiers; counts up to 1,000; and no unbounded quantifier on a group that holds one. The driver refuses a rules
file that breaks the subset or any of its vectors, so an SDK can load the file and trust it.

## Capture overhead

`conformance/capture-overhead.json` is the capture-overhead scenario: what opt-in content capture (contract 9.10)
may cost per call. It is not a driver case. Each SDK plays it in process with its own test, because it reads work
counts through a hook inside the SDK.

- Each SDK serves one tool, `shape`, from two in-process instances of a server it instruments: one without capture
  rules and one capturing the tool's arguments and result at `max_bytes`. Calls alternate between the two in paired
  rounds whose order flips every round.
- The result shapes: 200 B of text, text at the cap, 500 short records (structured content), a 64 KiB hexadecimal
  string, a 64 KiB alphanumeric run, a 1 MiB base64 image block and 5 MiB of text.
- The check, independent of the machine: through the SDK's capture work hook, the JSON the capture path serialized
  for each captured field before any cut, and the text it handed to the scrubber, stay at or below `work_bound`
  (the cap plus 8 KiB). A generous wall-clock ceiling backs it up: the slowest captured call may exceed the median
  uncaptured call by at most `ceiling_ms`.
- The latency gate: every run prints the added p99 per shape. `CRUCIBLE_CAPTURE_GATE=1` switches to the laptop's
  round counts and fails a shape whose added p99 is above `gate_added_p99_ms` (1 ms). CI never gates on timing.

| SDK | Test | Command, from the repository root |
|---|---|---|
| TypeScript | `sdk/typescript/test/capture-overhead.test.ts` | `pnpm --filter @elm-os/crucible exec vitest run test/capture-overhead.test.ts` |
| Python | `sdk/python/tests/test_capture_overhead.py` | `cd sdk/python && uv run --locked pytest -q -s tests/test_capture_overhead.py` |
| Go | `sdk/go/cruciblemcp/capture_overhead_test.go` | `go -C sdk/go test -count=1 -run TestCaptureOverhead -v ./cruciblemcp` |
| Rust | `sdk/rust/tests/capture_overhead.rs` | `cargo test --manifest-path sdk/rust/Cargo.toml --features mcp,test-support --test capture_overhead -- --nocapture`, with `--release` for the gate |

The sdk-conformance workflow runs each SDK's test in that SDK's matrix entry.

## Capability manifest

Each SDK commits one manifest (schema in `conformance/capability-manifest.schema.json`):

```json
{
  "schema": "crucible.conformance.manifest/v1",
  "sdk": "typescript",
  "language": "typescript",
  "sdk_version": "0.1.0",
  "runner": ["node", "sdk/typescript/conformance/runner.mjs"],
  "kinds": ["agent", "mcp"],
  "variants": ["legacy-stdio", "legacy-http", "modern-stdio", "modern-http"],
  "throw_outcome": "tool_error",
  "progress_count": "observed",
  "skips": []
}
```

| Field | Values | Effect |
|---|---|---|
| `kinds`, `variants` | Subsets of the suite's | Cases outside them are reported as skipped with the reason |
| `throw_outcome` | `tool_error` (the MCP library turns the exception into an `isError` result) or `internal` (a JSON-RPC error, -32603) | Picks the `throw` expectation |
| `progress_count` | `observed` or `unobserved` | `observed`: `crucible.mcp.progress.count` must equal the notifications sent; `unobserved`: it must be absent (contract 9.1: absent means unobserved) |
| `skips` | `{"case": "<id or scenario>", "reason": "..."}` | Explicit skips; each is printed as skipped, never as passed |

## Report

`<out>/<sdk>.jsonl` holds one line per case:

```json
{"sdk":"selftest","case":"mcp-echo@modern-http","scenario":"mcp-echo","variant":"modern-http","status":"pass","checks":61,"failed":0,"failures":[],"skip_reason":null,"runner":{"exit_code":0,"millis":180}}
```

`status` is `pass`, `fail`, `skip` or `error`; each failure names the expectation, the check, the expected and actual
values and the contract section. `<out>/<sdk>.md` renders the matrix (scenarios by variants) and lists every failure.

## Self-test and CI

`crucible-sdk-driver selftest-runner` reads a run spec and emits, with `opentelemetry-proto`, the spans an SDK that
follows this contract would emit, without any MCP server. It models the SDK from the run spec alone and never reads
the goldens, so a passing self-test means the collector, the driver and the goldens agree with an independent
encoding of the contract. Its defaults match `conformance/sdks/selftest.json`; `--throw-outcome tool_error` and
`--progress-count unobserved` model the other capability choices. Built with the `test-mutants` feature,
`--mutant unscrubbed-secret` makes it skip scrubbing; the driver must fail exactly the `mcp-secret` cases. `.github/workflows/sdk-conformance.yml` runs both on
every pull request that touches `conformance/**`, `sdk/**` or the crate. An SDK joins CI by committing its manifest
and adding an entry to the workflow's matrix.

## Readings of the contract

- 9.1 names no wire key for a transport failure or for the `mark_validation` and `mark_not_found` annotations; the
  driver reads annotations only through `crucible.mcp.error.class`, and no case covers a transport failure.
- 9.4 says senders should send `mcp.session.id` hashed; the suite requires the hashed form.
- 3.6 has no span keys for the tool inventory, so `tools/list` cases check the operation only.
- 9.1 and 9.2 do not spell out how span keys feed the classifier; the mapping under "What the driver checks in every
  case" is the driver's reading.
- 9.1 says "senders must truncate IP addresses themselves"; the suite also forbids `network.peer.address`, which
  would carry the raw client address on a server span.
- The suite configures no `identify` hook, so identity keys must be absent and `identity_source` must derive to
  `none`.
- Nothing has yet played these transcripts against a real MCP server; the first SDK runner is that check.
