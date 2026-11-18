package cruciblemcp_test

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/modelcontextprotocol/go-sdk/mcp"
	tracepb "go.opentelemetry.io/proto/otlp/trace/v1"
)

// rawClient writes JSON-RPC lines to a server the way the conformance runner does, so a test chooses the ids.
type rawClient struct {
	t         *testing.T
	w         *io.PipeWriter
	responses chan map[string]any
}

func rawConnect(t *testing.T, srv refServer) *rawClient {
	t.Helper()
	toServer, fromClient := io.Pipe()
	toClient, fromServer := io.Pipe()
	ss, err := srv.Connect(t.Context(), srv.in.Transport(&mcp.IOTransport{Reader: toServer, Writer: fromServer}), nil)
	if err != nil {
		t.Fatal(err)
	}
	c := &rawClient{t: t, w: fromClient, responses: make(chan map[string]any, 16)}
	go func() {
		sc := bufio.NewScanner(toClient)
		sc.Buffer(nil, 1<<20)
		for sc.Scan() {
			var m map[string]any
			if json.Unmarshal(sc.Bytes(), &m) == nil && (m["result"] != nil || m["error"] != nil) {
				c.responses <- m
			}
		}
	}()
	t.Cleanup(func() {
		_ = fromClient.Close()
		_ = ss.Close()
		_ = toClient.Close()
	})
	return c
}

func (c *rawClient) send(msg string) {
	c.t.Helper()
	if _, err := io.WriteString(c.w, msg+"\n"); err != nil {
		c.t.Fatal(err)
	}
}

func (c *rawClient) await(id string) map[string]any {
	c.t.Helper()
	for {
		select {
		case m := <-c.responses:
			if fmt.Sprint(m["id"]) == id {
				return m
			}
		case <-time.After(5 * time.Second):
			c.t.Fatalf("no response to %s", id)
		}
	}
}

const rawMeta = `"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",` +
	`"io.modelcontextprotocol/clientInfo":{"name":"raw","version":"1.0.0"},"io.modelcontextprotocol/clientCapabilities":{}}`

// Over a wrapped stdio-style transport, each request's span carries the id the client chose, and a
// notifications/cancelled message, which the MCP SDK handles before any middleware, becomes an operation.
func TestWrappedTransportIdsAndCancellation(t *testing.T) {
	for _, era := range eras {
		t.Run(era.name, func(t *testing.T) {
			c := startCrucible(t)
			client := rawConnect(t, newServer(t, cruciblemcp.Options{}))
			meta := ""
			if era.version == modern {
				meta = rawMeta + ","
			} else {
				client.send(`{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"raw","version":"1.0.0"}}}`)
				client.await("0")
				client.send(`{"jsonrpc":"2.0","method":"notifications/initialized"}`)
			}
			client.send(`{"jsonrpc":"2.0","id":"call-7","method":"tools/call","params":{` + meta + `"name":"echo","arguments":{"text":"x"}}}`)
			client.await("call-7")
			client.send(`{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{` + meta + `"name":"slow","arguments":{}}}`)
			time.Sleep(100 * time.Millisecond)
			client.send(`{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9,"reason":"test"}}`)

			expect(t, server(t, c, "tools/call echo"), map[string]any{
				"jsonrpc.request.id":   "call-7",
				"crucible.mcp.era":     era.name,
				"mcp.protocol.version": era.version,
			})
			expect(t, server(t, c, "tools/call slow"), map[string]any{
				"jsonrpc.request.id":         "9",
				"crucible.mcp.cancel.source": "notification",
			})
			cancel := server(t, c, "notifications/cancelled")
			expect(t, cancel, map[string]any{
				"mcp.method.name":               "notifications/cancelled",
				"crucible.mcp.is_notification":  true,
				"crucible.mcp.capture_source":   "sdk",
				"crucible.mcp.transport":        "stdio",
				"crucible.mcp.era":              era.name,
				"mcp.protocol.version":          era.version,
				"jsonrpc.request.id":            nil,
				"crucible.mcp.cancel.source":    nil,
				"crucible.mcp.client.ip_prefix": nil,
			})
			if cancel.ParentSpanID != "" {
				t.Errorf("the cancellation span has a parent %s", cancel.ParentSpanID)
			}
			if era.version == legacy {
				expect(t, server(t, c, "initialize"), map[string]any{"jsonrpc.request.id": "0"})
				expect(t, server(t, c, "notifications/initialized"), map[string]any{"jsonrpc.request.id": nil})
			}
		})
	}
}

// The wrapped handler passes the body on unchanged, so the MCP SDK still refuses one over its limit.
func TestWrappedHandlerKeepsTheBodyLimit(t *testing.T) {
	srv := newServer(t, cruciblemcp.Options{})
	h := srv.in.Handler(mcp.NewStreamableHTTPHandler(func(*http.Request) *mcp.Server { return srv.Server }, &mcp.StreamableHTTPOptions{Stateless: true}))
	hs := httptest.NewServer(h)
	t.Cleanup(hs.Close)
	body := `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo","arguments":{"text":"` +
		strings.Repeat("x", mcp.DefaultMaxRequestBodyBytes) + `"}}}`
	req, err := http.NewRequestWithContext(t.Context(), http.MethodPost, hs.URL, strings.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json, text/event-stream")
	req.Header.Set("Mcp-Protocol-Version", modern)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	_ = resp.Body.Close()
	if resp.StatusCode != http.StatusRequestEntityTooLarge {
		t.Fatalf("status %d, want 413", resp.StatusCode)
	}
}

// Over legacy streamable HTTP the wrapped handler records the client's cancellation with the session's hash.
func TestWrappedHandlerRecordsCancellation(t *testing.T) {
	c := startCrucible(t)
	got := make(chan float64, 4)
	cs := serveHTTP(t, cruciblemcp.Options{}, legacy, got)
	ctx, cancel := context.WithCancel(t.Context())
	done := make(chan error, 1)
	go func() {
		params := &mcp.CallToolParams{Name: "slow", Arguments: map[string]any{}}
		params.SetProgressToken("progress-1")
		_, err := cs.CallTool(ctx, params)
		done <- err
	}()
	select {
	case <-got:
	case <-time.After(5 * time.Second):
		t.Fatal("the call did not start")
	}
	cancel()
	if err := <-done; err == nil {
		t.Fatal("a cancelled call succeeded")
	}
	slow := server(t, c, "tools/call slow")
	if id, _ := slow.Attrs["jsonrpc.request.id"].(string); id == "" || strings.Trim(id, "0123456789") != "" {
		t.Errorf("jsonrpc.request.id = %#v", slow.Attrs["jsonrpc.request.id"])
	}
	expect(t, slow, map[string]any{"crucible.mcp.cancel.source": "notification"})
	n := server(t, c, "notifications/cancelled")
	expect(t, n, map[string]any{
		"crucible.mcp.is_notification": true,
		"crucible.mcp.transport":       "streamable_http",
		"crucible.mcp.era":             "legacy",
		"mcp.protocol.version":         legacy,
		"mcp.session.id":               slow.Attrs["mcp.session.id"],
	})
	if n.Attrs["mcp.session.id"] == nil {
		t.Error("the cancellation span has no session key")
	}
	if n.Kind != tracepb.Span_SPAN_KIND_SERVER {
		t.Errorf("kind = %v", n.Kind)
	}
}

// Without Transport or Handler the middleware records the same operation, without its JSON-RPC id.
func TestUnwrappedTransportHasNoRequestID(t *testing.T) {
	c := startCrucible(t)
	srv := newServer(t, cruciblemcp.Options{})
	st, ct := mcp.NewInMemoryTransports()
	ss, err := srv.Connect(t.Context(), st, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ss.Close() })
	cs, err := newClient(t, nil).Connect(t.Context(), ct, &mcp.ClientSessionOptions{ProtocolVersion: modern})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cs.Close() })
	if _, err := call(t, cs, "echo", map[string]any{"text": "hello"}); err != nil {
		t.Fatal(err)
	}
	expect(t, server(t, c, "tools/call echo"), map[string]any{
		"mcp.method.name":          "tools/call",
		"crucible.mcp.era":         "modern",
		"crucible.mcp.result_type": "complete",
		"jsonrpc.request.id":       nil,
	})
}
