package cruciblemcp_test

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/crucible"
	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/elm-os/crucible/sdk/go/internal/collectortest"
	"github.com/modelcontextprotocol/go-sdk/jsonrpc"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	tracepb "go.opentelemetry.io/proto/otlp/trace/v1"
)

const (
	// The key and request state of the conformance registry's multi-round hash vectors.
	testKey   = "csk_conformance00000000000000000000000000000000"
	mrtrState = "crucible-conformance-request-state-1"
	mrtrHash  = "59c50f1c773bc1354d420d974c06c5af"

	legacy = "2025-11-25"
	modern = "2026-07-28"
)

var eras = []struct{ name, version string }{{"legacy", legacy}, {"modern", modern}}

func startCrucible(t *testing.T) *collectortest.Collector {
	t.Helper()
	c := collectortest.New(t)
	err := crucible.Init(crucible.Options{
		Endpoint:       c.URL,
		SecretKey:      testKey,
		ServiceName:    "orders-service",
		ServiceVersion: "2.1.0",
		DeployID:       "deploy-3",
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = crucible.Shutdown(context.Background()) })
	return c
}

type textIn struct {
	Text string `json:"text"`
}

type orderIn struct {
	OrderID string `json:"order_id"`
}

type noteIn struct {
	Password string `json:"password"`
	Note     string `json:"note"`
}

func text(s string) []mcp.Content { return []mcp.Content{&mcp.TextContent{Text: s}} }

// newServer is the reference server: one tool per outcome the error taxonomy distinguishes, plus tools for
// content capture, progress and cancellation, and the multi-round exchange.
func newServer(t *testing.T, opts cruciblemcp.Options) *mcp.Server {
	t.Helper()
	s := mcp.NewServer(&mcp.Implementation{Name: "orders", Version: "2.1.0"}, nil)
	mcp.AddTool(s, &mcp.Tool{Name: "echo"}, func(_ context.Context, _ *mcp.CallToolRequest, in textIn) (*mcp.CallToolResult, any, error) {
		return &mcp.CallToolResult{Content: text(in.Text)}, nil, nil
	})
	mcp.AddTool(s, &mcp.Tool{Name: "fail"}, func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		cruciblemcp.AnnotateError(ctx, cruciblemcp.SubclassUpstream)
		return nil, nil, errors.New("the warehouse service is down")
	})
	mcp.AddTool(s, &mcp.Tool{Name: "validate"}, func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		cruciblemcp.MarkValidation(ctx)
		return nil, nil, errors.New("quantity must be positive")
	})
	mcp.AddTool(s, &mcp.Tool{Name: "lookup_order"}, func(ctx context.Context, _ *mcp.CallToolRequest, in orderIn) (*mcp.CallToolResult, any, error) {
		if in.OrderID != "A-1" {
			cruciblemcp.MarkNotFound(ctx)
			return nil, nil, errors.New("no such order")
		}
		return &mcp.CallToolResult{Content: text("shipped")}, nil, nil
	})
	mcp.AddTool(s, &mcp.Tool{Name: "missing"}, func(ctx context.Context, _ *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		cruciblemcp.MarkNotFound(ctx)
		return nil, nil, &jsonrpc.Error{Code: jsonrpc.CodeInvalidParams, Message: "no such record"}
	})
	mcp.AddTool(s, &mcp.Tool{Name: "throw"}, func(context.Context, *mcp.CallToolRequest, struct{}) (*mcp.CallToolResult, any, error) {
		return nil, nil, &jsonrpc.Error{Code: jsonrpc.CodeInternalError, Message: "boom"}
	})
	s.AddTool(&mcp.Tool{Name: "crash", InputSchema: json.RawMessage(`{"type":"object"}`)}, func(context.Context, *mcp.CallToolRequest) (*mcp.CallToolResult, error) {
		return nil, errors.New("database closed")
	})
	mcp.AddTool(s, &mcp.Tool{Name: "secret"}, func(context.Context, *mcp.CallToolRequest, noteIn) (*mcp.CallToolResult, any, error) {
		return &mcp.CallToolResult{Content: text("Mail ada@example.com with key csk_0123456789abcdefghijABCDEFGHIJ0123456789abc about internal-codename")}, nil, nil
	})
	mcp.AddTool(s, &mcp.Tool{Name: "slow"}, func(ctx context.Context, req *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		if tok := req.Params.GetProgressToken(); tok != nil {
			for i := 1; i <= 2; i++ {
				_ = req.Session.NotifyProgress(ctx, &mcp.ProgressNotificationParams{ProgressToken: tok, Progress: float64(i), Total: 3})
			}
		}
		<-ctx.Done()
		return nil, nil, ctx.Err()
	})
	mcp.AddTool(s, &mcp.Tool{Name: "needs_input"}, func(_ context.Context, req *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
		if len(req.Params.InputResponses) == 0 {
			return &mcp.CallToolResult{
				InputRequests: mcp.InputRequestMap{"confirm": &mcp.ElicitParams{Message: "Ship the order?"}},
				RequestState:  mrtrState,
			}, nil, nil
		}
		return &mcp.CallToolResult{Content: text("confirmed")}, nil, nil
	})
	if err := cruciblemcp.Instrument(s, opts); err != nil {
		t.Fatal(err)
	}
	return s
}

// newClient returns an instrumented client that accepts every elicitation and passes the progress it receives
// to progress, when that is not nil.
func newClient(t *testing.T, progress chan<- float64) *mcp.Client {
	t.Helper()
	c := mcp.NewClient(&mcp.Implementation{Name: "agent", Version: "0.9.0"}, &mcp.ClientOptions{
		ElicitationHandler: func(context.Context, *mcp.ElicitRequest) (*mcp.ElicitResult, error) {
			return &mcp.ElicitResult{Action: "accept", Content: map[string]any{}}, nil
		},
		ProgressNotificationHandler: func(_ context.Context, req *mcp.ProgressNotificationClientRequest) {
			if progress != nil {
				progress <- req.Params.Progress
			}
		},
	})
	if err := cruciblemcp.InstrumentClient(c, cruciblemcp.ClientOptions{}); err != nil {
		t.Fatal(err)
	}
	return c
}

// connect joins the server and the client over the in-memory transport at the given protocol version, and
// checks that the client negotiated that version rather than falling back.
func connect(t *testing.T, s *mcp.Server, c *mcp.Client, version string) *mcp.ClientSession {
	t.Helper()
	st, ct := mcp.NewInMemoryTransports()
	ss, err := s.Connect(t.Context(), st, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ss.Close() })
	cs, err := c.Connect(t.Context(), ct, &mcp.ClientSessionOptions{ProtocolVersion: version})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cs.Close() })
	if got := cs.InitializeResult().ProtocolVersion; got != version {
		t.Fatalf("negotiated protocol version %s, want %s", got, version)
	}
	return cs
}

func call(t *testing.T, cs *mcp.ClientSession, name string, args map[string]any) (*mcp.CallToolResult, error) {
	t.Helper()
	if args == nil {
		args = map[string]any{}
	}
	ctx, cancel := context.WithTimeout(t.Context(), 10*time.Second)
	defer cancel()
	return cs.CallTool(ctx, &mcp.CallToolParams{Name: name, Arguments: args})
}

// spans flushes and returns the spans of one kind named name, waiting up to five seconds for n of them: a
// server span ends when its handler returns, which for a cancelled call is after the client gave up.
func spans(t *testing.T, c *collectortest.Collector, kind tracepb.Span_SpanKind, name string, n int) []collectortest.Span {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		if err := crucible.Flush(t.Context()); err != nil {
			t.Fatal(err)
		}
		var out []collectortest.Span
		for _, s := range c.Spans() {
			if s.Kind == kind && s.Name == name {
				out = append(out, s)
			}
		}
		if len(out) >= n || time.Now().After(deadline) {
			if len(out) != n {
				t.Fatalf("%d %v spans named %q, want %d", len(out), kind, name, n)
			}
			return out
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func server(t *testing.T, c *collectortest.Collector, name string) collectortest.Span {
	t.Helper()
	return spans(t, c, tracepb.Span_SPAN_KIND_SERVER, name, 1)[0]
}

func client(t *testing.T, c *collectortest.Collector, name string) collectortest.Span {
	t.Helper()
	return spans(t, c, tracepb.Span_SPAN_KIND_CLIENT, name, 1)[0]
}

// expect checks attributes: a nil want means the key must be absent.
func expect(t *testing.T, s collectortest.Span, want map[string]any) {
	t.Helper()
	for k, v := range want {
		got, ok := s.Attrs[k]
		switch {
		case v == nil && ok:
			t.Errorf("%s: %s = %#v, want it absent", s.Name, k, got)
		case v != nil && got != v:
			t.Errorf("%s: %s = %#v, want %#v", s.Name, k, got, v)
		}
	}
}

func hasPrefix(attrs map[string]any, prefix string) []string {
	var keys []string
	for k := range attrs {
		if strings.HasPrefix(k, prefix) {
			keys = append(keys, k)
		}
	}
	return keys
}
