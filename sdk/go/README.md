# Crucible Go SDK

Module `github.com/elm-os/crucible/sdk/go` sends traces from Go agents and MCP servers to a self-hosted Crucible
server over OTLP/HTTP. It has two packages:

| Package | What it does |
|---|---|
| `crucible` | The process-wide exporter (`Init`, `Flush`, `Shutdown`), the session, user, tags and metadata of a trace carried in a `context.Context`, `Observe` for spans with input, output, model, usage and cost, and `SendFeedback` for scores |
| `cruciblemcp` | Middleware for the official MCP Go SDK (`github.com/modelcontextprotocol/go-sdk`, tested at v1.8.0) in both protocol eras: one span per MCP operation on the server, and trace context propagation from the client |

It needs Go 1.26 or later. Add it with `go get github.com/elm-os/crucible/sdk/go@v0.1.2`.

## Send traces from an agent

```go
import "github.com/elm-os/crucible/sdk/go/crucible"

err := crucible.Init(crucible.Options{
	Endpoint:    "http://localhost:4318", // your Crucible server; plain http works
	SecretKey:   secretKey,               // a project secret key (csk_...) with the ingest scope
	ServiceName: "support-agent",
})
if err != nil {
	log.Fatal(err) // the error says which option to change
}
defer crucible.Shutdown(context.Background())

ctx = crucible.WithSession(ctx, sessionID)
ctx = crucible.WithUser(ctx, userID)
ctx = crucible.WithTags(ctx, "checkout")
ctx = crucible.WithMetadata(ctx, "plan", "pro")

ctx, run := crucible.Observe(ctx, "answer", crucible.Agent)
defer run.End()

_, gen := crucible.Observe(ctx, "draft reply", crucible.Generation)
gen.SetModel("model-name")
gen.SetInput(messages)
gen.SetOutput(reply)
gen.SetUsage(crucible.Usage{Input: 1200, Output: 300, CacheRead: 1000})
gen.End()
```

Every span started under a context carries the session, user, tags and metadata set on it (`crucible.session.id`,
`crucible.user.id`, `crucible.tags`, `crucible.metadata.<key>`). `SetInput` and `SetOutput` send a string as is and any other value as JSON
(`crucible.input`, `crucible.output`). Usage goes out as the GenAI semantic-convention keys
(`gen_ai.usage.input_tokens` and the rest), and Crucible prices it from its price table. When you already know the
cost, `SetCost(crucible.Cost{Total: 0.0042})` sends it (`crucible.cost.total`, `crucible.cost.<usage key>`, in USD)
and it replaces the calculated cost. `Span()` returns the OpenTelemetry span for any other attribute.

| Option | Meaning |
|---|---|
| `Endpoint` | Base URL of the Crucible server. Spans go to `Endpoint + "/v1/traces"` as OTLP protobuf |
| `SecretKey` | Project secret key, sent as `Authorization: Bearer <key>`. Spans need the `ingest` scope and feedback the `write` scope; new keys have both |
| `ProjectID` | The key's project id (a UUID), which `SendFeedback` needs |
| `APIURL` | Base URL of the API listener, where `SendFeedback` posts; when empty, `Endpoint` |
| `ServiceName`, `ServiceVersion` | Resource `service.name` and `service.version`. Crucible uses them as an MCP server's name and version when `cruciblemcp.Options` gives none |
| `Environment` | Resource `deployment.environment.name` |
| `Release` | `crucible.release` on every span; when empty Crucible uses the service version |
| `DeployID` | Resource `crucible.deploy.id`; when it is empty, Crucible uses the server version (contracts 9.7) |
| `QueueSize` | Finished spans waiting for export; default 2048 |
| `QueueBytes` | Bytes of spans waiting for export or being exported, estimated from their strings; default 8 MiB |
| `FlushInterval` | How often the queue is exported; default one second |

Each process also gets a random `service.instance.id`. Before `Init`, `Observe` and the MCP middleware record
nothing and cost almost nothing.

## Export never blocks the caller

Finished spans go on a bounded queue that one goroutine exports in batches. When the queue is full a span is dropped
and counted; ending a span never waits for the network. `crucible.CurrentStats()` returns the counters (`Exported`,
`QueueFull`, `ExportFailed`, `QueuedBytes`, and `FeedbackSent`, `FeedbackDropped`, `FeedbackFailed` for scores),
`Flush` exports what has ended, sends the queued scores and returns the export error if there is one, else why the
last score that was not sent failed, and `Shutdown` gives up on spans and scores still queued when its context ends.

## Send feedback

```go
err := crucible.Init(crucible.Options{
	Endpoint:    "http://crucible.internal:8080", // the API listener, which also takes spans
	SecretKey:   secretKey,                       // new keys have the ingest and write scopes
	ProjectID:   projectID,                       // the key's project
	ServiceName: "support-agent",
})

_, answer := crucible.Observe(ctx, "answer", crucible.Agent)
traceID := answer.Span().SpanContext().TraceID().String()
answer.End()

crucible.SendFeedback(crucible.Feedback{TraceID: traceID, Name: "helpful", Value: 1, Comment: "solved it"})
crucible.SendFeedback(crucible.Feedback{TraceID: traceID, Name: "tone", Value: "friendly", IdempotencyKey: "tone-" + traceID})
```

`SendFeedback` records a score with source `feedback` on a trace (`TraceID`), a span (`TraceID` and `SpanID`) or a
session (`SessionID`), and returns at once. It posts to `APIURL + "/api/v1/projects/" + ProjectID + "/feedback"`
with the secret key, which needs the `write` scope.

- The other fields are the route's: `ScoreID`, `DataType` (`numeric`, `boolean`, `categorical` or `text`), `Comment`
  (4 KiB), `Metadata` (a JSON object, 16 KiB) and `Retracted`; empty ones are left out. The latest score of a name
  (and `ScoreID`) on a target wins.
- One goroutine posts the scores in call order, each after the spans that ended before the call, so a score on a
  trace that just ended finds the trace.
- Each score carries an `Idempotency-Key`, `IdempotencyKey` or else a random one. A failed connection, a timeout,
  408, 429, a 5xx answer and a `conflict` 409 are retried with the same key and body, three attempts in all, after
  `Retry-After` (at most 10 s) or else 1 s and 2 s, so Crucible writes the score once.
- `APIURL` defaults to `Endpoint`. The API listener (port 8080) serves both `/v1/traces` and the API; the OTLP
  listener (port 4318) does not serve the API, so with `Endpoint` on port 4318, set `APIURL`.
- The project id is the key's project: `crucible admin create-key` prints it as `project_id`, and the UI shows it as
  `project=` in the address bar. Without it, scores are dropped.
- Up to 1,024 scores wait to be sent; beyond that a score is dropped. `CurrentStats` counts sent, dropped and failed
  scores, and `Flush` returns why the last score that was not sent was dropped or failed.
- Crucible answers 404 for a target it does not know yet, which counts as failed: a trace none of whose spans has
  ended and been sent (a score sent from inside the trace's only open span, for example), and a session, which
  Crucible stores about ten seconds after its spans. Score a trace once one of its spans has ended, and a session
  once its spans are stored.

## Instrument an MCP server

```go
import "github.com/elm-os/crucible/sdk/go/cruciblemcp"

server := mcp.NewServer(&mcp.Implementation{Name: "orders", Version: "2.1.0"}, nil)
mcp.AddTool(server, &mcp.Tool{Name: "lookup_order"}, lookupOrder)

in, err := cruciblemcp.Instrument(server, cruciblemcp.Options{
	ServerName:    "orders",
	ServerVersion: "2.1.0",
	HandleParams:  map[string]string{"lookup_order": "order_id"},
})

// stdio:
err = server.Run(ctx, in.Transport(&mcp.StdioTransport{}))
// or streamable HTTP:
http.Handle("/mcp", in.Handler(mcp.NewStreamableHTTPHandler(getServer, nil)))
```

`Instrument` adds receiving and sending middleware. Add it after your own middleware so that it wraps them and sees
the whole operation. Every request and notification the server receives becomes a SERVER span named after its method
and tool or prompt, carrying the MCP wire keys of [contracts](../../docs/architecture/contracts.md) 9.1: method,
tool or prompt name, protocol version and era, transport, client name and version, the outcome and its error class,
result type, progress count, content block counts and, in the legacy era, the session id hashed as
`mcp.session.id`. A `traceparent` in `params._meta` makes the span a child of the caller's span.

The MCP Go SDK does not pass middleware the JSON-RPC id of a request, so `Instrument` returns an `Instrumentation`
whose `Transport` and `Handler` wrap what the server is connected over. With either, each request's span carries its
`jsonrpc.request.id`; without them everything else is recorded the same. `Handler` reads each POST body, up to 4 MiB,
to find the id and passes it on unchanged; wrap the MCP handler directly, because a handler in between must not
replace the request's header map. A connection from `Transport` does not reject JSON-RPC batches in protocol versions
that removed them, because the MCP SDK tells only its own connection type the negotiated version.

Streamable HTTP is detected; requests that arrive any other way are reported as `Options.Transport`, which defaults
to `stdio`. Over HTTP the span also carries the `User-Agent` and the request size from `Content-Length`.

### Errors

The error class follows the classifier of contracts 9.2. JSON-RPC errors are classified by code, results with
`isError: true` are tool errors, and a cancelled or timed-out request is `cancelled` or `timeout`. Three calls refine
the class from inside a tool handler, using the handler's context:

```go
cruciblemcp.AnnotateError(ctx, cruciblemcp.SubclassUpstream) // auth, rate_limit, upstream or business
cruciblemcp.MarkValidation(ctx)                               // the arguments were invalid
cruciblemcp.MarkNotFound(ctx)                                 // the thing the request named does not exist
```

A cancellation by `notifications/cancelled` is recorded as `notification`. A broken connection, and in the modern
era over HTTP a closed response stream, is recorded as `stream_close`.

### Identity

`Options.Identify` is called for every request and notification and returns what your server knows about the
caller: user id, token issuer and subject, tenant and handle id. When it gives no subject and the request came over streamable HTTP
with a token your `auth.TokenVerifier` accepted, the token's `UserID` is the subject and the `iss` entry of
`TokenInfo.Extra` its issuer. The token's `client_id` entry is sent as the OAuth client id. Crucible hashes
the subject with a per-project salt and keeps only the hash and the issuer (contracts 9.3).
`Options.NoIdentity` suppresses every identity key.

A handle id groups the operations of one logical session. It comes from `Identify`, or from `HandleParams`, which
names a tool argument whose string value is used, scrubbed and cut at 256 bytes. Nothing is added to a tool's schema
or arguments.

### Multi-round requests

In the modern era a tool can answer `input_required` with a `requestState`, and the client retries with it. The span
of each round carries keyed hashes of that state (`crucible.mcp.mrtr.state_out` on the answer,
`crucible.mcp.mrtr.state_in` on the retry), which Crucible stores with each operation; the state itself is never sent. In the
legacy era the MCP SDK completes the exchange inside one request, which is one span.

### Content capture and scrubbing

Arguments and results are not captured unless a tool opts in:

```go
cruciblemcp.Options{
	Capture: []cruciblemcp.CaptureRule{
		{Tool: "search_*", Args: true, Result: true, SampleRate: 0.1},
	},
	Scrub: func(field, value string) (string, bool) { return redactAccountNumbers(value), true },
}
```

The first rule whose glob matches the tool applies. The sampling decision is made before anything is copied, and each
field is cut at `MaxBytes` (default 16 KiB) on a character boundary. The shared scrubbing rules then replace PEM
blocks, JWTs, bearer tokens, Crucible secret keys (`csk_`), card numbers that pass the Luhn check, email addresses,
IP addresses and phone numbers. The rules are data: `internal/scrub/rules.json` is a copy of
`conformance/scrub-rules.json`, which every Crucible SDK uses, and a test keeps the two identical. The SDK scrubs 1 KiB
past the cap before cutting, so a secret that straddles the cap leaves no prefix behind. `Scrub` runs after the rules on
every captured field, error message and handle id from `HandleParams`, and is the place for anything the shared rules
do not cover, such as values of fields named like passwords; returning false sends nothing. Scrubbing and `Scrub` run
off the request path, on the SDK's export goroutine, one span at a time; a call only copies the part of each field
that scrubbing reads. `Scrub` must be safe for concurrent use.

Authorization headers, cookies, `requestState`, raw session ids, client IP addresses and baggage are never sent.

## Instrument an MCP client

```go
client := mcp.NewClient(&mcp.Implementation{Name: "agent", Version: "1.0.0"}, nil)
err := cruciblemcp.InstrumentClient(client, cruciblemcp.ClientOptions{Transport: cruciblemcp.TransportStdio})
```

Every request and notification the client sends becomes a CLIENT span, and its trace context (`traceparent`,
`tracestate`) is written into the request's `params._meta`. An instrumented server then records its span in the same
trace, so an agent's tool call and the server's handling of it link up. The `_meta` map is replaced by a copy rather
than edited in place, but the params value itself is updated, as the MCP SDK also does, so do not share one params
value between concurrent calls.

## What the middleware cannot observe

The MCP Go SDK hands middleware a method, its decoded params and a context, which leaves some fields of contracts
9.1 out of reach. These keys are not sent:

| Not observed | Why |
|---|---|
| `jsonrpc.request.id` without `Transport` or `Handler` | go-sdk v1.8.0 keeps the request id under an unexported context key |
| Errors raised before the middleware runs | An unknown method (-32601), a malformed request (-32600, -32700), params that fail to decode and invalid `_meta` are answered before any middleware; `crucible proxy` sees them |
| HTTP status and transport failures | They happen in the HTTP handler and the transport, outside the middleware |
| The resource template of `resources/read` | Only the concrete URI is visible, and a raw URI is not used as a target name |
| Client IP prefix, response size, client capabilities hash | The SDK does not expose the peer address or the encoded response, and the contract fixes no algorithm for the capabilities hash |
| Server-to-client requests, tasks, batches | Requests the server sends (elicitation, sampling, roots) are not recorded as operations; go-sdk v1.8.0 has no tasks, and batches were removed from the protocol |

## Develop

There is no need for a local Go toolchain; the checks run in the `golang` image from the repository root:

```sh
docker run --rm -v "$PWD:/work" -w /work/sdk/go golang:1.27.4 go test -race ./...
docker run --rm -v "$PWD:/work" -w /work/sdk/go golang:1.27.4 go vet ./...
docker run --rm -v "$PWD:/work" -w /work/sdk/go golang:1.27.4 go run honnef.co/go/tools/cmd/staticcheck@v0.8.2 ./...
```

The tests run an in-memory MCP transport in both eras and streamable HTTP against an `httptest` OTLP collector that
decodes every export and keeps the raw bodies, so they can check that no planted secret is exported.
`.github/workflows/ci-sdk-go.yml` runs the same checks.

## Conformance

The SDK passes the shared SDK conformance suite (`docs/testing/sdk-conformance.md`) in both eras over stdio and
streamable HTTP, with no skipped case. `conformance/runner` is its runner: for an agent case it is the agent program,
and for an MCP case it serves the reference server on go-sdk, instrumented through this SDK's public API. The manifest
is `conformance/manifest.json`, and the `go` entry of `.github/workflows/sdk-conformance.yml` runs it. To run it
locally, build the driver (`cargo build -p crucible-fake-collector`) and, from the repository root:

```sh
target/debug/crucible-sdk-driver run --manifest sdk/go/conformance/manifest.json
```
