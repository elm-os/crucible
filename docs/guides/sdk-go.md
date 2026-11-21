# Go SDK

Module `github.com/elm-os/crucible/sdk/go` sends traces from Go agents and MCP servers to Crucible over OTLP/HTTP.
It has two packages:

| Package | Contents |
|---|---|
| `crucible` | The process-wide exporter (`Init`, `Flush`, `Shutdown`, `CurrentStats`), the session, user, tags and metadata of a trace carried in a `context.Context`, and `Observe` for spans with input, output, model, usage and cost |
| `cruciblemcp` | Middleware for servers and clients of the official MCP Go SDK (`github.com/modelcontextprotocol/go-sdk`, tested at v1.8.0), in both protocol eras |

It needs Go 1.26 or newer.

## Install

Add the module at the version of your Crucible server:

```sh
go get github.com/elm-os/crucible/sdk/go@v0.1.1
```

To work on a checkout of the repository instead, point your module at it:

```sh
git clone https://github.com/elm-os/crucible.git
cd your-module
go mod edit -require=github.com/elm-os/crucible/sdk/go@v0.0.0 -replace=github.com/elm-os/crucible/sdk/go=../crucible/sdk/go
go mod tidy
```

The examples on this page are complete programs, one per directory of a module that requires the SDK. Each passed
`go vet` with Go 1.27.4 and was run, with only the endpoint changed, against a Crucible server whose API then
returned the traces they sent.

## Set up

```go
// hello/main.go
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
)

func main() {
	err := crucible.Init(crucible.Options{
		Endpoint:    "http://localhost:4318",
		SecretKey:   os.Getenv("CRUCIBLE_SECRET_KEY"),
		ServiceName: "hello-agent",
	})
	if err != nil {
		log.Fatal(err) // the error says which option to change
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	_, obs := crucible.Observe(ctx, "say hello", crucible.Span)
	fmt.Println("hello")
	obs.End()

	if err := crucible.Flush(ctx); err != nil {
		log.Fatal(err)
	}
	fmt.Printf("%+v\n", crucible.CurrentStats())
	if err := crucible.Shutdown(ctx); err != nil {
		log.Fatal(err)
	}
}
```

It prints `hello` and then
`{Exported:1 QueueFull:0 ExportFailed:0 QueuedBytes:0 FeedbackSent:0 FeedbackDropped:0 FeedbackFailed:0}`.

`Init` configures the process-wide client. Spans go to `Endpoint + "/v1/traces"` as OTLP/HTTP protobuf with
`Authorization: Bearer <SecretKey>`; plain `http` works.

| Option | Meaning |
|---|---|
| `Endpoint` | Required. Base URL of the Crucible server's OTLP listener, `http` or `https` |
| `SecretKey` | Required. The project's secret key (`csk_...`), sent as a Bearer token. Spans need the `ingest` scope and feedback the `write` scope; new keys have both |
| `ProjectID` | The key's project id (a UUID), which `SendFeedback` needs; see [Send feedback](#send-feedback) |
| `APIURL` | Base URL of the API listener, where `SendFeedback` posts; when empty, `Endpoint` |
| `ServiceName`, `ServiceVersion` | Resource `service.name` and `service.version`. Crucible uses them as an MCP server's name and version when `cruciblemcp.Options` gives none |
| `Environment` | Resource `deployment.environment.name` |
| `Release` | `crucible.release` on every span; when it is empty, Crucible uses the service version |
| `DeployID` | Resource `crucible.deploy.id`; without it, Crucible uses the server version as the deploy id |
| `QueueSize` | Finished spans waiting for export; 0 means 2,048 |
| `QueueBytes` | Bytes of spans waiting for export or being exported, estimated from their names, attributes, events and status messages; 0 means 8 MiB |
| `FlushInterval` | How often the queue is exported; 0 means one second |

The SDK reads no `CRUCIBLE_*` environment variables: pass every value in `Options`. `Init` returns an error that says
which option to change when `Endpoint` is empty or not an `http` or `https` URL, when `SecretKey` is empty or does not
start with `csk_` (a public key `cpk_...` cannot send telemetry), when `APIURL` is not an `http` or `https` URL, when
`ProjectID` is not a UUID, when a size or interval is negative, and when `Init` was already called; call `Shutdown`
before calling `Init` again.

The SDK keeps its own tracer provider and does not set the global one. Every span is kept: a remote parent that was
not sampled does not suppress an MCP operation. Each process gets a random `service.instance.id`. Before `Init`,
`Observe` and the MCP middleware record nothing and cost almost nothing.

- `Flush(ctx)` exports every span that has ended and sends every queued score. It returns the export error, else why
  the last score that was not sent since the previous `Flush` was dropped or failed, or the context's error when the
  context ends first. Before `Init` it does nothing.
- `Shutdown(ctx)` flushes, stops the client and lets `Init` be called again. When the context ends first, the spans
  and scores still queued are dropped. Give it a context with a deadline, as above, so that a collector that does not answer
  cannot hold the process.

## Trace an agent

```go
// agent/main.go
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"go.opentelemetry.io/otel/attribute"
)

// callModel stands in for your model client.
func callModel(_ context.Context, question string) string {
	return fmt.Sprintf("For %q: Lisbon", question)
}

func main() {
	err := crucible.Init(crucible.Options{
		Endpoint:    os.Getenv("CRUCIBLE_ENDPOINT"),
		SecretKey:   os.Getenv("CRUCIBLE_SECRET_KEY"),
		ServiceName: "trip-agent",
		Environment: "staging",
		Release:     "1.4.0",
	})
	if err != nil {
		log.Fatal(err)
	}
	defer shutdown()

	ctx := context.Background()
	ctx = crucible.WithSession(ctx, "conv-42")
	ctx = crucible.WithUser(ctx, "user-7")
	ctx = crucible.WithTags(ctx, "beta")
	ctx = crucible.WithMetadata(ctx, "plan", "pro")

	question := "Where should I go in May?"
	ctx, agent := crucible.Observe(ctx, "plan trip", crucible.Agent)
	agent.SetInput(question)
	agent.Span().SetAttributes(
		attribute.String("crucible.trace.name", "trip planner"),
		attribute.StringSlice("crucible.trace.tags", []string{"beta"}),
	)

	_, gen := crucible.Observe(ctx, "chat", crucible.Generation)
	reply := callModel(ctx, question)
	gen.SetModel("model-large")
	gen.Span().SetAttributes(attribute.String("gen_ai.response.model", "model-large-2026-05"))
	gen.SetInput(question)
	gen.SetOutput(reply)
	gen.SetUsage(crucible.Usage{Input: 1200, Output: 300, CacheRead: 1000})
	gen.SetCost(crucible.Cost{Total: 0.0042})
	gen.End()

	agent.SetOutput(reply)
	agent.End()
	fmt.Println(reply)
}

func shutdown() {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := crucible.Shutdown(ctx); err != nil {
		log.Print(err)
	}
}
```

- `WithSession`, `WithUser`, `WithTags` and `WithMetadata` return a context whose spans carry
  `crucible.session.id`, `crucible.user.id`, `crucible.tags` (added to the tags the context already carries) and
  `crucible.metadata.<key>`. A metadata value that is a string, bool, integer or float is sent as that type; any
  other value as JSON text.
- `Observe(ctx, name, type)` starts a span as a child of the span in `ctx`, sends the type as
  `crucible.observation.type` (`crucible.Span`, `Generation`, `Event`, `Embedding`, `Agent`, `Tool`, `Chain`,
  `Retriever`, `Evaluator` or `Guardrail`), and returns a context that carries it. `End` ends it; its span is
  exported in the background.
- `SetInput` and `SetOutput` send a string or `[]byte` as it is and any other value as JSON (`crucible.input`,
  `crucible.output`). `SetModel` sends `gen_ai.request.model`, which prices the usage. `SetUsage` sends the
  `gen_ai.usage.*` token counts; `CacheRead` and `CacheWrite` are parts of `Input`, and `Reasoning` of `Output`.
  `SetCost` sends a cost you already know (`crucible.cost.total` and `crucible.cost.<usage key>`, in USD), which
  replaces the cost Crucible would calculate. Zero fields are not sent. `SetError` marks the span as failed.
- `Span()` returns the OpenTelemetry span, for keys the helpers do not set, such as `crucible.trace.name`,
  `crucible.trace.tags`, `gen_ai.response.model` and `gen_ai.provider.name`.

[Sending traces](sending-traces.md#attributes-the-trace-views-read) lists which keys the trace views show on main.
Two notes for agents: the views show the model from `gen_ai.response.model`, so set it on the span as the example
does; and the trace list shows the trace tags, `crucible.trace.tags`, not the span tags of `WithTags`.

## Send feedback

`crucible.SendFeedback(f)` records a score with source `feedback` on a trace, a span or a session: the scores of
[facts](facts.md), sent by the SDK.

```go
// feedback/main.go
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
)

func main() {
	err := crucible.Init(crucible.Options{
		Endpoint:    "http://localhost:8080",          // the API listener, which also takes spans
		SecretKey:   os.Getenv("CRUCIBLE_SECRET_KEY"), // new keys have the ingest and write scopes
		ProjectID:   os.Getenv("CRUCIBLE_PROJECT_ID"), // the key's project
		ServiceName: "support-agent",
	})
	if err != nil {
		log.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	_, answer := crucible.Observe(ctx, "answer", crucible.Agent)
	answer.SetOutput("Your refund is on its way.")
	traceID := answer.Span().SpanContext().TraceID().String()
	answer.End()

	// Straight after the trace ends: its spans are sent first.
	crucible.SendFeedback(crucible.Feedback{
		TraceID: traceID, Name: "helpful", Value: 1, Comment: "solved it", IdempotencyKey: "thumbs-" + traceID,
	})
	crucible.SendFeedback(crucible.Feedback{TraceID: traceID, Name: "tone", Value: "friendly", ScoreID: "reviewer-2"})

	if err := crucible.Flush(ctx); err != nil {
		log.Fatal(err) // says why a score was not sent
	}
	s := crucible.CurrentStats()
	fmt.Println(s.Exported, s.FeedbackSent, s.FeedbackDropped, s.FeedbackFailed)
	if err := crucible.Shutdown(ctx); err != nil {
		log.Fatal(err)
	}
}
```

Run against a Crucible server, it prints `1 2 0 0`, and the trace list shows both scores on the trace.

Each score goes to `POST <APIURL>/api/v1/projects/<ProjectID>/feedback` as JSON with
`Authorization: Bearer <SecretKey>`; the key needs the `write` scope.

- **Where it goes.** `APIURL` defaults to `Endpoint`. The API listener (port 8080 by default) serves both
  `/v1/traces` and the API, so pointing `Endpoint` at it, as above, is enough. The OTLP listener (port 4318) serves
  only ingest and answers 404 for the feedback route: with `Endpoint` on port 4318, set `APIURL` to the API
  listener.
- **The project id** is the key's project: `crucible admin create-key` prints it as `project_id`, a keys file names
  it with each key, and the UI shows it as `project=` in the address bar. Without it, every score is dropped.
- **The score.** The target is `TraceID`, `TraceID` with `SpanID`, or `SessionID`. `Name` (without `#`) and `Value`
  (a number, a bool or a string) are required; `ScoreID`, `DataType` (`numeric`, `boolean`, `categorical` or
  `text`), `Comment` (4 KiB), `Metadata` (a JSON object, 16 KiB) and `Retracted` are optional, and empty ones are
  left out. The latest score of a name (and `ScoreID`) on a target wins; `Retracted: true` removes it.
- **Never in the way.** `SendFeedback` queues the score and returns at once. One goroutine posts the scores in call
  order, one at a time, each after the spans that ended before the call, so a score on a trace that just ended
  finds the trace.
- **Sent once.** Each score carries an `Idempotency-Key`: `IdempotencyKey` when you give one, else a random key. A
  failed connection, a timeout, 408, 429, a 5xx answer and a `conflict` 409 are retried with the same key and body,
  three attempts in all, after the answer's `Retry-After` (at most 10 s) or else after 1 s and 2 s, so Crucible
  writes the score once. Any other refusal is final.
- **Counted.** Up to 1,024 scores wait to be sent; beyond that a score is dropped. `CurrentStats()` counts
  `FeedbackSent`, `FeedbackDropped` (queue full, no project id, or `Shutdown` out of time) and `FeedbackFailed`
  (refused, or failed on every attempt), and `Flush` returns the reason, with the URL, Crucible's answer and the
  options to check.
- **Not found yet.** Crucible answers 404 for a target it does not know yet, which counts as a failure: a trace
  none of whose spans has ended and been sent (a score sent from inside the trace's only open span, for example), and
  a session, which Crucible stores about ten seconds after its spans. Score a trace once one of its spans has ended,
  and a session once its spans are stored.

## Instrument an MCP server

```go
// server/main.go
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type orderIn struct {
	OrderID string `json:"order_id"`
}

type refundIn struct {
	OrderID string `json:"order_id"`
	Card    string `json:"card"`
}

var orders = map[string]string{"A-1001": "shipped"}

func text(s string) []mcp.Content { return []mcp.Content{&mcp.TextContent{Text: s}} }

func lookupOrder(ctx context.Context, _ *mcp.CallToolRequest, in orderIn) (*mcp.CallToolResult, any, error) {
	status, ok := orders[in.OrderID]
	if !ok {
		cruciblemcp.MarkNotFound(ctx)
		return &mcp.CallToolResult{Content: text("no order " + in.OrderID), IsError: true}, nil, nil
	}
	return &mcp.CallToolResult{Content: text(fmt.Sprintf("order %s: %s", in.OrderID, status))}, nil, nil
}

func refund(ctx context.Context, _ *mcp.CallToolRequest, in refundIn) (*mcp.CallToolResult, any, error) {
	cruciblemcp.AnnotateError(ctx, cruciblemcp.SubclassUpstream)
	return &mcp.CallToolResult{Content: text("the payment service refused " + in.OrderID), IsError: true}, nil, nil
}

func main() {
	err := crucible.Init(crucible.Options{
		Endpoint:       os.Getenv("CRUCIBLE_ENDPOINT"),
		SecretKey:      os.Getenv("CRUCIBLE_SECRET_KEY"),
		ServiceName:    "orders-mcp",
		ServiceVersion: "2.1.0",
	})
	if err != nil {
		log.Fatal(err)
	}
	defer shutdown()

	server := mcp.NewServer(&mcp.Implementation{Name: "orders-mcp", Version: "2.1.0"}, nil)
	mcp.AddTool(server, &mcp.Tool{Name: "lookup_order", Description: "Looks an order up by id."}, lookupOrder)
	mcp.AddTool(server, &mcp.Tool{Name: "refund", Description: "Refunds an order to a card."}, refund)

	in, err := cruciblemcp.Instrument(server, cruciblemcp.Options{
		ServerName:    "orders-mcp",
		ServerVersion: "2.1.0",
		Capture:       []cruciblemcp.CaptureRule{{Tool: "refund", Args: true, Result: true, MaxBytes: 4096}},
		HandleParams:  map[string]string{"lookup_order": "order_id", "refund": "order_id"},
		Identify: func(context.Context, mcp.Request) cruciblemcp.Identity {
			return cruciblemcp.Identity{UserID: "user-7", Tenant: "acme"}
		},
	})
	if err != nil {
		log.Fatal(err)
	}
	if err := server.Run(context.Background(), in.Transport(&mcp.StdioTransport{})); err != nil {
		log.Print(err)
	}
}

func shutdown() {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := crucible.Shutdown(ctx); err != nil {
		log.Print(err)
	}
}
```

`Instrument(server, options)` adds receiving and sending middleware to the server. Add it after your own middleware
so that it wraps them and sees the whole operation. Every request and notification the server receives becomes a
SERVER span named after its method and tool or prompt, carrying the keys of
[contract 9.1](../architecture/contracts.md#91-wire-attribute-contract): method, tool or prompt name, protocol
version and era, transport, client name and version, the outcome and its error class, result type, progress count,
content block counts and, in the legacy era, the session id hashed as `mcp.session.id`. A `traceparent` in
`params._meta` makes the span a child of the caller's span.

On main, Crucible stores these spans as spans: the trace view shows a `tools/call` span as a `tool` span and a
tool error with status `error`, captured arguments and results become the span's input and output, and the other
MCP keys stay among its attributes (the payload route's `attributes` field). Crucible also records each such span
as an MCP operation on the server side, which the SQL workspace's `crucible_sql.mcp_operations` view reads.

The MCP Go SDK does not pass middleware the JSON-RPC id of a request, so `Instrument` returns an `Instrumentation`
whose `Transport` and `Handler` wrap what the server is connected over. With either, each request's span carries its
`jsonrpc.request.id`; without them everything else is recorded the same. Over streamable HTTP:

```go
// http/main.go
package main

import (
	"log"
	"net/http"
	"os"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

func main() {
	err := crucible.Init(crucible.Options{
		Endpoint:    os.Getenv("CRUCIBLE_ENDPOINT"),
		SecretKey:   os.Getenv("CRUCIBLE_SECRET_KEY"),
		ServiceName: "orders-mcp",
	})
	if err != nil {
		log.Fatal(err)
	}
	server := mcp.NewServer(&mcp.Implementation{Name: "orders-mcp", Version: "2.1.0"}, nil)
	in, err := cruciblemcp.Instrument(server, cruciblemcp.Options{})
	if err != nil {
		log.Fatal(err)
	}
	handler := mcp.NewStreamableHTTPHandler(func(*http.Request) *mcp.Server { return server }, nil)
	http.Handle("/mcp", in.Handler(handler))
	log.Fatal(http.ListenAndServe("127.0.0.1:8000", nil))
}
```

`Handler` reads each POST body, up to 4 MiB, to find the id and passes it on unchanged; wrap the MCP handler
directly, because a handler in between must not replace the request's header map. A connection from `Transport`
does not reject JSON-RPC batches in protocol versions that removed them, because the MCP SDK tells only its own
connection type the negotiated version. Streamable HTTP is detected; requests that arrive any other way are
reported as `Options.Transport`, which defaults to `stdio`. Over HTTP the span also carries the `User-Agent` and the
request size from `Content-Length`.

| Option | Meaning |
|---|---|
| `ServerName`, `ServerVersion` | The server's name and version. When they are empty, the spans carry none, and Crucible uses the service name and version given to `crucible.Init` |
| `Transport` | The transport reported for requests that did not arrive over streamable HTTP: `stdio` (the default), `sse` or `unknown` |
| `Capture` | Content capture rules; see below |
| `HandleParams` | Tool name to one of its arguments whose string value becomes the handle id, scrubbed and cut at 256 bytes. A handle id from `Identify` takes precedence |
| `Identify` | Called for every request and notification; returns what your server knows about the caller |
| `NoIdentity` | Sends no user identity keys: `Identify` is not called and tokens are not read |
| `Scrub` | Runs after the built-in rules on every captured argument and result, error message and handle id from `HandleParams`, off the request path; returns the value to send, or false to send nothing |

### Errors

The error class follows the classifier of [contract 9.2](../architecture/contracts.md#92-error-taxonomy). JSON-RPC
errors are classified by code, results with `isError: true` are tool errors, and a cancelled or timed-out request is
`cancelled` or `timeout`. Three calls refine the class from inside a tool handler, with the handler's context:

| Call | Effect |
|---|---|
| `cruciblemcp.AnnotateError(ctx, subclass)` | Refines a tool error with `SubclassAuth`, `SubclassRateLimit`, `SubclassUpstream` or `SubclassBusiness` |
| `cruciblemcp.MarkValidation(ctx)` | The tool rejected its arguments: an `isError` result counts as a validation error |
| `cruciblemcp.MarkNotFound(ctx)` | What the request named does not exist: an `isError` result, or a -32602 JSON-RPC error, counts as not found |

A cancellation by `notifications/cancelled` is recorded as `notification`. A broken connection, and in the modern
era over HTTP a closed response stream, is recorded as `stream_close`. Error messages are scrubbed and cut at 2 KiB.

### Identity

`Identify` returns `cruciblemcp.Identity{UserID, Issuer, Subject, Tenant, HandleID}`, every field optional. It is
the only way to set an identity; there is no call inside a tool handler. When it gives no subject and the request
came over streamable HTTP with a token that your `auth.TokenVerifier` accepted, the token's `UserID` is the subject
and the `iss` entry of `TokenInfo.Extra` its issuer; the `client_id` entry is sent as the OAuth client id. Crucible
hashes the subject with a per-project salt and keeps only the hash and the issuer, in the MCP operation's
`user_subject_hash` and `user_issuer`.

### Multi-round requests

In the modern era a tool can answer `input_required` with a `requestState`, and the client retries with it. The
span of each round carries keyed hashes of that state (`crucible.mcp.mrtr.state_out` on the answer,
`crucible.mcp.mrtr.state_in` on the retry), which Crucible stores with each operation; the state itself is never sent. In
the legacy era the MCP SDK completes the exchange inside one request, which is one span.

### Content capture and scrubbing

Arguments and results are not captured unless a rule opts a tool in. A `CaptureRule` has `Tool`, a glob in
`path.Match` syntax; `Args` and `Result`, which say what to capture and are false unless you set them;
`SampleRate`, the share of matching calls captured (0 means 1), decided before anything is copied; and `MaxBytes`,
the cap per field (0 means 16 KiB). The first rule whose glob matches the tool applies.

Each captured field is scrubbed with the shared rules, which replace PEM blocks, JWTs, bearer tokens, Crucible secret
keys (`csk_`), card numbers that pass the Luhn check, email addresses, IP addresses and phone numbers, each match with
a marker such as `[redacted:card]`, and then cut at `MaxBytes` on a character boundary. The rules are data:
`internal/scrub/rules.json` is a copy of `conformance/scrub-rules.json`, which every Crucible SDK uses, and a test
keeps the two identical. The SDK scrubs 1 KiB past the cap before cutting, so a secret that straddles the cap
leaves no prefix behind. `Scrub` runs after the rules and is the place for anything they do not cover, such as
values of fields named like passwords.

Scrubbing runs off the request path. While the call runs, the SDK takes only the part of each field that scrubbing
reads, a copy of at most the cap plus 1 KiB. The rules, `Scrub` and the scrubbing of error messages and handle ids
run on the SDK's export goroutine as it takes each span off the export queue, one span at a time, so a slow `Scrub`
never delays a call and capture takes at most one processor from the server. What a field holds until then counts
toward the export queue's 8 MiB: a server that captures faster than one processor scrubs fills the queue, which then
drops spans and counts them, as it does when the collector cannot keep up. `Scrub` must be safe for concurrent use.

Authorization headers, cookies, `requestState`, raw session ids, client IP addresses and baggage are never sent.

## Instrument an MCP client

```go
// client/main.go
package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

func main() {
	err := crucible.Init(crucible.Options{
		Endpoint:    os.Getenv("CRUCIBLE_ENDPOINT"),
		SecretKey:   os.Getenv("CRUCIBLE_SECRET_KEY"),
		ServiceName: "support-agent",
	})
	if err != nil {
		log.Fatal(err)
	}
	defer shutdown()

	client := mcp.NewClient(&mcp.Implementation{Name: "support-agent", Version: "1.0.0"}, nil)
	opts := cruciblemcp.ClientOptions{Transport: cruciblemcp.TransportStdio}
	if err := cruciblemcp.InstrumentClient(client, opts); err != nil {
		log.Fatal(err)
	}
	ctx := crucible.WithSession(context.Background(), "conv-42")
	// The server built from server/main.go; it inherits this process's environment.
	transport := &mcp.CommandTransport{Command: exec.Command("./orders-mcp")}
	session, err := client.Connect(ctx, transport, nil)
	if err != nil {
		log.Fatal(err)
	}
	defer session.Close()

	ctx, agent := crucible.Observe(ctx, "handle refund request", crucible.Agent)
	defer agent.End()
	order, err := session.CallTool(ctx, &mcp.CallToolParams{
		Name:      "lookup_order",
		Arguments: map[string]any{"order_id": "A-1001"},
	})
	if err != nil {
		log.Fatal(err)
	}
	refund, err := session.CallTool(ctx, &mcp.CallToolParams{
		Name:      "refund",
		Arguments: map[string]any{"order_id": "A-1001", "card": "4111 1111 1111 1111"},
	})
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println(order.Content[0].(*mcp.TextContent).Text, refund.IsError)
}

func shutdown() {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := crucible.Shutdown(ctx); err != nil {
		log.Print(err)
	}
}
```

Build the server first with `go build -o orders-mcp ./server`. `InstrumentClient(client, options)` adds sending
middleware: every request and notification the client sends becomes a CLIENT span, and its trace context
(`traceparent`, `tracestate`) is written into the request's `params._meta`. An instrumented server records its span
in the same trace, so the agent's tool call and the server's handling of it link up. The `_meta` map is replaced by
a copy rather than edited in place, but the params value itself is updated, as the MCP SDK also does, so do not
share one params value between concurrent calls. `ClientOptions.Transport` is the label, `unknown` when empty.

## The export queue

Finished spans go on a bounded queue that one goroutine exports in batches of up to 512: as soon as a batch is full,
every `FlushInterval`, and whenever `Flush` asks. When the queue holds `QueueSize` spans, or `QueueBytes` bytes of
spans with the batch being exported, a span is dropped and counted; ending a span never waits for the network.

The OpenTelemetry exporter the SDK uses retries a refused export, such as a 429 or a 503, and waits for the server's
`Retry-After`; by default it gives up after a minute. A batch that still fails is dropped and counted.

`CurrentStats()` returns the counters of the process-wide client, zero before `Init` and after `Shutdown`:

| Counter | Meaning |
|---|---|
| `Exported` | Spans the server accepted |
| `QueueFull` | Spans dropped because the export queue was full |
| `ExportFailed` | Spans dropped because their export failed |
| `QueuedBytes` | Estimated bytes of the queued spans and of the batch being exported |
| `FeedbackSent` | Scores Crucible accepted |
| `FeedbackDropped` | Scores never sent: the feedback queue was full, `ProjectID` is empty, or `Shutdown` ran out of time |
| `FeedbackFailed` | Scores Crucible refused, or that failed on every attempt |

## Configuration

The SDK reads no environment variables of its own: the endpoint, the key and every other setting come from
`crucible.Options` and `cruciblemcp.Options`, so the examples read `CRUCIBLE_ENDPOINT` and `CRUCIBLE_SECRET_KEY`
themselves. The resource starts from the OpenTelemetry Go SDK's default resource, so `OTEL_SERVICE_NAME` and
`OTEL_RESOURCE_ATTRIBUTES` still fill the resource keys that `Options` leave empty. Each export and each feedback
request carries `User-Agent: crucible-sdk-go/<version>`; `crucible.Version` is the version.

## What the middleware cannot observe

The MCP Go SDK hands middleware a method, its decoded params and a context, which leaves some fields of
[contract 9.1](../architecture/contracts.md#91-wire-attribute-contract) out of reach. These keys are not sent:

| Not observed | Why |
|---|---|
| `jsonrpc.request.id` without `Transport` or `Handler` | go-sdk v1.8.0 keeps the request id under an unexported context key |
| Errors raised before the middleware runs | An unknown method (-32601), a malformed request (-32600, -32700), params that fail to decode and invalid `_meta` are answered before any middleware; `crucible proxy` reads them on the wire ([MCP proxy](mcp-proxy.md)) |
| HTTP status and transport failures | They happen in the HTTP handler and the transport, outside the middleware |
| The resource template of `resources/read` | Only the concrete URI is visible, and a raw URI is not used as a target name |
| Client IP prefix, response size, client capabilities hash | The SDK does not expose the peer address or the encoded response, and the contract fixes no algorithm for the capabilities hash |
| Server-to-client requests, tasks, batches | Requests the server sends (elicitation, sampling, roots) are not recorded as operations; go-sdk v1.8.0 has no tasks, and batches were removed from the protocol |

## Conformance

The SDK passes the shared [SDK conformance suite](../testing/sdk-conformance.md), agent and MCP cases, in both protocol
eras over stdio and streamable HTTP, with no skipped case. `sdk/go/conformance/runner` is its runner: for an agent case
it is the agent program, and for an MCP case it serves the reference server on go-sdk, instrumented through this SDK's
public API. Its capability manifest, `sdk/go/conformance/manifest.json`, declares that a tool handler's error becomes a
tool error and that progress notifications are counted. CI runs the suite (`.github/workflows/sdk-conformance.yml`) on
every pull request that touches the SDKs or the suite. To run it as CI does, on Linux with Rust and the SDK's own
toolchain, from the repository root:

```sh
cargo build -p crucible-fake-collector
target/debug/crucible-sdk-driver run --manifest sdk/go/conformance/manifest.json
```

The SDK's own checks (`.github/workflows/ci-sdk-go.yml`) run `go test -race`, `go vet` and staticcheck. The tests
run an in-memory MCP transport in both eras, and streamable HTTP, against an OTLP collector that decodes every export
and keeps the raw bodies, so they can check that no planted secret is exported.
