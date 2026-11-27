# Crucible Rust SDK

Crate `crucible-sdk` sends traces from Rust agents and MCP servers to a self-hosted Crucible server over
OTLP/HTTP. It has one module that is always built and two optional features:

| Part | What it does |
|---|---|
| `crucible_sdk` | The process-wide exporter (`init`, `flush`, `shutdown`, `stats`), the session, user, tags and metadata of a trace carried in an OpenTelemetry `Context`, `observe` for spans with input, output, model, usage and cost, and `feedback` for scores |
| `crucible_sdk::mcp` (feature `mcp`) | `CrucibleService`, a wrapper for MCP servers built on rmcp 3.11 that records one span per operation in both protocol eras, over stdio and streamable HTTP |
| `crucible_sdk::mcp_client` (feature `mcp-client`) | `TracedClient`, which records the calls of an rmcp client and carries their trace context to the server |

## Install

Depend on it by git, pinned to the tag of a Crucible release:

```toml
[dependencies]
crucible-sdk = { git = "https://github.com/elm-os/crucible", tag = "v0.1.3" }
```

Add `features = ["mcp"]` to instrument an MCP server, or `features = ["mcp-client"]` for an MCP client. The crate
needs Rust 1.95 or newer. It uses OpenTelemetry 0.33 and exports over hyper with rustls and the ring provider: it
never links OpenSSL.

Each release also carries the crate's source archive, `crucible-sdk-0.1.3.crate`: the gzipped tar that
`cargo package` writes. Cargo cannot depend on a `.crate` file by URL, so to build from it, unpack it and depend on
the folder by path:

```sh
curl -LO https://github.com/elm-os/crucible/releases/download/v0.1.3/crucible-sdk-0.1.3.crate
mkdir crucible-sdk
tar -xzf crucible-sdk-*.crate -C crucible-sdk --strip-components 1
```

```toml
[dependencies]
crucible-sdk = { path = "crucible-sdk" }
```

A checkout of the repository works the same way, with the path of its `sdk/rust` folder.

## Hello example

A complete program that sends an agent span with a child generation, then flushes, prints the export counters
and shuts down. `Cargo.toml`:

```toml
[package]
name = "hello-crucible"
version = "0.1.0"
edition = "2024"

[dependencies]
crucible-sdk = { git = "https://github.com/elm-os/crucible", tag = "v0.1.3" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

`src/main.rs`, with `endpoint` set to your server:

```rust
use crucible_sdk::{Context, ObservationType, Options, Usage, observe};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    crucible_sdk::init(Options {
        endpoint: "http://localhost:4318".into(), // your Crucible server's OTLP/HTTP address
        secret_key: std::env::var("CRUCIBLE_SECRET_KEY")?, // a project secret key (csk_...)
        service_name: "hello-crucible".into(),
        ..Options::default()
    })?;

    let run = observe(&Context::new(), "answer", ObservationType::Agent);
    let generation = observe(&run.context(), "draft reply", ObservationType::Generation);
    generation.set_model("model-name");
    generation.set_input("What is Crucible?");
    generation.set_output("A self-hosted observability server for AI agents.");
    generation.set_usage(Usage { input: 12, output: 9, ..Usage::default() });
    generation.end();
    run.end();

    crucible_sdk::flush().await?;
    println!("{:?}", crucible_sdk::stats());
    crucible_sdk::shutdown().await?;
    Ok(())
}
```

Run it with a project secret key that has the `ingest` scope:

```sh
export CRUCIBLE_SECRET_KEY=csk_...
cargo run
```

It prints the counters of `crucible_sdk::stats()`, here two spans exported:

```text
Stats { exported: 2, dropped: 0, failed: 0, queued_bytes: 0, feedback_sent: 0, feedback_dropped: 0, feedback_failed: 0 }
```

Crucible then shows the trace `answer`, with the generation `draft reply` under it.

## Send traces from an agent

The fragments from here on show more of the API; the [hello example](#hello-example) is a whole program.

```rust
use crucible_sdk::{Context, ObservationType, Options, Usage, observe, with_session, with_user};

crucible_sdk::init(Options {
    endpoint: "http://localhost:4318".into(), // your Crucible server; plain http works
    secret_key: secret_key.into(),            // a project secret key (csk_...) with the ingest scope
    service_name: "support-agent".into(),
    ..Options::default()
})?; // the error says which option to change

let cx = with_session(&Context::new(), session_id);
let cx = with_user(&cx, user_id);

let run = observe(&cx, "answer", ObservationType::Agent);
let generation = observe(&run.context(), "draft reply", ObservationType::Generation);
generation.set_model("model-name");
generation.set_input(&messages);
generation.set_output(&reply);
generation.set_usage(Usage { input: 1200, output: 300, cache_read: 1000, ..Usage::default() });
generation.end();
run.end();

crucible_sdk::shutdown().await?;
```

`ObservationType` has ten variants, sent as `crucible.observation.type`: `Span`, `Generation`, `Event`, `Embedding`,
`Agent`, `Tool`, `Chain`, `Retriever`, `Evaluator` and `Guardrail`.

Every span started under a context carries the session, user, tags and metadata set on it with `with_session`,
`with_user`, `with_tags` and `with_metadata` (`crucible.session.id`, `crucible.user.id`, `crucible.tags`,
`crucible.metadata.<key>`). `set_input` and `set_output` send a string as it is and any other serializable value
as JSON (`crucible.input`, `crucible.output`). Usage goes out as the GenAI semantic-convention keys
(`gen_ai.usage.input_tokens` and the rest), and Crucible prices it from its price table. When you already know the
cost, `set_cost(Cost { total: 0.0042, ..Cost::default() })` sends it (`crucible.cost.total` and
`crucible.cost.<usage key>`, in USD) and it replaces the calculated cost. `set_attribute` sets any other key, for
example `crucible.trace.name` or `gen_ai.provider.name`.

`Observation::context` returns the context that carries the span: start child observations with it, or run async
work under it with OpenTelemetry's `FutureExt::with_context`. The SDK uses its own tracer provider and never
installs it globally, so an application's own OpenTelemetry setup is left alone; spans of other tracers that run
under an observation's context nest under it.

| Option | Meaning |
|---|---|
| `endpoint` | Base URL of the Crucible server. Spans go to `endpoint + "/v1/traces"` as OTLP protobuf |
| `secret_key` | Project secret key, sent as `Authorization: Bearer <key>`. Spans need the `ingest` scope and feedback the `write` scope; new keys have both |
| `project_id` | The key's project id (a UUID), which `feedback` needs |
| `api_url` | Base URL of the API listener, where `feedback` posts; when empty, `endpoint` |
| `service_name`, `service_version` | Resource `service.name` and `service.version` |
| `environment` | Resource `deployment.environment.name` |
| `release` | `crucible.release` on every span; when empty Crucible uses the service version |
| `deploy_id` | Resource `crucible.deploy.id`; when empty, Crucible uses the MCP server version (contracts 9.7) |
| `queue_size` | Finished spans waiting for export; default 2,048 |
| `queue_bytes` | Bytes of spans waiting for export or being exported, estimated from their strings; default 8 MiB |
| `flush_interval` | How often the queue is exported; default one second |
| `export_timeout` | Bound on one export, and on each `flush` and `shutdown`; default ten seconds |

Each process also gets a random `service.instance.id`. Before `init`, `observe` and the MCP wrappers record nothing
and cost almost nothing. `OTEL_*` environment variables cannot redirect the export, replace its key, relabel its
resource, lower its span limits or turn its sampling off.

## Export never blocks the caller

Finished spans go on a bounded queue that one background thread exports in batches. The thread runs an async
runtime of its own, so `init` works inside or outside an application's runtime and exporting never shares a thread
with the application. When the queue holds `queue_size` spans, or `queue_bytes` bytes of spans with the batch being
exported, a span is dropped and counted; ending a span never waits for the network, whether the server is up,
refusing connections or not answering. `crucible_sdk::stats()` returns the counters (`exported`, `dropped`,
`failed`, `queued_bytes`, and `feedback_sent`, `feedback_dropped`, `feedback_failed` for scores). `flush` exports what
has ended, sends the queued scores and returns the export error if there is one, else why the last score that was not
sent failed, and `shutdown` exports the rest and stops the threads; each waits at most `export_timeout` in all, for
the spans and the scores together. The first failed export after `init` emits one `tracing` warning (target
`crucible_sdk::export`), which shows where the application installs a tracing subscriber; later failures only count
in `failed`.

## Send feedback

```rust
use crucible_sdk::{Context, Feedback, ObservationType, Options, observe};
use opentelemetry::trace::TraceContextExt as _;

crucible_sdk::init(Options {
    endpoint: "http://crucible.internal:8080".into(), // the API listener, which also takes spans
    secret_key: secret_key.into(),                    // new keys have the ingest and write scopes
    project_id: project_id.into(),                    // the key's project
    service_name: "support-agent".into(),
    ..Options::default()
})?;

let answer = observe(&Context::new(), "answer", ObservationType::Agent);
let trace_id = answer.context().span().span_context().trace_id().to_string();
answer.end();

crucible_sdk::feedback(Feedback {
    trace_id,
    name: "helpful".into(),
    value: 1.into(),
    comment: "solved it".into(),
    ..Feedback::default()
});
```

`feedback` records a score with source `feedback` on a trace (`trace_id`), a span (`trace_id` and `span_id`) or a
session (`session_id`), and returns at once; it never blocks or panics. It posts to
`api_url + "/api/v1/projects/" + project_id + "/feedback"` with the secret key, which needs the `write` scope.

- The other fields are the route's: `score_id`, `data_type` (`numeric`, `boolean`, `categorical` or `text`),
  `comment` (4 KiB), `metadata` (a JSON object, 16 KiB) and `retracted`; empty ones are left out. `value` is a
  `ScoreValue`: a number, a bool or a string, each with a `From` conversion. The latest score of a name (and
  `score_id`) on a target wins.
- A worker thread posts the scores in call order, each after the spans that ended before the call, so a score on a
  trace that just ended finds the trace.
- Each score carries an `Idempotency-Key`, `idempotency_key` or else a random one. A failed connection, a timeout,
  408, 429, a 5xx answer and a `conflict` 409 are retried with the same key and body, three attempts in all, after
  `Retry-After` (at most 10 s) or else 1 s and 2 s, so Crucible writes the score once.
- `api_url` defaults to `endpoint`. The API listener (port 8080) serves both `/v1/traces` and the API; the OTLP
  listener (port 4318) does not serve the API, so with `endpoint` on port 4318, set `api_url`.
- The project id is the key's project: `crucible admin create-key` prints it as `project_id`, and the UI shows it as
  `project=` in the address bar. Without it, scores are dropped.
- Up to 1,024 scores wait to be sent; beyond that a score is dropped. `stats` counts sent, dropped and failed
  scores, and `flush` returns why the last score that was not sent was dropped or failed.
- Crucible answers 404 for a target it does not know yet, which counts as failed: a trace none of whose spans has
  ended and been sent (a score sent from inside the trace's only open span, for example), and a session, which
  Crucible stores about ten seconds after its spans. Score a trace once one of its spans has ended, and a session
  once its spans are stored.

## Instrument an MCP server

`Instrumentation` holds the options, built once; `wrap` puts a `CrucibleService` around a handler. The wrapper
implements rmcp's `ServerHandler` by forwarding every method, so rmcp serves it like any handler.

```rust
use crucible_sdk::mcp::{Instrumentation, Options};
use rmcp::ServiceExt as _;

let instrumentation = Instrumentation::new(Options {
    handle_params: [("lookup_order".into(), "order_id".into())].into(),
    ..Options::default()
})?;

// stdio:
let running = instrumentation.wrap(Orders::new()).serve(rmcp::transport::stdio()).await?;

// or streamable HTTP, which builds one handler per session or stateless request:
let service = StreamableHttpService::new(
    move || Ok(instrumentation.wrap(Orders::new())),
    LocalSessionManager::default().into(),
    StreamableHttpServerConfig::default(),
);
```

Every request and notification the server handles becomes a SERVER span named after its method and tool or
prompt, carrying the MCP wire keys of [contracts](../../docs/architecture/contracts.md) 9.1: method, tool or
prompt name, JSON-RPC id, protocol version and era, transport, client name and version, server name and version
(from `Options`, else the `server_info` the handler declares), the outcome and its error class, result type,
content block counts and, in the legacy era over HTTP, the session id hashed as `mcp.session.id`. Over HTTP the span
also carries the `User-Agent` and the request size from `Content-Length`. A `traceparent` in `params._meta` makes
the span a child of the caller's span; baggage is never read. The handler runs under the span's context, so its
own spans nest under the operation.

### Errors and progress

The error class follows the classifier of contracts 9.2: JSON-RPC errors are classified by code, results with
`isError: true` are tool errors, and a cancelled request is `cancelled`. From inside a tool handler, with its
`RequestContext`:

```rust
crucible_sdk::mcp::annotate_error(&context, Subclass::Upstream); // auth, rate_limit, upstream or business
crucible_sdk::mcp::mark_validation(&context);                    // the arguments were invalid
crucible_sdk::mcp::mark_not_found(&context);                     // the thing the request named does not exist
crucible_sdk::mcp::notify_progress(&context, params).await?;     // sends progress and counts it
```

A cancellation by `notifications/cancelled` is recorded as `notification`; in the modern era over HTTP, a closed
response stream is recorded as `stream_close`. Progress sent with `notify_progress` is counted in
`crucible.mcp.progress.count`; an operation whose handler never calls it carries no count, which Crucible reads as
unobserved.

### Identity

`Options::identify` is called for every request and notification with the context's extensions (on streamable HTTP
they hold the request's `http::request::Parts`) and returns what the server knows about the caller: user id, token
issuer and subject, tenant and handle id. Crucible hashes the subject with a per-project salt and keeps
only the hash and the issuer (contracts 9.3). `Options::no_identity` suppresses every identity key.

A handle id groups the operations of one logical session. It comes from `identify`, or from
`Options::handle_params`, which names a tool argument whose string value is used, scrubbed and cut at 256 bytes.
Nothing is added to a tool's schema or arguments.

### Multi-round requests

In the modern era a tool can answer `input_required` with a `requestState`, and the client retries with it. The span
of each round carries keyed hashes of that state (`crucible.mcp.mrtr.state_out` on the answer,
`crucible.mcp.mrtr.state_in` on the retry), which Crucible stores with each operation; the state itself is never sent.

### Content capture and scrubbing

Arguments and results are not captured unless a tool opts in:

```rust
Options {
    capture: vec![CaptureRule {
        tool: "search_*".into(),
        args: true,
        result: true,
        sample_rate: 0.1,
        ..CaptureRule::default()
    }],
    scrub: Some(Arc::new(|_field, text| Some(redact_account_numbers(text)))),
    ..Options::default()
}
```

The first rule whose glob (`*` and `?`) matches the tool applies. The sampling decision is made before anything is
copied, and only the part of a value the cap reads is serialized. The shared scrubbing rules then replace PEM
blocks, JWTs, bearer tokens, Crucible secret keys (`csk_`), card numbers that pass the Luhn check, email addresses,
IP addresses and phone numbers, and each field is cut at `max_bytes` (default 16 KiB) on a character boundary. The
rules are data: `src/scrub-rules.json` is a copy of `conformance/scrub-rules.json`, which every Crucible SDK uses,
and a test keeps the two identical. The SDK scrubs 1 KiB past the cap before cutting, so a secret that straddles
the cap leaves no prefix behind. `Options::scrub` runs after the rules on every captured field, error message and
handle id from `handle_params`, over the same window, and is the place for anything the shared rules do not cover;
returning `None` sends nothing. A result is captured as its content and structured content only, never the request
state or input requests of a multi-round exchange.

Authorization headers, cookies, `requestState`, raw session ids, client IP addresses and baggage are never sent.

## Instrument an MCP client

```rust
use crucible_sdk::mcp_client::TracedClient;
use opentelemetry::context::FutureExt as _;

let client = TracedClient::new(running.peer().clone());
let step = observe(&cx, "look up the order", ObservationType::Tool);
let response = client.call_tool(params).with_context(step.context()).await?;
step.end();
```

Every request sent through `send_request` or `call_tool` becomes a CLIENT span, a child of the current context's
span, and its trace context (`traceparent`, `tracestate`) is written into the request's `params._meta`. An
instrumented server then records its span in the same trace, so an agent's tool call and the server's handling of it
link up.

## What the wrapper cannot observe

`CrucibleService` sees what rmcp hands a `ServerHandler`, which leaves some fields of contracts 9.1 out of reach.

| Not observed | Why |
|---|---|
| Requests rmcp answers before the handler | An unsupported protocol version, missing `_meta` fields, a capability the client did not declare, a malformed message and a ping before `initialize` are answered inside rmcp; `crucible proxy` sees them |
| HTTP status, response size, client IP prefix, capabilities hash | rmcp does not expose them to handlers, and the contract fixes no algorithm for the capabilities hash |
| Timeouts | rmcp has no server-side deadline, so `crucible.mcp.timed_out` is never set |
| A stdio end of input with an operation pending | rmcp cancels the handler the same way as for `notifications/cancelled`, so the operation is recorded as cancelled rather than as a transport error |
| Progress sent through the peer directly | Only `notify_progress` counts |
| The resource template of `resources/read` | Only the concrete URI is visible, and a raw URI is not used as a target name |
| Server-to-client requests, batches | Requests the server sends (elicitation, sampling, roots) are not recorded as operations; batches were removed from the protocol |
| What rmcp changes after the handler returns | rmcp turns an `input_required` result for a peer below 2026-07-28 into an error, answers modern peers' -32002 as -32602, and turns a task result for a client that did not declare tasks into an error; the span records what the handler returned |

The span of an `initialize` request carries the version the handshake settles on, computed by rmcp's negotiation
rule from the requested version, the handler's offer and `supported_protocol_versions`.

## Supported versions

The `mcp` and `mcp-client` features support the rmcp 3.11 line. rmcp gives every `ServerHandler` method a default,
so a method added in a later minor release would answer with its default instead of reaching the wrapped handler
until this crate forwards it; the dependency is pinned to `~3.11.0` for that reason.

## Develop

The crate is its own Cargo workspace (with the conformance runner as its second member) and has its own
`Cargo.lock`. On a host without a Rust toolchain the checks run in the repository's dev container. Build its image
once, then run the checks, all from the repository root:

```sh
docker build -t crucible-dev:1 -f deploy/dev/Dockerfile deploy/dev
docker run --rm -v "$PWD":/work -w /work/sdk/rust crucible-dev:1 cargo test --all-features --locked
docker run --rm -v "$PWD":/work -w /work/sdk/rust crucible-dev:1 cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
docker run --rm -v "$PWD":/work -w /work/sdk/rust crucible-dev:1 cargo fmt --all --check
```

The tests run rmcp's in-memory transport in both eras against a fake OTLP collector (a hyper server) that decodes
every export and keeps the raw bodies, so they can check that no planted secret is exported.
`.github/workflows/ci-sdk-rust.yml` runs the same checks, clippy for each feature set, and a check that no OpenSSL,
native-tls or aws-lc crate enters the dependency tree.

## Conformance

The SDK passes the shared SDK conformance suite (`docs/testing/sdk-conformance.md`) in both eras over stdio and
streamable HTTP, with no skipped case. `conformance/runner` is its runner: for an agent case it is the agent program,
and for an MCP case it serves the reference server on rmcp, instrumented through this SDK's public API. The reference
server's `throw` tool returns `Err(ErrorData::internal_error(..))`, the error path of a hand-written `call_tool`,
which rmcp answers with a JSON-RPC error (-32603), so the manifest declares `throw_outcome: internal`; it reports
progress with `notify_progress`, so the manifest declares `progress_count: observed`. The manifest is
`conformance/manifest.json`, and the `rust` entry of `.github/workflows/sdk-conformance.yml` runs it. To run it
locally, build the driver (`cargo build -p crucible-fake-collector`) and, from the repository root:

```sh
target/debug/crucible-sdk-driver run --manifest sdk/rust/conformance/manifest.json
```
