package cruciblemcp_test

import (
	"bytes"
	"context"
	"strings"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	tracepb "go.opentelemetry.io/proto/otlp/trace/v1"
)

func TestOperationsInBothEras(t *testing.T) {
	for _, era := range eras {
		t.Run(era.name, func(t *testing.T) {
			c := startCrucible(t)
			cs := connect(t, newServer(t, cruciblemcp.Options{ServerName: "orders", ServerVersion: "2.1.0"}), newClient(t, nil), era.version)
			if _, err := call(t, cs, "echo", map[string]any{"text": "hello"}); err != nil {
				t.Fatal(err)
			}

			srv, cli := server(t, c, "tools/call echo"), client(t, c, "tools/call echo")
			expect(t, srv, map[string]any{
				"mcp.method.name":                       "tools/call",
				"gen_ai.tool.name":                      "echo",
				"mcp.protocol.version":                  era.version,
				"crucible.mcp.era":                      era.name,
				"crucible.mcp.capture_source":           "sdk",
				"crucible.mcp.transport":                "stdio",
				"crucible.mcp.client.name":              "agent",
				"crucible.mcp.client.version":           "0.9.0",
				"crucible.mcp.server.name":              "orders",
				"crucible.mcp.server.version":           "2.1.0",
				"crucible.mcp.result_type":              "complete",
				"crucible.mcp.progress.count":           int64(0),
				"crucible.mcp.content.blocks.text":      int64(1),
				"crucible.mcp.error.class":              nil,
				"error.type":                            nil,
				"crucible.mcp.is_notification":          nil,
				"crucible.mcp.content.captured":         nil,
				"gen_ai.tool.call.arguments":            nil,
				"mcp.session.id":                        nil,
				"jsonrpc.request.id":                    nil,
				"crucible.mcp.identity.source":          nil,
				"crucible.mcp.mrtr.state_out":           nil,
				"crucible.mcp.client.capabilities_hash": nil,
			})
			if srv.StatusCode != tracepb.Status_STATUS_CODE_UNSET {
				t.Errorf("server status = %v", srv.StatusCode)
			}
			if srv.Resource["service.name"] != "orders-service" || srv.Resource["crucible.deploy.id"] != "deploy-3" {
				t.Errorf("resource = %v", srv.Resource)
			}
			expect(t, cli, map[string]any{
				"mcp.method.name":             "tools/call",
				"gen_ai.tool.name":            "echo",
				"mcp.protocol.version":        era.version,
				"crucible.mcp.era":            era.name,
				"crucible.mcp.capture_source": "sdk",
				"crucible.mcp.transport":      "unknown",
				"crucible.mcp.server.name":    "orders",
				"crucible.mcp.server.version": "2.1.0",
				"crucible.mcp.result_type":    "complete",
			})
			// The in-memory transport shares no context between the two sides, so equal ids prove that the
			// trace context travelled in params._meta.
			if srv.TraceID != cli.TraceID || srv.ParentSpanID != cli.SpanID {
				t.Errorf("server span %s/%s parent %s is not a child of client span %s/%s",
					srv.TraceID, srv.SpanID, srv.ParentSpanID, cli.TraceID, cli.SpanID)
			}

			if era.version == legacy {
				expect(t, server(t, c, "initialize"), map[string]any{
					"mcp.protocol.version":     legacy,
					"crucible.mcp.era":         "legacy",
					"crucible.mcp.client.name": "agent",
					"crucible.mcp.result_type": "complete",
				})
				expect(t, server(t, c, "notifications/initialized"), map[string]any{
					"crucible.mcp.is_notification": true,
					"crucible.mcp.result_type":     nil,
					"crucible.mcp.progress.count":  nil,
				})
			} else {
				expect(t, server(t, c, "server/discover"), map[string]any{
					"mcp.protocol.version": modern,
					"crucible.mcp.era":     "modern",
				})
			}
		})
	}
}

func TestErrorTaxonomy(t *testing.T) {
	for _, era := range eras {
		t.Run(era.name, func(t *testing.T) {
			c := startCrucible(t)
			cs := connect(t, newServer(t, cruciblemcp.Options{}), newClient(t, nil), era.version)
			for _, tc := range []struct {
				tool     string
				args     map[string]any
				class    string
				errType  string
				subclass any
				code     any
			}{
				{"fail", nil, "tool_execution", "tool_error", "upstream", nil},
				{"validate", nil, "validation", "tool_error", nil, nil},
				{"lookup_order", map[string]any{"order_id": "A-9"}, "not_found", "tool_error", nil, nil},
				{"missing", nil, "not_found", "-32602", nil, "-32602"},
				{"throw", nil, "internal", "-32603", nil, "-32603"},
				{"crash", nil, "internal", "0", nil, "0"},
				{"nope", nil, "validation", "-32602", nil, "-32602"},
			} {
				_, _ = call(t, cs, tc.tool, tc.args)
				s := server(t, c, "tools/call "+tc.tool)
				expect(t, s, map[string]any{
					"crucible.mcp.error.class":    tc.class,
					"error.type":                  tc.errType,
					"crucible.mcp.error.subclass": tc.subclass,
					"rpc.response.status_code":    tc.code,
				})
				if s.StatusCode != tracepb.Status_STATUS_CODE_ERROR || s.StatusMessage == "" {
					t.Errorf("%s: status %v %q, want an error with a message", tc.tool, s.StatusCode, s.StatusMessage)
				}
				if tc.code != nil {
					expect(t, s, map[string]any{"crucible.mcp.result_type": nil})
				} else {
					expect(t, s, map[string]any{"crucible.mcp.result_type": "complete"})
				}
			}
			if s := server(t, c, "tools/call fail"); s.StatusMessage != "the warehouse service is down" {
				t.Errorf("fail: status message %q", s.StatusMessage)
			}
			// The client sees the result but not the server's annotations.
			expect(t, client(t, c, "tools/call validate"), map[string]any{
				"crucible.mcp.error.class": "tool_execution",
				"error.type":               "tool_error",
			})
			expect(t, client(t, c, "tools/call throw"), map[string]any{
				"crucible.mcp.error.class": "internal",
				"rpc.response.status_code": "-32603",
			})
		})
	}
}

func TestIdentifyHookAndHandleParam(t *testing.T) {
	for _, era := range eras {
		t.Run(era.name, func(t *testing.T) {
			c := startCrucible(t)
			opts := cruciblemcp.Options{
				Identify: func(_ context.Context, req mcp.Request) cruciblemcp.Identity {
					if _, ok := req.GetParams().(*mcp.CallToolParamsRaw); !ok {
						return cruciblemcp.Identity{}
					}
					return cruciblemcp.Identity{UserID: "user-7", Tenant: "acme", Issuer: "https://id.example", Subject: "sub-1"}
				},
				HandleParams: map[string]string{"lookup_order": "order_id"},
			}
			cs := connect(t, newServer(t, opts), newClient(t, nil), era.version)
			if _, err := call(t, cs, "lookup_order", map[string]any{"order_id": "A-1"}); err != nil {
				t.Fatal(err)
			}
			if _, err := call(t, cs, "lookup_order", map[string]any{"order_id": "ada@example.com"}); err != nil {
				t.Fatal(err)
			}
			ops := spans(t, c, tracepb.Span_SPAN_KIND_SERVER, "tools/call lookup_order", 2)
			for _, s := range ops {
				expect(t, s, map[string]any{
					"user.id":                      "user-7",
					"crucible.mcp.tenant":          "acme",
					"crucible.mcp.user.issuer":     "https://id.example",
					"crucible.mcp.user.subject":    "sub-1",
					"crucible.mcp.identity.source": "identify_hook",
					"crucible.mcp.oauth.client_id": nil,
				})
			}
			handles := []any{ops[0].Attrs["crucible.mcp.handle.id"], ops[1].Attrs["crucible.mcp.handle.id"]}
			if handles[0] != "A-1" || handles[1] != "[redacted:email]" {
				t.Errorf("handle ids = %v, want A-1 and the scrubbed email", handles)
			}
		})
	}
}

func TestNoIdentitySuppressesEveryIdentityKey(t *testing.T) {
	c := startCrucible(t)
	opts := cruciblemcp.Options{
		Identify: func(context.Context, mcp.Request) cruciblemcp.Identity {
			return cruciblemcp.Identity{UserID: "user-7", Tenant: "acme", Subject: "sub-1", HandleID: "h-1"}
		},
		NoIdentity:   true,
		HandleParams: map[string]string{"lookup_order": "order_id"},
	}
	cs := connect(t, newServer(t, opts), newClient(t, nil), modern)
	if _, err := call(t, cs, "lookup_order", map[string]any{"order_id": "A-1"}); err != nil {
		t.Fatal(err)
	}
	s := server(t, c, "tools/call lookup_order")
	expect(t, s, map[string]any{
		"user.id":                      nil,
		"crucible.mcp.tenant":          nil,
		"crucible.mcp.user.subject":    nil,
		"crucible.mcp.user.issuer":     nil,
		"crucible.mcp.identity.source": nil,
		"crucible.mcp.handle.id":       "A-1",
	})
}

func TestContentCaptureIsScrubbedAndCapped(t *testing.T) {
	c := startCrucible(t)
	opts := cruciblemcp.Options{
		Capture: []cruciblemcp.CaptureRule{
			{Tool: "secret", Args: true, Result: true},
			{Tool: "ech*", Args: true, MaxBytes: 16},
		},
		Scrub: func(field, value string) (string, bool) {
			return strings.ReplaceAll(value, "internal-codename", "[codename]"), true
		},
	}
	cs := connect(t, newServer(t, opts), newClient(t, nil), modern)
	if _, err := call(t, cs, "secret", map[string]any{"password": "hunter2", "note": "card 4111 1111 1111 1111"}); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, cs, "echo", map[string]any{"text": strings.Repeat("x", 100)}); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, cs, "lookup_order", map[string]any{"order_id": "A-1"}); err != nil {
		t.Fatal(err)
	}

	secret := server(t, c, "tools/call secret")
	expect(t, secret, map[string]any{
		"gen_ai.tool.call.arguments":       `{"note":"card [redacted:card]","password":"[redacted]"}`,
		"crucible.mcp.content.captured":    true,
		"crucible.mcp.content.sample_rate": 1.0,
		"crucible.mcp.content.truncation":  "none",
	})
	result, _ := secret.Attrs["gen_ai.tool.call.result"].(string)
	for _, want := range []string{"[redacted:email]", "[redacted:key]", "[codename]", `"type":"text"`} {
		if !strings.Contains(result, want) {
			t.Errorf("result %q does not contain %q", result, want)
		}
	}
	echo := server(t, c, "tools/call echo")
	args, _ := echo.Attrs["gen_ai.tool.call.arguments"].(string)
	if len(args) != 16 || echo.Attrs["crucible.mcp.content.truncation"] != "cap_exceeded" {
		t.Errorf("echo arguments %q (%d bytes), truncation %v", args, len(args), echo.Attrs["crucible.mcp.content.truncation"])
	}
	if _, ok := echo.Attrs["gen_ai.tool.call.result"]; ok {
		t.Error("echo result captured without opting in")
	}
	lookup := server(t, c, "tools/call lookup_order")
	if keys := hasPrefix(lookup.Attrs, "crucible.mcp.content.captured"); len(keys) != 0 || lookup.Attrs["gen_ai.tool.call.arguments"] != nil {
		t.Errorf("a tool without a capture rule has content keys: %v", lookup.Attrs)
	}

	// Taint: no planted secret reaches the collector in any form.
	bodies := c.Bodies()
	for _, planted := range []string{"hunter2", "ada@example.com", "csk_0123456789abcdefghij", "4111 1111 1111 1111", "internal-codename"} {
		if bytes.Contains(bodies, []byte(planted)) {
			t.Errorf("the exported bodies contain %q", planted)
		}
	}
}

func TestMultiRoundModernHashesTheRequestState(t *testing.T) {
	c := startCrucible(t)
	cs := connect(t, newServer(t, cruciblemcp.Options{}), newClient(t, nil), modern)
	res, err := call(t, cs, "needs_input", nil)
	if err != nil {
		t.Fatal(err)
	}
	if res.NeedsInput() {
		t.Fatal("the client did not complete the exchange")
	}
	rounds := spans(t, c, tracepb.Span_SPAN_KIND_SERVER, "tools/call needs_input", 2)
	first, second := rounds[0], rounds[1]
	if first.Attrs["crucible.mcp.result_type"] != "input_required" {
		first, second = second, first
	}
	expect(t, first, map[string]any{
		"crucible.mcp.result_type":    "input_required",
		"crucible.mcp.mrtr.state_out": mrtrHash,
		"crucible.mcp.mrtr.state_in":  nil,
	})
	expect(t, second, map[string]any{
		"crucible.mcp.result_type":    "complete",
		"crucible.mcp.mrtr.state_in":  mrtrHash,
		"crucible.mcp.mrtr.state_out": nil,
	})
	cli := client(t, c, "tools/call needs_input")
	expect(t, cli, map[string]any{"crucible.mcp.result_type": "complete"})
	for _, s := range rounds {
		if s.ParentSpanID != cli.SpanID {
			t.Errorf("round %v is not a child of the client span", s.Attrs["crucible.mcp.result_type"])
		}
	}
	if bytes.Contains(c.Bodies(), []byte(mrtrState)) {
		t.Error("the exported bodies contain the raw requestState")
	}
}

func TestMultiRoundLegacyIsOneOperation(t *testing.T) {
	c := startCrucible(t)
	cs := connect(t, newServer(t, cruciblemcp.Options{}), newClient(t, nil), legacy)
	if _, err := call(t, cs, "needs_input", nil); err != nil {
		t.Fatal(err)
	}
	expect(t, server(t, c, "tools/call needs_input"), map[string]any{
		"crucible.mcp.result_type":    "complete",
		"crucible.mcp.mrtr.state_in":  nil,
		"crucible.mcp.mrtr.state_out": nil,
	})
	if bytes.Contains(c.Bodies(), []byte(mrtrState)) {
		t.Error("the exported bodies contain the raw requestState")
	}
}

func TestProgressAndCancellation(t *testing.T) {
	for _, era := range eras {
		t.Run(era.name, func(t *testing.T) {
			c := startCrucible(t)
			got := make(chan float64, 4)
			cs := connect(t, newServer(t, cruciblemcp.Options{}), newClient(t, got), era.version)
			ctx, cancel := context.WithCancel(t.Context())
			done := make(chan error, 1)
			go func() {
				params := &mcp.CallToolParams{Name: "slow", Arguments: map[string]any{}}
				params.SetProgressToken("progress-1")
				_, err := cs.CallTool(ctx, params)
				done <- err
			}()
			for i := 0; i < 2; i++ {
				select {
				case <-got:
				case <-time.After(5 * time.Second):
					t.Fatal("no progress notification")
				}
			}
			cancel()
			if err := <-done; err == nil {
				t.Fatal("a cancelled call succeeded")
			}
			expect(t, server(t, c, "tools/call slow"), map[string]any{
				"crucible.mcp.progress.count": int64(2),
				"crucible.mcp.cancel.source":  "notification",
				"crucible.mcp.error.class":    "cancelled",
				"crucible.mcp.result_type":    nil,
			})
			expect(t, client(t, c, "tools/call slow"), map[string]any{
				"crucible.mcp.cancel.source": "notification",
				"crucible.mcp.error.class":   "cancelled",
			})
		})
	}
}

func TestInstrumentRefusesInvalidOptions(t *testing.T) {
	for _, tc := range []struct {
		opts cruciblemcp.Options
		want string
	}{
		{cruciblemcp.Options{Transport: "pipe"}, "is not one of unknown, stdio, streamable_http or sse"},
		{cruciblemcp.Options{Capture: []cruciblemcp.CaptureRule{{Tool: "[", Args: true}}}, "Capture[0].Tool \"[\" is not a valid glob"},
		{cruciblemcp.Options{Capture: []cruciblemcp.CaptureRule{{Tool: "*", SampleRate: 1.5}}}, "use 0 or 1 to capture every call"},
		{cruciblemcp.Options{Capture: []cruciblemcp.CaptureRule{{Tool: "*", MaxBytes: -1}}}, "use 0 for the default of 16 KiB"},
	} {
		err := cruciblemcp.Instrument(mcp.NewServer(&mcp.Implementation{Name: "x"}, nil), tc.opts)
		if err == nil || !strings.Contains(err.Error(), tc.want) {
			t.Errorf("Instrument(%+v) = %v, want it to say %q", tc.opts, err, tc.want)
		}
	}
	if err := cruciblemcp.InstrumentClient(mcp.NewClient(&mcp.Implementation{Name: "x"}, nil), cruciblemcp.ClientOptions{Transport: "tcp"}); err == nil {
		t.Error("InstrumentClient accepted an unknown transport")
	}
}

// Without crucible.Init the middleware records nothing and changes nothing.
func TestWithoutInitCallsStillWork(t *testing.T) {
	cs := connect(t, newServer(t, cruciblemcp.Options{}), newClient(t, nil), modern)
	res, err := call(t, cs, "echo", map[string]any{"text": "hello"})
	if err != nil {
		t.Fatal(err)
	}
	if got := res.Content[0].(*mcp.TextContent).Text; got != "hello" {
		t.Fatalf("echo returned %q", got)
	}
}
