package cruciblemcp_test

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/elm-os/crucible/sdk/go/cruciblemcp"
	"github.com/modelcontextprotocol/go-sdk/auth"
	"github.com/modelcontextprotocol/go-sdk/mcp"
)

type bearer struct {
	token string
	next  http.RoundTripper
}

func (b bearer) RoundTrip(r *http.Request) (*http.Response, error) {
	r = r.Clone(r.Context())
	r.Header.Set("Authorization", "Bearer "+b.token)
	return b.next.RoundTrip(r)
}

// serveHTTP serves an instrumented reference server over streamable HTTP behind a bearer-token check, and
// connects a client at version that passes the progress it receives to progress, when that is not nil. The
// modern era needs a stateless handler; the legacy one keeps sessions.
func serveHTTP(t *testing.T, opts cruciblemcp.Options, version string, progress chan<- float64) *mcp.ClientSession {
	t.Helper()
	srv := newServer(t, opts)
	verify := func(_ context.Context, token string, _ *http.Request) (*auth.TokenInfo, error) {
		if token != "user-token" {
			return nil, auth.ErrInvalidToken
		}
		return &auth.TokenInfo{
			UserID:     "sub-9",
			Expiration: time.Now().Add(time.Hour),
			Extra:      map[string]any{"iss": "https://idp.example", "client_id": "client-1", "email": "ada@example.com"},
		}, nil
	}
	hopts := &mcp.StreamableHTTPOptions{Stateless: version >= modern}
	handler := srv.in.Handler(mcp.NewStreamableHTTPHandler(func(*http.Request) *mcp.Server { return srv.Server }, hopts))
	hs := httptest.NewServer(auth.RequireBearerToken(verify, nil)(handler))
	t.Cleanup(hs.Close)
	transport := &mcp.StreamableClientTransport{
		Endpoint:   hs.URL,
		HTTPClient: &http.Client{Transport: bearer{"user-token", http.DefaultTransport}},
	}
	cs, err := newClient(t, progress).Connect(t.Context(), transport, &mcp.ClientSessionOptions{ProtocolVersion: version})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cs.Close() })
	if got := cs.InitializeResult().ProtocolVersion; got != version {
		t.Fatalf("negotiated protocol version %s, want %s", got, version)
	}
	return cs
}

// Over streamable HTTP the middleware detects the transport, reads the headers and the verified token, and in
// the legacy era hashes the session id.
func TestStreamableHTTP(t *testing.T) {
	for _, era := range eras {
		t.Run(era.name, func(t *testing.T) {
			c := startCrucible(t)
			cs := serveHTTP(t, cruciblemcp.Options{}, era.version, nil)
			if _, err := call(t, cs, "echo", map[string]any{"text": "hello"}); err != nil {
				t.Fatal(err)
			}

			s := server(t, c, "tools/call echo")
			expect(t, s, map[string]any{
				"crucible.mcp.transport":       "streamable_http",
				"crucible.mcp.era":             era.name,
				"crucible.mcp.user.subject":    "sub-9",
				"crucible.mcp.user.issuer":     "https://idp.example",
				"crucible.mcp.oauth.client_id": "client-1",
				"crucible.mcp.identity.source": "token_verified",
				"user.id":                      nil,
			})
			if ua, _ := s.Attrs["user_agent.original"].(string); ua == "" {
				t.Error("no user_agent.original")
			}
			if n, _ := s.Attrs["crucible.mcp.request.bytes"].(int64); n <= 0 {
				t.Errorf("crucible.mcp.request.bytes = %v", s.Attrs["crucible.mcp.request.bytes"])
			}

			cli := client(t, c, "tools/call echo")
			if era.version == legacy {
				if cs.ID() == "" {
					t.Fatal("no session id over stateful streamable HTTP")
				}
				sum := sha256.Sum256([]byte(cs.ID()))
				want := hex.EncodeToString(sum[:16])
				expect(t, s, map[string]any{"mcp.session.id": want})
				expect(t, cli, map[string]any{"mcp.session.id": want})
			} else {
				expect(t, s, map[string]any{"mcp.session.id": nil})
				expect(t, cli, map[string]any{"mcp.session.id": nil})
			}
			if s.ParentSpanID != cli.SpanID {
				t.Error("the server span is not a child of the client span")
			}

			// Taint: the secret key, the token's other claims and the raw session id are never exported.
			planted := []string{testKey, "ada@example.com", "user-token"}
			if cs.ID() != "" {
				planted = append(planted, cs.ID())
			}
			for _, p := range planted {
				if bytes.Contains(c.Bodies(), []byte(p)) {
					t.Errorf("the exported bodies contain %q", p)
				}
			}
		})
	}
}

func TestNoIdentityIgnoresVerifiedTokens(t *testing.T) {
	c := startCrucible(t)
	cs := serveHTTP(t, cruciblemcp.Options{NoIdentity: true}, legacy, nil)
	if _, err := call(t, cs, "echo", map[string]any{"text": "hello"}); err != nil {
		t.Fatal(err)
	}
	expect(t, server(t, c, "tools/call echo"), map[string]any{
		"crucible.mcp.user.subject":    nil,
		"crucible.mcp.user.issuer":     nil,
		"crucible.mcp.oauth.client_id": nil,
		"crucible.mcp.identity.source": nil,
	})
}

// In the modern era over HTTP a cancelled call closes its response stream, which the server records as a
// stream close; the progress notifications sent before it are counted.
func TestModernHTTPCancellationIsAStreamClose(t *testing.T) {
	c := startCrucible(t)
	srv := newServer(t, cruciblemcp.Options{})
	opts := &mcp.StreamableHTTPOptions{Stateless: true, PropagateRequestCancellation: true}
	hs := httptest.NewServer(srv.in.Handler(mcp.NewStreamableHTTPHandler(func(*http.Request) *mcp.Server { return srv.Server }, opts)))
	t.Cleanup(hs.Close)
	got := make(chan float64, 4)
	cs, err := newClient(t, got).Connect(t.Context(), &mcp.StreamableClientTransport{Endpoint: hs.URL}, &mcp.ClientSessionOptions{ProtocolVersion: modern})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cs.Close() })
	if v := cs.InitializeResult().ProtocolVersion; v != modern {
		t.Fatalf("negotiated %s", v)
	}

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
		"crucible.mcp.transport":      "streamable_http",
		"crucible.mcp.era":            "modern",
		"crucible.mcp.cancel.source":  "stream_close",
		"crucible.mcp.error.class":    "cancelled",
		"crucible.mcp.progress.count": int64(2),
	})
}
